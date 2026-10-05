//! The command worker: edits from the UI go to the engine in order, on their
//! own thread, so the window never waits on the engine. Gain drags are merged:
//! at most one `SetPoint` per point per [`GAIN_INTERVAL`], the newest value winning.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use confluence_api::{BusRef, Command, DeviceKind, Response};
use confluence_client::Client;

/// Gain changes to one point are sent at most this often.
pub const GAIN_INTERVAL: Duration = Duration::from_millis(30);
/// How long a connection attempt waits for the engine's pipe.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// How long one edit may take before the engine counts as not responding.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Opening a device can legitimately take seconds (a slow driver).
const ADD_DEVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// A change the user asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    SetPoint { input: u32, output: u32, gain_db: f32, mute: bool, invert: bool },
    RemovePoint { input: u32, output: u32 },
    AddDevice { kind: DeviceKind, name: String },
    RemoveSlot { id: u32 },
    AddBus { name: String, channels: u32 },
    LoadPlugin { bus: u32, path: String, plugin_id: String },
    UnloadPlugin { bus: u32 },
    SetParam { bus: u32, param: u32, value: f64 },
    ShowEditor { bus: u32 },
    HideEditor { bus: u32 },
    SaveScene { name: String, morph_ms: u32 },
    RecallScene { name: String },
    DeleteScene { name: String },
    SetSceneMorph { name: String, morph_ms: u32 },
    LearnMidi { input: u32, output: u32 },
    CancelMidiLearn,
    RemoveMidiBinding { device: String, channel: u8, cc: u8 },
}

/// What a merged, rate-limited edit is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MergeKey {
    Point(u32, u32),
    Param(u32, u32),
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
            Edit::LoadPlugin { bus, path, plugin_id } => {
                Command::LoadPlugin { bus: BusRef::Id(*bus), path: path.clone(), plugin_id: plugin_id.clone() }
            }
            Edit::UnloadPlugin { bus } => Command::UnloadPlugin { bus: BusRef::Id(*bus) },
            Edit::ShowEditor { bus } => Command::ShowEditor { bus: BusRef::Id(*bus) },
            Edit::HideEditor { bus } => Command::HideEditor { bus: BusRef::Id(*bus) },
            Edit::SaveScene { name, morph_ms } => Command::SaveScene { name: name.clone(), morph_ms: *morph_ms },
            Edit::RecallScene { name } => Command::RecallScene { name: name.clone() },
            Edit::LearnMidi { input, output } => Command::LearnMidi { input: *input, output: *output },
            Edit::CancelMidiLearn => Command::CancelMidiLearn,
            Edit::RemoveMidiBinding { device, channel, cc } => {
                Command::RemoveMidiBinding { device: device.clone(), channel: *channel, cc: *cc }
            }
            Edit::DeleteScene { name } => Command::DeleteScene { name: name.clone() },
            Edit::SetSceneMorph { name, morph_ms } => {
                Command::SetSceneMorph { name: name.clone(), morph_ms: *morph_ms }
            }
            Edit::SetParam { bus, param, value } => {
                Command::SetParam { bus: BusRef::Id(*bus), param: *param, value: *value }
            }
            Edit::AddBus { name, channels } => {
                Command::AddBus { name: name.clone(), channels: *channels, first_input: None, first_output: None }
            }
        }
    }

    /// The matrix point this edit changes, if any.
    pub fn point(&self) -> Option<(u32, u32)> {
        match self {
            Edit::SetPoint { input, output, .. } | Edit::RemovePoint { input, output } => Some((*input, *output)),
            _ => None,
        }
    }

    /// Opening a device or loading a plugin can take seconds: it gets its own
    /// connection and thread.
    fn is_slow(&self) -> bool {
        matches!(self, Edit::AddDevice { .. } | Edit::LoadPlugin { .. })
    }

    /// Gains and parameter values: merged while queued and rate limited.
    fn merge_key(&self) -> Option<MergeKey> {
        match *self {
            Edit::SetPoint { input, output, .. } => Some(MergeKey::Point(input, output)),
            Edit::SetParam { bus, param, .. } => Some(MergeKey::Param(bus, param)),
            _ => None,
        }
    }
}

/// What became of an edit.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Applied; `version` is the engine state version after it, `ids` the new slots of an `AddDevice` or `AddBus`.
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
    last_gain: HashMap<MergeKey, Instant>,
}

impl Outbox {
    /// Queues `edit`. A gain (`SetPoint`) or parameter value (`SetParam`)
    /// replaces the queued one for the same point or parameter, if that is the
    /// latest queued edit about it.
    pub fn push(&mut self, edit: Edit) {
        if let Some(k) = edit.merge_key() {
            let about = |e: &Edit| match k {
                MergeKey::Point(i, o) => e.point() == Some((i, o)),
                MergeKey::Param(..) => e.merge_key() == Some(k),
            };
            if let Some(last) = self.queue.iter_mut().rev().find(|e| about(e)) {
                if last.merge_key() == Some(k) {
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

    /// Takes every queued edit, in order.
    pub fn drain(&mut self) -> Vec<Edit> {
        self.queue.drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The next edit if it may go now. Edits leave strictly in order; a gain
    /// for a point sent less than [`GAIN_INTERVAL`] ago holds the queue.
    pub fn next_ready(&mut self, now: Instant) -> Option<Edit> {
        self.last_gain.retain(|_, t| now.saturating_duration_since(*t) < GAIN_INTERVAL);
        let front = self.queue.front()?;
        if let Some(k) = front.merge_key() {
            if self.last_gain.contains_key(&k) {
                return None;
            }
            self.last_gain.insert(k, now);
        }
        self.queue.pop_front()
    }

    /// How long until the front edit may go; `None` when nothing is queued.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let front = self.queue.front()?;
        Some(match front.merge_key().and_then(|k| self.last_gain.get(&k)) {
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
        Self::spawn_with(pipe, wake, CALL_TIMEOUT)
    }

    /// As [`Worker::spawn`], with a given limit on how long one call may take.
    pub fn spawn_with(pipe: String, wake: Arc<dyn Fn() + Send + Sync>, call_timeout: Duration) -> Worker {
        let (edits, edit_rx) = channel::<Edit>();
        let (out_tx, outcomes) = channel::<Outcome>();
        let _ = std::thread::Builder::new()
            .name("confluence-commands".into())
            .spawn(move || run(&pipe, &edit_rx, &out_tx, &wake, call_timeout));
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

/// Why a call produced no reply.
enum Failure {
    /// The connection failed or could not be made.
    Lost(String),
    /// No reply within the time limit: the engine is not responding.
    Hung,
}

const NOT_RESPONDING: &str = "the engine is not responding";

/// A connection on its own thread, so a call that never returns can be given
/// up on: the thread is abandoned and ends when its call finally returns.
struct Caller {
    requests: Sender<Command>,
    replies: Receiver<Result<Response, String>>,
}

impl Caller {
    fn connect(pipe: &str) -> Result<Caller, String> {
        let mut client =
            Client::connect(pipe, CONNECT_TIMEOUT).map_err(|e| format!("not connected to the engine ({e})"))?;
        let (requests, request_rx) = channel::<Command>();
        let (reply_tx, replies) = channel();
        std::thread::Builder::new()
            .name("confluence-call".into())
            .spawn(move || {
                for cmd in request_rx {
                    let reply = client.call(cmd).map_err(|e| format!("lost the engine ({e})"));
                    if reply_tx.send(reply).is_err() {
                        return; // given up on
                    }
                }
            })
            .map_err(|e| format!("cannot start a connection thread ({e})"))?;
        Ok(Caller { requests, replies })
    }

    fn call(&self, cmd: Command, timeout: Duration) -> Result<Response, Failure> {
        if self.requests.send(cmd).is_err() {
            return Err(Failure::Lost("lost the engine".into()));
        }
        match self.replies.recv_timeout(timeout) {
            Ok(reply) => reply.map_err(Failure::Lost),
            Err(RecvTimeoutError::Timeout) => Err(Failure::Hung),
            Err(RecvTimeoutError::Disconnected) => Err(Failure::Lost("lost the engine".into())),
        }
    }
}

fn run(pipe: &str, edits: &Receiver<Edit>, out: &Sender<Outcome>, wake: &Wake, timeout: Duration) {
    let mut outbox = Outbox::default();
    let mut caller: Option<Caller> = None;
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
            let (outcome, hung) = send(&mut caller, pipe, edit, timeout);
            let _ = out.send(outcome);
            if hung {
                // Everything waiting behind the stuck call fails too, rather
                // than each waiting out its own timeout.
                for edit in outbox.drain().into_iter().chain(edits.try_iter()) {
                    let _ = out.send(Outcome::Failed { edit, reason: NOT_RESPONDING.into() });
                }
            }
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
        let mut caller = None;
        let (outcome, _) = send(&mut caller, &pipe, edit, ADD_DEVICE_TIMEOUT);
        let _ = out.send(outcome);
        wake();
    });
}

/// One call, connecting first if needed; a failed call drops the connection.
fn call(caller: &mut Option<Caller>, pipe: &str, edit: &Edit, timeout: Duration) -> Result<Response, Failure> {
    let c = match caller {
        Some(c) => c,
        None => caller.insert(Caller::connect(pipe).map_err(Failure::Lost)?),
    };
    let result = c.call(edit.command(), timeout);
    if result.is_err() {
        *caller = None;
    }
    result
}

/// Sends one edit; the flag is true when the engine did not answer in time.
fn send(caller: &mut Option<Caller>, pipe: &str, edit: Edit, timeout: Duration) -> (Outcome, bool) {
    let reused = caller.is_some();
    let mut result = call(caller, pipe, &edit, timeout);
    if reused && matches!(result, Err(Failure::Lost(_))) {
        // The connection may be from before an engine restart: try once more
        // on a fresh one. Every edit sent this way is safe to repeat.
        result = call(caller, pipe, &edit, timeout);
    }
    let outcome = match result {
        Ok(Response::Applied { version }) => Outcome::Done { edit, version: Some(version), ids: Vec::new() },
        Ok(Response::Added { ids, version }) => Outcome::Done { edit, version: Some(version), ids },
        Ok(Response::Error(reason)) => Outcome::Failed { edit, reason },
        Ok(_) => Outcome::Done { edit, version: None, ids: Vec::new() },
        Err(Failure::Lost(reason)) => Outcome::Failed { edit, reason },
        Err(Failure::Hung) => return (Outcome::Failed { edit, reason: NOT_RESPONDING.into() }, true),
    };
    (outcome, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_edits_merge_and_are_rate_limited_per_parameter() {
        let param = |bus, param, value| Edit::SetParam { bus, param, value };
        let mut o = Outbox::default();
        o.push(param(1, 1, -1.0));
        o.push(param(1, 1, -2.0)); // replaces the queued one
        o.push(param(1, 2, 0.5)); // another parameter: kept
        assert_eq!(o.len(), 2);
        let t = Instant::now();
        assert_eq!(o.next_ready(t), Some(param(1, 1, -2.0)));
        assert_eq!(o.next_ready(t), Some(param(1, 2, 0.5)));
        o.push(param(1, 1, -3.0));
        assert_eq!(o.next_ready(t), None, "the same parameter again waits out the interval");
        assert_eq!(o.next_ready(t + GAIN_INTERVAL), Some(param(1, 1, -3.0)));
        assert_eq!(
            param(4, 1, -6.0).command(),
            Command::SetParam { bus: confluence_api::BusRef::Id(4), param: 1, value: -6.0 }
        );
    }

    #[test]
    fn loading_a_plugin_is_slow_and_unloading_is_not() {
        let load = Edit::LoadPlugin { bus: 2, path: "x.clap".into(), plugin_id: "dev.x".into() };
        assert!(load.is_slow());
        assert_eq!(
            load.command(),
            Command::LoadPlugin {
                bus: confluence_api::BusRef::Id(2),
                path: "x.clap".into(),
                plugin_id: "dev.x".into()
            }
        );
        let show = Edit::ShowEditor { bus: 2 };
        assert!(!show.is_slow());
        assert_eq!(show.command(), Command::ShowEditor { bus: confluence_api::BusRef::Id(2) });
        assert_eq!(Edit::HideEditor { bus: 2 }.command(), Command::HideEditor { bus: confluence_api::BusRef::Id(2) });
        let unload = Edit::UnloadPlugin { bus: 2 };
        assert!(!unload.is_slow());
        assert_eq!(unload.command(), Command::UnloadPlugin { bus: confluence_api::BusRef::Id(2) });
    }

    #[test]
    fn add_bus_is_a_plain_edit() {
        let e = Edit::AddBus { name: "Verb".into(), channels: 2 };
        assert_eq!(
            e.command(),
            Command::AddBus { name: "Verb".into(), channels: 2, first_input: None, first_output: None }
        );
        assert_eq!(e.point(), None);
        assert!(!e.is_slow(), "a bus opens no device: the normal timeout applies");
    }

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

    /// Collects outcomes until `n` arrived, failing after `limit`.
    fn outcomes(worker: &Worker, n: usize, limit: Duration) -> Vec<Outcome> {
        let start = Instant::now();
        let mut got = Vec::new();
        while got.len() < n {
            got.extend(worker.outcomes());
            assert!(start.elapsed() < limit, "only {} of {n} outcomes after {limit:?}: {got:?}", got.len());
            std::thread::sleep(Duration::from_millis(10));
        }
        got
    }

    /// An engine that stops answering must not block edits forever: the stuck
    /// edit and everything queued behind it fail, and the next edit gets a
    /// fresh connection.
    #[test]
    fn a_hung_engine_fails_edits_instead_of_blocking_them() {
        use confluence_engine::ipc::{service_fn, PipeServer};
        use std::sync::atomic::{AtomicBool, Ordering};
        let name = format!("confluence-hung-{}", std::process::id());
        let first = Arc::new(AtomicBool::new(true));
        let server = PipeServer::start(&name, {
            let first = first.clone();
            service_fn(move |_| {
                if first.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(5)); // hung
                }
                Response::Applied { version: 1 }
            })
        })
        .unwrap();
        let worker = Worker::spawn_with(name, Arc::new(|| {}), Duration::from_millis(300));
        worker.send(gain(1, 1, -6.0));
        std::thread::sleep(Duration::from_millis(50));
        worker.send(Edit::RemovePoint { input: 2, output: 2 }); // queued behind the hung call
        let got = outcomes(&worker, 2, Duration::from_secs(2));
        for o in &got {
            assert!(matches!(o, Outcome::Failed { reason, .. } if reason.contains("not responding")), "{got:?}");
        }
        worker.send(Edit::RemovePoint { input: 3, output: 3 });
        let next = outcomes(&worker, 1, Duration::from_secs(2));
        assert!(matches!(next[0], Outcome::Done { .. }), "the next edit is not stuck: {next:?}");
        server.stop();
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
