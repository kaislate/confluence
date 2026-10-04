//! The command worker: edits from the UI go to the engine in order, on their
//! own thread, so the window never waits on the engine. Gain drags are merged:
//! at most one `SetPoint` per point per [`GAIN_INTERVAL`], the newest value winning.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use confluence_api::{Command, DeviceKind, Response};
use confluence_client::Client;

/// Gain changes to one point are sent at most this often.
pub const GAIN_INTERVAL: Duration = Duration::from_millis(30);
/// How long a connection attempt waits for the engine's pipe.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// A change the user asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    SetPoint { input: u32, output: u32, gain_db: f32, mute: bool, invert: bool },
    RemovePoint { input: u32, output: u32 },
    AddDevice { kind: DeviceKind, name: String },
    RemoveSlot { id: u32 },
}

impl Edit {
    pub fn command(&self) -> Command {
        match self {
            Edit::SetPoint { input, output, gain_db, mute, invert } => {
                Command::SetPoint { input: *input, output: *output, gain_db: *gain_db, mute: *mute, invert: *invert }
            }
            Edit::RemovePoint { input, output } => Command::RemovePoint { input: *input, output: *output },
            Edit::AddDevice { kind, name } => Command::AddDevice { kind: *kind, name: name.clone() },
            Edit::RemoveSlot { id } => Command::RemoveSlot { id: *id },
        }
    }

    /// The matrix point this edit changes, if any.
    pub fn point(&self) -> Option<(u32, u32)> {
        match self {
            Edit::SetPoint { input, output, .. } | Edit::RemovePoint { input, output } => Some((*input, *output)),
            _ => None,
        }
    }

    /// Opening a device can take seconds: it gets its own connection and thread.
    fn is_slow(&self) -> bool {
        matches!(self, Edit::AddDevice { .. })
    }
}

/// What became of an edit.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Applied; `version` is the engine state version after it, `ids` the new slots of an `AddDevice`.
    Done {
        edit: Edit,
        version: Option<u64>,
        ids: Vec<u32>,
    },
    Failed {
        edit: Edit,
        reason: String,
    },
}

/// Edits waiting to be sent, in order, with gain merging and rate limiting.
#[derive(Default)]
pub struct Outbox {
    queue: VecDeque<Edit>,
    last_gain: HashMap<(u32, u32), Instant>,
}

impl Outbox {
    /// Queues `edit`. A `SetPoint` replaces the queued `SetPoint` for the same
    /// point, if that is the latest queued edit for the point.
    pub fn push(&mut self, edit: Edit) {
        if let (Edit::SetPoint { .. }, Some(p)) = (&edit, edit.point()) {
            if let Some(last) = self.queue.iter_mut().rev().find(|e| e.point() == Some(p)) {
                if matches!(last, Edit::SetPoint { .. }) {
                    *last = edit;
                    return;
                }
            }
        }
        self.queue.push_back(edit);
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The next edit if it may go now. Edits leave strictly in order; a gain
    /// for a point sent less than [`GAIN_INTERVAL`] ago holds the queue.
    pub fn next_ready(&mut self, now: Instant) -> Option<Edit> {
        self.last_gain.retain(|_, t| now.saturating_duration_since(*t) < GAIN_INTERVAL);
        let front = self.queue.front()?;
        if let (Edit::SetPoint { .. }, Some(p)) = (front, front.point()) {
            if self.last_gain.contains_key(&p) {
                return None;
            }
            self.last_gain.insert(p, now);
        }
        self.queue.pop_front()
    }

    /// How long until the front edit may go; `None` when nothing is queued.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let front = self.queue.front()?;
        let gain_point = match front {
            Edit::SetPoint { .. } => front.point(),
            _ => None,
        };
        Some(match gain_point.and_then(|p| self.last_gain.get(&p)) {
            Some(t) => GAIN_INTERVAL.saturating_sub(now.saturating_duration_since(*t)),
            None => Duration::ZERO,
        })
    }
}

/// The UI's handle on the worker thread.
pub struct Worker {
    edits: Sender<Edit>,
    outcomes: Receiver<Outcome>,
}

impl Worker {
    /// Starts the worker; `wake` runs after every outcome (the UI repaints).
    pub fn spawn(pipe: String, wake: Arc<dyn Fn() + Send + Sync>) -> Worker {
        let (edits, edit_rx) = channel::<Edit>();
        let (out_tx, outcomes) = channel::<Outcome>();
        let _ = std::thread::Builder::new()
            .name("confluence-commands".into())
            .spawn(move || run(&pipe, &edit_rx, &out_tx, &wake));
        Worker { edits, outcomes }
    }

    pub fn send(&self, edit: Edit) {
        let _ = self.edits.send(edit);
    }

    /// Outcomes that arrived since the last call.
    pub fn outcomes(&self) -> Vec<Outcome> {
        self.outcomes.try_iter().collect()
    }
}

type Wake = Arc<dyn Fn() + Send + Sync>;

fn run(pipe: &str, edits: &Receiver<Edit>, out: &Sender<Outcome>, wake: &Wake) {
    let mut outbox = Outbox::default();
    let mut client: Option<Client> = None;
    loop {
        let received = match outbox.wait(Instant::now()) {
            None => edits.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(d) => edits.recv_timeout(d),
        };
        match received {
            Ok(edit) => queue(&mut outbox, edit, pipe, out, wake),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if outbox.is_empty() {
                    return; // the UI is gone and nothing is left to send
                }
                std::thread::sleep(outbox.wait(Instant::now()).unwrap_or_default());
            }
        }
        for edit in edits.try_iter() {
            queue(&mut outbox, edit, pipe, out, wake);
        }
        while let Some(edit) = outbox.next_ready(Instant::now()) {
            let _ = out.send(send(&mut client, pipe, edit));
            wake();
        }
    }
}

fn queue(outbox: &mut Outbox, edit: Edit, pipe: &str, out: &Sender<Outcome>, wake: &Wake) {
    if !edit.is_slow() {
        outbox.push(edit);
        return;
    }
    let (pipe, out, wake) = (pipe.to_string(), out.clone(), wake.clone());
    let _ = std::thread::Builder::new().name("confluence-add-device".into()).spawn(move || {
        let mut client = None;
        let _ = out.send(send(&mut client, &pipe, edit));
        wake();
    });
}

fn send(client: &mut Option<Client>, pipe: &str, edit: Edit) -> Outcome {
    let c = match client {
        Some(c) => c,
        None => match Client::connect(pipe, CONNECT_TIMEOUT) {
            Ok(c) => client.insert(c),
            Err(e) => return Outcome::Failed { edit, reason: format!("not connected to the engine ({e})") },
        },
    };
    match c.call(edit.command()) {
        Ok(Response::Applied { version }) => Outcome::Done { edit, version: Some(version), ids: Vec::new() },
        Ok(Response::Added { ids, version }) => Outcome::Done { edit, version: Some(version), ids },
        Ok(Response::Error(reason)) => Outcome::Failed { edit, reason },
        Ok(_) => Outcome::Done { edit, version: None, ids: Vec::new() },
        Err(e) => {
            *client = None;
            Outcome::Failed { edit, reason: format!("lost the engine ({e})") }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain(input: u32, output: u32, gain_db: f32) -> Edit {
        Edit::SetPoint { input, output, gain_db, mute: false, invert: false }
    }

    #[test]
    fn gain_changes_to_one_point_merge_into_the_newest() {
        let mut o = Outbox::default();
        o.push(gain(1, 1, -6.0));
        o.push(gain(1, 1, -3.0));
        assert_eq!(o.len(), 1);
        assert_eq!(o.next_ready(Instant::now()), Some(gain(1, 1, -3.0)));
    }

    #[test]
    fn different_points_do_not_merge() {
        let mut o = Outbox::default();
        o.push(gain(1, 1, -6.0));
        o.push(gain(1, 2, -3.0));
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn a_gain_never_overtakes_a_removal_of_the_same_point() {
        let mut o = Outbox::default();
        let now = Instant::now();
        o.push(gain(1, 1, 0.0));
        o.push(Edit::RemovePoint { input: 1, output: 1 });
        o.push(gain(1, 1, -3.0));
        assert_eq!(o.len(), 3, "the last gain is not merged across the removal");
        assert_eq!(o.next_ready(now), Some(gain(1, 1, 0.0)));
        assert_eq!(o.next_ready(now), Some(Edit::RemovePoint { input: 1, output: 1 }));
        assert_eq!(o.next_ready(now + GAIN_INTERVAL), Some(gain(1, 1, -3.0)));
    }

    #[test]
    fn a_point_gets_at_most_one_gain_per_interval() {
        let mut o = Outbox::default();
        let t0 = Instant::now();
        o.push(gain(1, 1, -6.0));
        assert!(o.next_ready(t0).is_some());
        o.push(gain(1, 1, -5.0));
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(o.next_ready(t1), None, "too soon");
        assert_eq!(o.wait(t1), Some(Duration::from_millis(20)));
        assert_eq!(o.next_ready(t0 + GAIN_INTERVAL), Some(gain(1, 1, -5.0)));
        assert_eq!(o.wait(t0 + GAIN_INTERVAL), None, "nothing left");
    }

    #[test]
    fn other_edits_are_not_rate_limited() {
        let mut o = Outbox::default();
        let now = Instant::now();
        o.push(Edit::RemovePoint { input: 1, output: 1 });
        o.push(Edit::RemovePoint { input: 2, output: 2 });
        assert_eq!(o.wait(now), Some(Duration::ZERO));
        assert!(o.next_ready(now).is_some());
        assert!(o.next_ready(now).is_some());
    }

    #[test]
    fn edits_become_their_commands() {
        assert_eq!(
            gain(3, 4, -1.5).command(),
            Command::SetPoint { input: 3, output: 4, gain_db: -1.5, mute: false, invert: false }
        );
        assert_eq!(Edit::RemoveSlot { id: 7 }.command(), Command::RemoveSlot { id: 7 });
        assert_eq!(gain(3, 4, 0.0).point(), Some((3, 4)));
        assert_eq!(Edit::RemoveSlot { id: 7 }.point(), None);
    }

    #[test]
    fn without_an_engine_edits_fail_with_a_reason() {
        let worker = Worker::spawn(format!("confluence-no-engine-cmd-{}", std::process::id()), Arc::new(|| {}));
        worker.send(Edit::RemovePoint { input: 0, output: 0 });
        let start = Instant::now();
        let outcome = loop {
            if let Some(o) = worker.outcomes().pop() {
                break o;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no outcome");
            std::thread::sleep(Duration::from_millis(20));
        };
        match outcome {
            Outcome::Failed { reason, .. } => assert!(reason.contains("not connected"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
}
