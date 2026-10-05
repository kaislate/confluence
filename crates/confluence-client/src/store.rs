//! A live local copy of the engine's state for front ends. `Store` is the pure
//! core (snapshot + events + health history); `StateStore` runs it on a thread
//! that subscribes, applies, and reconnects with a fresh snapshot after any
//! error or version gap.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_api::{Change, EngineStatus, Event, SlotHealth, State};

use crate::Subscription;

/// Telemetry samples kept per slot: 60 s at 10 Hz.
pub const HISTORY_LEN: usize = 600;

/// First and longest wait between reconnect attempts.
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// A subscription that lasted this long was healthy: the backoff starts over.
const HEALTHY: Duration = Duration::from_secs(10);

/// The wait before each (re)subscription: doubling while subscriptions keep
/// ending soon (no engine, or repeated gaps), starting over after a healthy one.
#[derive(Debug)]
struct Backoff {
    wait: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { wait: BACKOFF_MIN }
    }
}

impl Backoff {
    /// The wait to use now; the next one is longer.
    fn next(&mut self) -> Duration {
        let wait = self.wait;
        self.wait = (self.wait * 2).min(BACKOFF_MAX);
        wait
    }

    /// A subscription ended after `lasted`.
    fn ended_after(&mut self, lasted: Duration) {
        if lasted >= HEALTHY {
            self.wait = BACKOFF_MIN;
        }
    }
}

/// What a store update changed, for a front end deciding when to redraw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    /// The state or the connection changed: show it now.
    State,
    /// Only telemetry (status, health, history): it may be drawn less often.
    Telemetry,
}

/// How a subscription ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SessionEnd {
    /// A version gap: resubscribing for a fresh snapshot; still connected.
    Gap,
    /// The connection is gone (or could not be made).
    Lost,
}
/// How long one connection attempt waits for the pipe.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub enum ConnState {
    /// No engine reached yet.
    Connecting,
    Live,
    /// A live connection was lost.
    Reconnecting {
        since: Instant,
    },
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

/// Per slot, oldest first; `None` is a gap (disconnected).
pub type History = BTreeMap<u32, VecDeque<Option<HealthSample>>>;

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
    /// When anything (snapshot or event) last arrived from the engine.
    pub last_event: Option<Instant>,
    /// Snapshots taken so far: a new one replaced the state wholesale.
    pub snapshots: u64,
}

/// A versioned event that did not follow the last version.
#[derive(Debug)]
pub struct Gap;

/// The pure core of [`StateStore`].
pub struct Store {
    view: StoreView,
    /// Telemetry history, outside the view: grown in place under its lock,
    /// read one slot at a time (see [`Store::history_of`]).
    history: Arc<Mutex<History>>,
    gap_pending: bool,
    ever_live: bool,
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
                last_event: None,
                snapshots: 0,
            },
            history: Arc::default(),
            gap_pending: false,
            ever_live: false,
        }
    }

    /// A (re)subscription's snapshot: replaces the state entirely and drops
    /// the history of slots it no longer has.
    pub fn snapshot(&mut self, s: State, now: Instant) {
        let ids: BTreeSet<u32> = s.slots.iter().map(|slot| slot.id).collect();
        self.with_history(|h| h.retain(|id, _| ids.contains(id)));
        self.view.status = Some(s.status.clone());
        self.view.state = Some(s);
        self.view.conn = ConnState::Live;
        self.view.last_event = Some(now);
        self.view.snapshots += 1;
        self.ever_live = true;
    }

    pub fn event(&mut self, e: Event, now: Instant) -> Result<(), Gap> {
        match e {
            Event::Changed { version, changes } => {
                let state = self.view.state.as_mut().ok_or(Gap)?;
                if version != state.version + 1 {
                    return Err(Gap);
                }
                state.apply(&changes);
                state.version = version;
                let removed: Vec<u32> = changes
                    .iter()
                    .filter_map(|c| match c {
                        Change::SlotRemoved { id } => Some(*id),
                        _ => None,
                    })
                    .collect();
                if !removed.is_empty() {
                    self.with_history(|h| {
                        for id in &removed {
                            h.remove(id);
                        }
                    });
                }
            }
            Event::Telemetry { status, health } => {
                let gap_pending = self.gap_pending;
                self.with_history(|history| {
                    for h in &health {
                        let ring = history.entry(h.id).or_default();
                        if gap_pending {
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
                });
                self.gap_pending = false;
                self.view.status = Some(status);
                self.view.health = health;
            }
        }
        self.view.last_event = Some(now);
        Ok(())
    }

    /// A subscription ended. A gap keeps the view `Live` (a fresh snapshot
    /// follows at once); a lost connection is a disconnect.
    pub fn ended(&mut self, how: SessionEnd, now: Instant) {
        if how == SessionEnd::Lost {
            self.disconnected(now);
        }
    }

    /// The connection ended (or an attempt failed). The state stays, stale,
    /// until the next snapshot. Before any engine was reached this stays
    /// `Connecting`.
    pub fn disconnected(&mut self, now: Instant) {
        if !self.ever_live {
            return;
        }
        if !matches!(self.view.conn, ConnState::Reconnecting { .. }) {
            self.view.conn = ConnState::Reconnecting { since: now };
        }
        self.gap_pending = true;
    }

    pub fn view(&self) -> StoreView {
        self.view.clone()
    }

    /// One slot's telemetry history, oldest first (`None` entries are gaps).
    pub fn history_of(&self, id: u32) -> Option<VecDeque<Option<HealthSample>>> {
        read_history(&self.history, id)
    }

    /// The shared history, for a reader on another thread.
    pub fn history_handle(&self) -> Arc<Mutex<History>> {
        self.history.clone()
    }

    fn with_history(&self, f: impl FnOnce(&mut History)) {
        let mut h = self.history.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut h);
    }
}

fn read_history(history: &Mutex<History>, id: u32) -> Option<VecDeque<Option<HealthSample>>> {
    history.lock().unwrap_or_else(|p| p.into_inner()).get(&id).cloned()
}

/// [`Store`] on a thread that keeps it connected to the engine. Dropping it
/// stops the thread: within one connection attempt and wait (at most about
/// 2.5 s) while it is reconnecting, otherwise at
/// the next event (telemetry arrives every 100 ms from a live engine).
pub struct StateStore {
    shared: Arc<Mutex<Arc<StoreView>>>,
    history: Arc<Mutex<History>>,
    stop: Arc<AtomicBool>,
}

impl StateStore {
    /// Starts following the engine on `pipe`; `on_change` runs after every update.
    pub fn spawn(pipe: String, on_change: Box<dyn Fn(Update) + Send + Sync>) -> StateStore {
        let shared = Arc::new(Mutex::new(Arc::new(Store::new().view())));
        let stop = Arc::new(AtomicBool::new(false));
        let store = Store::new();
        let history = store.history_handle();
        let (out, stopping) = (shared.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("confluence-state-store".into()).spawn(move || {
            let mut store = store;
            let mut backoff = Backoff::default();
            let update = |store: &Store, what: Update| {
                let view = Arc::new(store.view()); // built before taking the lock
                if let Ok(mut v) = out.lock() {
                    *v = view;
                }
                on_change(what);
            };
            while !stopping.load(Ordering::SeqCst) {
                let mut how = SessionEnd::Lost;
                if let Ok((snapshot, mut sub)) = Subscription::connect(&pipe, CONNECT_TIMEOUT) {
                    let began = Instant::now();
                    store.snapshot(snapshot, began);
                    update(&store, Update::State);
                    while let Ok(e) = sub.recv() {
                        if stopping.load(Ordering::SeqCst) {
                            return;
                        }
                        let what = match e {
                            Event::Telemetry { .. } => Update::Telemetry,
                            Event::Changed { .. } => Update::State,
                        };
                        if store.event(e, Instant::now()).is_err() {
                            how = SessionEnd::Gap; // resubscribe for a fresh snapshot
                            break;
                        }
                        update(&store, what);
                    }
                    backoff.ended_after(began.elapsed());
                }
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                store.ended(how, Instant::now());
                update(&store, Update::State);
                std::thread::sleep(backoff.next());
            }
        });
        StateStore { shared, history, stop }
    }

    /// One slot's telemetry history, oldest first (`None` entries are gaps).
    pub fn history_of(&self, id: u32) -> Option<VecDeque<Option<HealthSample>>> {
        read_history(&self.history, id)
    }

    /// A consistent view for one UI frame.
    pub fn view(&self) -> Arc<StoreView> {
        match self.shared.lock() {
            Ok(v) => v.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{Change, ClockRole, PointState, SlotState};

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

    fn slot(id: u32) -> SlotState {
        SlotState {
            id,
            name: format!("slot {id}"),
            device: String::new(),
            role: ClockRole::Soft,
            online: true,
            first_input: 0,
            inputs: 2,
            first_output: 0,
            outputs: 2,
        }
    }

    fn state(version: u64, slots: &[u32]) -> State {
        State {
            version,
            status: status(),
            slots: slots.iter().map(|&id| slot(id)).collect(),
            points: Vec::new(),
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
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

    fn telemetry(ids: &[u32], fill: f64) -> Event {
        Event::Telemetry { status: status(), health: ids.iter().map(|&id| health(id, fill)).collect() }
    }

    #[test]
    fn events_apply_on_top_of_the_snapshot() {
        let mut s = Store::new();
        let now = Instant::now();
        assert!(matches!(s.view().conn, ConnState::Connecting));
        s.snapshot(state(4, &[]), now);
        s.event(Event::Changed { version: 5, changes: vec![set(1)] }, now).unwrap();
        let v = s.view();
        assert!(matches!(v.conn, ConnState::Live));
        assert_eq!(v.state.as_ref().unwrap().version, 5);
        assert_eq!(v.state.unwrap().points.len(), 1);
    }

    #[test]
    fn a_version_gap_is_reported() {
        let mut s = Store::new();
        s.snapshot(state(4, &[]), Instant::now());
        assert!(s.event(Event::Changed { version: 6, changes: vec![set(1)] }, Instant::now()).is_err());
    }

    #[test]
    fn telemetry_builds_bounded_history_with_gaps_for_disconnects() {
        let mut s = Store::new();
        let now = Instant::now();
        s.snapshot(state(0, &[1]), now);
        for i in 0..(HISTORY_LEN + 5) {
            s.event(telemetry(&[1], i as f64), now).unwrap();
        }
        assert_eq!(s.history_of(1).unwrap().len(), HISTORY_LEN);
        s.disconnected(now);
        assert!(matches!(s.view().conn, ConnState::Reconnecting { .. }));
        s.snapshot(state(0, &[1]), now);
        s.event(telemetry(&[1], 7.0), now).unwrap();
        let h = s.history_of(1).unwrap();
        assert_eq!(h.len(), HISTORY_LEN, "still bounded with the gap");
        assert!(h.iter().rev().nth(1).unwrap().is_none(), "a gap marks the disconnect");
        assert_eq!(h.back().unwrap().as_ref().unwrap().fill, 7.0);
    }

    #[test]
    fn a_fresh_snapshot_replaces_stale_state() {
        let mut s = Store::new();
        let now = Instant::now();
        s.snapshot(state(0, &[]), now);
        s.event(Event::Changed { version: 1, changes: vec![set(9)] }, now).unwrap();
        s.disconnected(now);
        s.snapshot(state(0, &[]), now); // a restarted engine starts again at 0
        assert!(s.view().state.unwrap().points.is_empty(), "no stale point survives");
    }

    #[test]
    fn the_wait_doubles_until_a_subscription_stays_healthy() {
        let mut b = Backoff::default();
        let waits: Vec<u128> = (0..7).map(|_| b.next().as_millis()).collect();
        assert_eq!(waits, vec![100, 200, 400, 800, 1600, 2000, 2000]);
        b.ended_after(Duration::from_secs(1));
        assert_eq!(b.next().as_millis(), 2000, "a short-lived subscription (a gap loop) keeps backing off");
        b.ended_after(HEALTHY);
        assert_eq!(b.next().as_millis(), 100, "a long healthy subscription starts over");
    }

    #[test]
    fn a_version_gap_resyncs_without_looking_disconnected() {
        let mut s = Store::new();
        let now = Instant::now();
        s.snapshot(state(4, &[]), now);
        s.ended(SessionEnd::Gap, now);
        assert!(matches!(s.view().conn, ConnState::Live), "still live: a fresh snapshot is on its way");
        s.ended(SessionEnd::Lost, now);
        assert!(matches!(s.view().conn, ConnState::Reconnecting { .. }));
    }

    #[test]
    fn snapshots_are_counted() {
        let mut s = Store::new();
        assert_eq!(s.view().snapshots, 0);
        s.snapshot(state(0, &[]), Instant::now());
        s.disconnected(Instant::now());
        s.snapshot(state(0, &[]), Instant::now());
        assert_eq!(s.view().snapshots, 2);
    }

    #[test]
    fn it_stays_connecting_until_the_first_snapshot() {
        let mut s = Store::new();
        let now = Instant::now();
        s.disconnected(now);
        s.disconnected(now);
        assert!(matches!(s.view().conn, ConnState::Connecting), "never reached an engine");
        s.snapshot(state(0, &[]), now);
        s.disconnected(now);
        assert!(matches!(s.view().conn, ConnState::Reconnecting { .. }));
    }

    #[test]
    fn last_event_tracks_snapshots_and_events() {
        let mut s = Store::new();
        assert!(s.view().last_event.is_none());
        let t0 = Instant::now();
        s.snapshot(state(0, &[1]), t0);
        assert_eq!(s.view().last_event, Some(t0));
        let t1 = t0 + Duration::from_millis(100);
        s.event(telemetry(&[1], 1.0), t1).unwrap();
        assert_eq!(s.view().last_event, Some(t1));
    }

    #[test]
    fn removed_slots_lose_their_history() {
        let mut s = Store::new();
        let now = Instant::now();
        s.snapshot(state(0, &[1, 2]), now);
        s.event(telemetry(&[1, 2], 1.0), now).unwrap();
        s.event(Event::Changed { version: 1, changes: vec![Change::SlotRemoved { id: 2 }] }, now).unwrap();
        assert!(s.history_of(2).is_none(), "removed by the event");
        s.disconnected(now);
        s.snapshot(state(0, &[]), now);
        assert!(s.history_of(1).is_none(), "pruned by a snapshot without the slot");
    }

    /// The history is shared and grown in place: telemetry never copies it,
    /// and readers take one slot's ring when they need it.
    #[test]
    fn telemetry_grows_the_shared_history_in_place() {
        let mut s = Store::new();
        let now = Instant::now();
        let shared = s.history_handle();
        s.snapshot(state(0, &[1]), now);
        s.event(telemetry(&[1], 1.0), now).unwrap();
        s.event(telemetry(&[1], 2.0), now).unwrap();
        assert_eq!(shared.lock().unwrap()[&1].len(), 2, "the same history, grown in place");
        assert_eq!(s.history_of(1).unwrap().back().unwrap().as_ref().unwrap().fill, 2.0);
        assert!(s.history_of(9).is_none());
    }

    #[test]
    fn dropping_the_store_stops_its_thread() {
        let guard = Arc::new(());
        let held = guard.clone();
        let store = StateStore::spawn(
            format!("confluence-no-engine-{}", std::process::id()),
            Box::new(move |_| {
                let _ = &held;
            }),
        );
        std::thread::sleep(Duration::from_millis(700));
        assert!(matches!(store.view().conn, ConnState::Connecting), "no engine was ever reached");
        drop(store);
        let start = Instant::now();
        while Arc::strong_count(&guard) > 1 {
            assert!(start.elapsed() < Duration::from_secs(5), "the store's thread is still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
