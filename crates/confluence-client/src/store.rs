//! A live local copy of the engine's state for front ends. `Store` is the pure
//! core (snapshot + events + health history); `StateStore` runs it on a thread
//! that subscribes, applies, and reconnects with a fresh snapshot after any
//! error or version gap.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_api::{EngineStatus, Event, SlotHealth, State};

use crate::Subscription;

/// Telemetry samples kept per slot: 60 s at 10 Hz.
pub const HISTORY_LEN: usize = 600;

/// First and longest wait between reconnect attempts.
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub enum ConnState {
    Connecting,
    Live,
    Reconnecting { since: Instant },
}

/// One slot's telemetry at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct HealthSample {
    pub fill: f64,
    pub target: f64,
    pub ppm: f64,
    pub correction: f64,
    pub underruns: u64,
    pub overruns: u64,
}

/// Everything a front end draws from, consistent for one frame.
#[derive(Clone, Debug)]
pub struct StoreView {
    /// `None` until the first snapshot.
    pub state: Option<State>,
    pub conn: ConnState,
    /// The latest status (from the snapshot or telemetry).
    pub status: Option<EngineStatus>,
    /// The latest per-slot health.
    pub health: Vec<SlotHealth>,
    /// Per slot, oldest first; `None` is a gap (disconnected).
    pub history: BTreeMap<u32, VecDeque<Option<HealthSample>>>,
}

/// A versioned event that did not follow the last version.
#[derive(Debug)]
pub struct Gap;

/// The pure core of [`StateStore`].
pub struct Store {
    view: StoreView,
    gap_pending: bool,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Store {
            view: StoreView {
                state: None,
                conn: ConnState::Connecting,
                status: None,
                health: Vec::new(),
                history: BTreeMap::new(),
            },
            gap_pending: false,
        }
    }

    /// A (re)subscription's snapshot: replaces the state entirely.
    pub fn snapshot(&mut self, s: State) {
        self.view.status = Some(s.status.clone());
        self.view.state = Some(s);
        self.view.conn = ConnState::Live;
    }

    pub fn event(&mut self, e: Event) -> Result<(), Gap> {
        match e {
            Event::Changed { version, changes } => {
                let state = self.view.state.as_mut().ok_or(Gap)?;
                if version != state.version + 1 {
                    return Err(Gap);
                }
                state.apply(&changes);
                state.version = version;
            }
            Event::Telemetry { status, health } => {
                for h in &health {
                    let ring = self.view.history.entry(h.id).or_default();
                    if self.gap_pending {
                        ring.push_back(None);
                    }
                    ring.push_back(Some(HealthSample {
                        fill: h.fill_frames,
                        target: h.target_frames,
                        ppm: h.device_ppm,
                        correction: h.correction_ppm,
                        underruns: h.underruns,
                        overruns: h.overruns,
                    }));
                    while ring.len() > HISTORY_LEN {
                        ring.pop_front();
                    }
                }
                self.gap_pending = false;
                self.view.status = Some(status);
                self.view.health = health;
            }
        }
        Ok(())
    }

    /// The connection ended; the state stays (stale) until the next snapshot.
    pub fn disconnected(&mut self, now: Instant) {
        if !matches!(self.view.conn, ConnState::Reconnecting { .. }) {
            self.view.conn = ConnState::Reconnecting { since: now };
        }
        self.gap_pending = true;
    }

    pub fn view(&self) -> StoreView {
        self.view.clone()
    }
}

/// [`Store`] on a thread that keeps it connected to the engine. The thread
/// runs for the rest of the process (a front end keeps one store).
pub struct StateStore {
    shared: Arc<Mutex<Arc<StoreView>>>,
}

impl StateStore {
    /// Starts following the engine on `pipe`; `on_change` runs after every update.
    pub fn spawn(pipe: String, on_change: Box<dyn Fn() + Send + Sync>) -> StateStore {
        let shared = Arc::new(Mutex::new(Arc::new(Store::new().view())));
        let out = shared.clone();
        let _ = std::thread::Builder::new().name("confluence-state-store".into()).spawn(move || {
            let mut store = Store::new();
            let mut backoff = BACKOFF_MIN;
            let update = |store: &Store| {
                if let Ok(mut v) = out.lock() {
                    *v = Arc::new(store.view());
                }
                on_change();
            };
            loop {
                match Subscription::connect(&pipe, Duration::from_millis(500)) {
                    Ok((snapshot, mut sub)) => {
                        backoff = BACKOFF_MIN;
                        store.snapshot(snapshot);
                        update(&store);
                        while let Ok(e) = sub.recv() {
                            if store.event(e).is_err() {
                                break; // gap: resubscribe for a fresh snapshot
                            }
                            update(&store);
                        }
                    }
                    Err(_) => std::thread::sleep(backoff),
                }
                store.disconnected(Instant::now());
                update(&store);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        });
        StateStore { shared }
    }

    /// A consistent view for one UI frame.
    pub fn view(&self) -> Arc<StoreView> {
        match self.shared.lock() {
            Ok(v) => v.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{Change, PointState};

    fn status() -> EngineStatus {
        EngineStatus {
            master: "internal".into(),
            sample_rate: 48_000.0,
            block: 256,
            blocks: 1,
            dsp_load: 0.1,
            xruns: 0,
        }
    }

    fn state(version: u64) -> State {
        State {
            version,
            status: status(),
            slots: Vec::new(),
            points: Vec::new(),
            devices: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn health(id: u32, fill: f64) -> SlotHealth {
        SlotHealth {
            id,
            underruns: 0,
            overruns: 0,
            fill_frames: fill,
            target_frames: 600.0,
            device_ppm: 1.0,
            correction_ppm: 0.0,
            device_lost: false,
            device_faults: 0,
            driver_requests: 0,
            attached: None,
            idle_note: None,
        }
    }

    fn set(i: u32) -> Change {
        Change::PointSet(PointState { input: i, output: 0, gain_db: 0.0, mute: false, invert: false })
    }

    #[test]
    fn events_apply_on_top_of_the_snapshot() {
        let mut s = Store::new();
        assert!(matches!(s.view().conn, ConnState::Connecting));
        s.snapshot(state(4));
        s.event(Event::Changed { version: 5, changes: vec![set(1)] }).unwrap();
        let v = s.view();
        assert!(matches!(v.conn, ConnState::Live));
        assert_eq!(v.state.as_ref().unwrap().version, 5);
        assert_eq!(v.state.unwrap().points.len(), 1);
    }

    #[test]
    fn a_version_gap_is_reported() {
        let mut s = Store::new();
        s.snapshot(state(4));
        assert!(s.event(Event::Changed { version: 6, changes: vec![set(1)] }).is_err());
    }

    #[test]
    fn telemetry_builds_bounded_history_with_gaps_for_disconnects() {
        let mut s = Store::new();
        s.snapshot(state(0));
        for i in 0..(HISTORY_LEN + 5) {
            s.event(Event::Telemetry { status: status(), health: vec![health(1, i as f64)] }).unwrap();
        }
        assert_eq!(s.view().history[&1].len(), HISTORY_LEN);
        s.disconnected(Instant::now());
        assert!(matches!(s.view().conn, ConnState::Reconnecting { .. }));
        s.snapshot(state(0));
        s.event(Event::Telemetry { status: status(), health: vec![health(1, 7.0)] }).unwrap();
        let v = s.view();
        let h = &v.history[&1];
        assert_eq!(h.len(), HISTORY_LEN, "still bounded with the gap");
        assert!(h.iter().rev().nth(1).unwrap().is_none(), "a gap marks the disconnect");
        assert_eq!(h.back().unwrap().as_ref().unwrap().fill, 7.0);
    }

    #[test]
    fn a_fresh_snapshot_replaces_stale_state() {
        let mut s = Store::new();
        s.snapshot(state(0));
        s.event(Event::Changed { version: 1, changes: vec![set(9)] }).unwrap();
        s.disconnected(Instant::now());
        s.snapshot(state(0)); // a restarted engine starts again at 0
        assert!(s.view().state.unwrap().points.is_empty(), "no stale point survives");
    }
}
