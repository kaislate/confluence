//! Versioned state publishing: the engine's published state is diffed after
//! every change and every 100 ms; each non-empty diff is one versioned event
//! for every subscriber. A subscriber whose queue is full is dropped: the
//! engine never waits for a client.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use confluence_api::{diff, Command, DeviceInfo, EngineStatus, Event, Response, SlotHealth, State};

use crate::devices::DeviceManager;
use crate::Engine;

/// Events queued per subscriber before it counts as stalled and is dropped.
pub const SUBSCRIBER_QUEUE: usize = 256;

pub struct Publisher {
    last: State,
    version: u64,
    subscribers: Vec<SyncSender<Event>>,
}

impl Publisher {
    pub fn new(initial: State) -> Self {
        Publisher { last: State { version: 0, ..initial }, version: 0, subscribers: Vec::new() }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Publishes `now`: returns the version after it (unchanged if nothing changed).
    pub fn publish(&mut self, now: State) -> u64 {
        let changes = diff(&self.last, &now);
        if !changes.is_empty() {
            self.version += 1;
            self.send(&Event::Changed { version: self.version, changes });
        }
        self.last = State { version: self.version, ..now };
        self.version
    }

    /// The current state and a queue that receives every event after it.
    pub fn subscribe(&mut self) -> (State, Receiver<Event>) {
        let (tx, rx) = sync_channel(SUBSCRIBER_QUEUE);
        self.subscribers.push(tx);
        (self.last.clone(), rx)
    }

    /// Ends every subscription (at shutdown).
    pub fn close(&mut self) {
        self.subscribers.clear();
    }

    pub fn telemetry(&mut self, status: EngineStatus, health: Vec<SlotHealth>) {
        self.send(&Event::Telemetry { status, health });
    }

    /// Sends to every subscriber; a full queue or a vanished receiver drops it.
    fn send(&mut self, event: &Event) {
        self.subscribers.retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}

/// The engine's current published state (version 0; the publisher sets it).
pub fn published_state(
    engine: &mut Engine,
    devices: &DeviceManager,
    device_list: &[DeviceInfo],
    status: EngineStatus,
) -> State {
    let mut slots = engine.slots();
    slots.sort_by_key(|s| s.id);
    let mut points = match engine.handle(&Command::ListPoints) {
        Response::Points(p) => p,
        _ => Vec::new(),
    };
    points.sort_by_key(|p| (p.input, p.output));
    State {
        version: 0,
        status,
        slots,
        points,
        devices: device_list.to_vec(),
        notices: devices.notices(),
        plugins: Vec::new(),
        bad_plugins: Vec::new(),
        bus_plugins: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{PointState, State};

    fn status() -> EngineStatus {
        EngineStatus {
            master: "internal".into(),
            sample_rate: 48_000.0,
            block: 256,
            blocks: 0,
            dsp_load: 0.0,
            xruns: 0,
        }
    }

    fn state(points: Vec<(u32, u32, f32)>) -> State {
        State {
            version: 0,
            status: status(),
            slots: Vec::new(),
            points: points
                .into_iter()
                .map(|(input, output, gain_db)| PointState { input, output, gain_db, mute: false, invert: false })
                .collect(),
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
        }
    }

    #[test]
    fn a_subscriber_gets_the_snapshot_then_contiguous_versions() {
        let mut p = Publisher::new(state(vec![]));
        let (snap, rx) = p.subscribe();
        assert_eq!(snap.version, 0);
        assert_eq!(p.publish(state(vec![(0, 0, 0.0)])), 1);
        assert_eq!(p.publish(state(vec![(0, 0, -6.0), (1, 1, 0.0)])), 2);
        let versions: Vec<u64> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::Changed { version, .. } => Some(version),
                Event::Telemetry { .. } => None,
            })
            .collect();
        assert_eq!(versions, vec![1, 2]);
    }

    #[test]
    fn one_publish_is_one_event_however_many_changes() {
        let mut p = Publisher::new(state(vec![]));
        let (_, rx) = p.subscribe();
        p.publish(state(vec![(0, 0, 0.0), (1, 1, 0.0), (2, 2, 0.0)]));
        let events: Vec<Event> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Changed { version: 1, changes } if changes.len() == 3));
    }

    #[test]
    fn a_no_op_command_publishes_nothing() {
        let mut p = Publisher::new(state(vec![(0, 0, 0.0)]));
        let (_, rx) = p.subscribe();
        assert_eq!(p.publish(state(vec![(0, 0, 0.0)])), 0, "nothing changed: same version");
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn a_late_subscriber_snapshot_includes_everything_so_far() {
        let mut p = Publisher::new(state(vec![]));
        p.publish(state(vec![(3, 4, -1.0)]));
        let (snap, _rx) = p.subscribe();
        assert_eq!(snap.version, 1);
        assert_eq!(snap.points.len(), 1);
    }

    #[test]
    fn a_full_queue_drops_only_that_subscriber() {
        let mut p = Publisher::new(state(vec![]));
        let (_, stalled) = p.subscribe(); // never read
        let (_, live) = p.subscribe();
        for i in 0..(SUBSCRIBER_QUEUE as u32 + 10) {
            p.publish(state(vec![(i, 0, 0.0)]));
            while live.try_recv().is_ok() {}
        }
        assert_eq!(p.subscriber_count(), 1, "the stalled one was dropped");
        drop(stalled);
        p.publish(state(vec![]));
        assert!(live.try_recv().is_ok(), "the live one still gets events");
    }

    #[test]
    fn closing_ends_every_subscription() {
        let mut p = Publisher::new(state(vec![]));
        let (_, rx) = p.subscribe();
        p.close();
        assert_eq!(p.subscriber_count(), 0);
        assert!(rx.recv().is_err(), "the stream ended");
    }

    #[test]
    fn vanished_subscribers_are_removed() {
        let mut p = Publisher::new(state(vec![]));
        for _ in 0..20 {
            let (_, rx) = p.subscribe();
            drop(rx);
        }
        let (_, keep) = p.subscribe();
        p.telemetry(status(), Vec::new());
        assert_eq!(p.subscriber_count(), 1);
        assert!(matches!(keep.try_recv(), Ok(Event::Telemetry { .. })));
    }
}
