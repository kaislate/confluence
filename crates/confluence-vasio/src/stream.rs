//! The driver's stream thread: it drives the DAW's `bufferSwitch`.
//!
//! - **Linked:** each engine block wakes the thread, which hands the DAW the
//!   engine's audio, calls `bufferSwitch`, and returns the DAW's output.
//! - **Engine absent or stalled:** the thread paces itself on a timer and
//!   hands the DAW silence, so the DAW keeps running. It retries the engine
//!   every half second and relinks by itself.
//! - **Engine shape changed** (rate, block or channels): the DAW is asked to
//!   reset (`kAsioResetRequest`) so it re-initialises with the new shape.
//! - **Another DAW already on this instance:** a stream has one reader, so the
//!   first DAW keeps it (its id is claimed in the header) and later ones run
//!   on silence until it stops, or until its heartbeat goes stale (a crashed
//!   or hung DAW), when the next one takes over.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use confluence_provider_asio::sys::*;
use confluence_provider_vasio::config::InstanceConfig;
use confluence_provider_vasio::stream_name;
use confluence_rt::{now_seconds, ProAudioThread};
use confluence_shm::ring::{RingReader, RingWriter};
use confluence_shm::Client;

/// A server heartbeat frozen this long means the engine is stalled or gone.
const STALL_S: f64 = 0.5;
/// How often the prober looks for the engine while unlinked.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// How long a driver that did not own an instance's previous stream waits
/// before claiming a fresh one, so the previous owner gets it back first.
const CLAIM_GRACE_S: f64 = 0.5;
/// Consecutive engine wake-ups missed before pacing falls back to the timer.
const MISSED_WAKES_TO_STALL: u32 = 2;

/// A process-unique, non-zero id this driver claims a stream with.
fn new_client_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    (std::process::id().wrapping_mul(0x9E37_79B1) ^ NEXT.fetch_add(1, Ordering::Relaxed)) | 1
}

/// Another driver's claim on a stream, watched to see whether its heartbeat moves.
#[derive(Clone, Copy)]
struct Watch {
    generation: u64,
    heartbeat: u64,
    since: f64,
}

/// The DAW-visible sample position and the time of the latest `bufferSwitch`.
#[derive(Debug, Default)]
pub(crate) struct Position {
    pub samples: AtomicI64,
    /// On the SDK's `timeGetTime()` basis, in nanoseconds.
    pub nanos: AtomicI64,
    /// True while the stream thread is delivering blocks.
    pub running: AtomicBool,
}

/// Nanoseconds on the `timeGetTime()` basis the ASIO SDK specifies for
/// `systemTime`, with QueryPerformanceCounter resolution between anchors.
struct SystemClock {
    anchor_ns: i64,
    anchor_s: f64,
}

impl SystemClock {
    /// Anchors on a `timeGetTime()` tick edge: a single sample could be up to
    /// a whole tick (15.6 ms at the default timer resolution) stale.
    fn new() -> Self {
        // SAFETY (both calls): plain queries.
        let first = unsafe { windows::Win32::Media::timeGetTime() };
        let give_up = now_seconds() + 0.02;
        loop {
            let ms = unsafe { windows::Win32::Media::timeGetTime() };
            let now = now_seconds();
            if ms != first || now > give_up {
                return SystemClock { anchor_ns: i64::from(ms) * 1_000_000, anchor_s: now };
            }
            std::hint::spin_loop();
        }
    }

    fn nanos(&self, now: f64) -> i64 {
        self.anchor_ns + ((now - self.anchor_s) * 1e9) as i64
    }
}

/// Decides whether a connected stream can be claimed, remembering what it saw
/// between attempts.
struct Chooser {
    cfg: InstanceConfig,
    watch: Option<Watch>,
    /// When a fresh, unclaimed stream was first seen.
    first_seen: Option<(u64, f64)>,
}

enum Verdict {
    /// Claimed with this id.
    Claimed(u32),
    /// Another DAW owns it, or the previous owner gets it first: try later.
    Wait,
    /// The engine runs another shape than the DAW opened (its generation).
    ShapeDiffers(u64),
}

impl Chooser {
    fn decide(&mut self, client: &Client, was_owner: bool) -> Verdict {
        let (now, generation) = (now_seconds(), client.generation());
        if InstanceConfig::from_layout(&client.layout()) != self.cfg {
            return Verdict::ShapeDiffers(generation);
        }
        let h = client.header();
        let current = h.client_active.load(Ordering::Acquire);
        if current != 0 {
            // Another driver streams this instance. Take over only once its
            // heartbeat has stood still for a while (it crashed or hung).
            let heartbeat = h.client_alive.load(Ordering::Acquire);
            match self.watch {
                Some(w) if w.generation == generation && w.heartbeat == heartbeat => {
                    if now - w.since < STALL_S {
                        return Verdict::Wait;
                    }
                }
                _ => {
                    self.watch = Some(Watch { generation, heartbeat, since: now });
                    return Verdict::Wait;
                }
            }
        } else if !was_owner {
            // A fresh stream: let the previous owner reclaim it first.
            match self.first_seen {
                Some((g, since)) if g == generation => {
                    if now - since < CLAIM_GRACE_S {
                        return Verdict::Wait;
                    }
                }
                _ => {
                    self.first_seen = Some((generation, now));
                    return Verdict::Wait;
                }
            }
        }
        let id = new_client_id();
        if h.client_active.compare_exchange(current, id, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Verdict::Wait;
        }
        (self.watch, self.first_seen) = (None, None);
        client.set_name(crate::host::host_name());
        Verdict::Claimed(id)
    }
}

/// State shared by the stream thread and its prober.
#[derive(Default)]
struct ProbeShared {
    want: AtomicBool,
    stop: AtomicBool,
    /// The last stream ended while this driver owned it (it reclaims first).
    was_owner: AtomicBool,
    /// Generation of an engine stream whose shape differs from the DAW's (0 = none).
    shape_differs: AtomicU64,
}

/// A stream this driver has claimed. Dropping it gives the claim back (unless
/// another driver has taken over since), so a claim the stream thread never
/// took, or a link that ends, does not leave the instance looking owned.
struct Claim {
    client: Client,
    /// The id this driver claimed the stream with.
    id: u32,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let h = self.client.header();
        if h.client_active.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.client.set_name("");
        }
    }
}

/// One probing attempt: connect, decide, claim.
fn probe_once(name: &str, chooser: &mut Chooser, shared: &ProbeShared) -> Option<Claim> {
    let client = Client::connect(name).ok()??;
    match chooser.decide(&client, shared.was_owner.load(Ordering::Acquire)) {
        Verdict::Claimed(id) => {
            shared.was_owner.store(false, Ordering::Release);
            Some(Claim { client, id })
        }
        Verdict::Wait => None,
        Verdict::ShapeDiffers(generation) => {
            shared.shape_differs.store(generation, Ordering::Release);
            None
        }
    }
}

/// Looks for the engine on a helper thread and hands over only streams it has
/// already claimed, so the thread that drives the DAW's `bufferSwitch` never
/// opens or maps anything to find the engine, nor drops a stream it rejected.
/// (It does unmap a link that has died.) It sleeps between attempts but is
/// woken at once when asked to look again or to stop.
struct Prober {
    shared: Arc<ProbeShared>,
    found: Receiver<Claim>,
    thread: Option<JoinHandle<()>>,
    /// Only if the helper thread could not be started: probe inline, rarely.
    fallback: Option<(String, Chooser, f64)>,
}

impl Prober {
    fn start(name: String, cfg: InstanceConfig) -> Self {
        let shared = Arc::new(ProbeShared::default());
        shared.want.store(true, Ordering::Release);
        let (tx, found) = sync_channel(1);
        let chooser = Chooser { cfg, watch: None, first_seen: None };
        let sh = shared.clone();
        let spawned = std::thread::Builder::new().name("confluence-vasio-probe".into()).spawn({
            let name = name.clone();
            let mut chooser = Chooser { cfg, watch: None, first_seen: None };
            move || {
                while !sh.stop.load(Ordering::Acquire) {
                    if sh.want.load(Ordering::Acquire) {
                        if let Some(claimed) = probe_once(&name, &mut chooser, &sh) {
                            sh.want.store(false, Ordering::Release);
                            if tx.send(claimed).is_err() {
                                return;
                            }
                        }
                    }
                    std::thread::park_timeout(PROBE_INTERVAL);
                }
            }
        });
        match spawned {
            Ok(thread) => Prober { shared, found, thread: Some(thread), fallback: None },
            Err(_) => Prober { shared, found, thread: None, fallback: Some((name, chooser, 0.0)) },
        }
    }

    /// A stream claimed since the last call. Never blocks; allocates only in
    /// the fallback case where the helper thread could not be started.
    fn take(&mut self) -> Option<Claim> {
        if let Some((name, chooser, next)) = self.fallback.as_mut() {
            let now = now_seconds();
            if now < *next || !self.shared.want.load(Ordering::Acquire) {
                return None;
            }
            *next = now + 0.5;
            let claimed = probe_once(name, chooser, &self.shared)?;
            self.shared.want.store(false, Ordering::Release);
            return Some(claimed);
        }
        self.found.try_recv().ok()
    }

    /// Asks for another attempt now.
    fn look_again(&self) {
        self.shared.want.store(true, Ordering::Release);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}

impl Drop for Prober {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Unblock a pending send by draining; wake the thread; wait for it.
        while self.found.try_recv().is_ok() {}
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// One channel's `2 * block` double buffer, owned by the driver object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Buffer(pub *mut f32);

// SAFETY: the driver keeps the buffer alive until the stream thread is joined,
// and only the stream thread (plus the DAW inside bufferSwitch) touches it.
unsafe impl Send for Buffer {}

impl Buffer {
    fn half(self, half: usize, block: usize) -> *mut f32 {
        // SAFETY: half is 0 or 1; the buffer holds 2 * block samples.
        unsafe { self.0.add(half * block) }
    }
}

pub(crate) struct Params {
    pub instance: u32,
    pub cfg: InstanceConfig,
    pub block: usize,
    pub callbacks: AsioCallbacks,
    /// Per DAW channel; `None` if the DAW did not create a buffer for it.
    pub inputs: Vec<Option<Buffer>>,
    pub outputs: Vec<Option<Buffer>>,
    pub position: Arc<Position>,
}

// SAFETY: the callbacks are plain function pointers the host allows to be
// called from the driver's thread; buffers are covered by `Buffer`.
unsafe impl Send for Params {}

pub(crate) struct Stream {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Stream {
    pub fn start(params: Params) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("confluence-vasio-{}", params.instance))
            .spawn(move || run(params, &flag))?;
        Ok(Stream { stop, thread })
    }

    /// Asks the thread to stop after its current block (never blocks).
    pub fn signal(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// True when called from this stream's own thread (i.e. from inside the
    /// DAW's `bufferSwitch`), where waiting for the thread would deadlock.
    pub fn is_current_thread(&self) -> bool {
        self.thread.thread().id() == std::thread::current().id()
    }

    /// Stops the thread and waits for it (the DAW's buffers stay valid until
    /// then). Must not be called from the stream thread itself.
    pub fn stop(self) {
        self.signal();
        let _ = self.thread.join();
    }
}

/// A live connection to the engine.
struct Link {
    /// Dropped first: its ends below touch no memory when dropped.
    claim: Claim,
    from_engine: RingReader,
    to_engine: RingWriter,
    last_heartbeat: u64,
    heartbeat_changed_at: f64,
    missed_wakes: u32,
    stalled: bool,
}

enum Tick {
    /// The engine paced this block (its audio, or silence if it was late).
    Engine,
    /// No engine pacing: wait for the timer, use silence.
    Timer,
}

impl Link {
    /// A link over a stream the prober has claimed for this driver.
    fn claimed(claim: Claim) -> Link {
        // SAFETY: the ends live in the same `Link` as the claim's client and
        // are only used through it while it is alive (dropping an end touches
        // no memory).
        let (mut from_engine, to_engine) = unsafe { claim.client.ends() };
        from_engine.skip_all();
        let last_heartbeat = claim.client.header().server_heartbeat.load(Ordering::Acquire);
        Link {
            claim,
            from_engine,
            to_engine,
            last_heartbeat,
            heartbeat_changed_at: now_seconds(),
            missed_wakes: 0,
            stalled: false,
        }
    }

    /// Fills the DAW's inputs for this block. `None` = the link is dead.
    fn next_block(&mut self, p: &Params, half: usize, timeout_ms: u32) -> Option<Tick> {
        let owner = self.claim.client.header().client_active.load(Ordering::Acquire);
        if !self.claim.client.is_current() || owner != self.claim.id {
            return None;
        }
        let now = now_seconds();
        let hb = self.claim.client.header().server_heartbeat.load(Ordering::Acquire);
        if hb != self.last_heartbeat {
            self.last_heartbeat = hb;
            self.heartbeat_changed_at = now;
            if self.stalled {
                self.stalled = false;
                self.missed_wakes = 0;
                self.from_engine.skip_all();
            }
        } else if now - self.heartbeat_changed_at > STALL_S {
            self.stalled = true;
        }
        if !self.stalled && !self.claim.client.wait(timeout_ms) {
            // Missed wake-ups: after a couple, pace on the timer so the DAW
            // keeps its full callback rate while the engine is away.
            self.missed_wakes += 1;
            if self.missed_wakes >= MISSED_WAKES_TO_STALL {
                self.stalled = true;
            }
            silence(p, half);
            return Some(Tick::Engine);
        }
        if self.stalled {
            silence(p, half);
            return Some(Tick::Timer);
        }
        self.missed_wakes = 0;
        let block = p.block;
        let backlog = self.from_engine.available().saturating_sub(block as u64);
        self.from_engine.skip(backlog);
        let got = self.from_engine.read_frames(block, |ch, f, s| {
            if let Some(b) = p.inputs[ch] {
                // SAFETY: f < block, inside this half.
                unsafe { *b.half(half, block).add(f) = s };
            }
        });
        if !got {
            silence(p, half);
        }
        Some(Tick::Engine)
    }

    fn send_outputs(&mut self, p: &Params, half: usize) {
        if self.claim.client.header().client_active.load(Ordering::Acquire) != self.claim.id {
            // Displaced while the DAW was inside bufferSwitch: never write into
            // a stream another driver now owns.
            return;
        }
        let block = p.block;
        let outputs = &p.outputs;
        self.to_engine.write_frames(block, |ch, f| match outputs[ch] {
            // SAFETY: f < block, inside this half.
            Some(b) => unsafe { *b.half(half, block).add(f) },
            None => 0.0,
        });
        self.claim.client.header().client_heartbeat.fetch_add(1, Ordering::Release);
    }
}

fn silence(p: &Params, half: usize) {
    for b in p.inputs.iter().flatten() {
        // SAFETY: one half of the buffer, `block` samples.
        unsafe { std::slice::from_raw_parts_mut(b.half(half, p.block), p.block) }.fill(0.0);
    }
}

fn supports_time_info(cb: &AsioCallbacks) -> bool {
    (cb.asio_message)(K_SELECTOR_SUPPORTED, K_SUPPORTS_TIME_INFO, null_mut(), null_mut()) != 0
        && (cb.asio_message)(K_SUPPORTS_TIME_INFO, 0, null_mut(), null_mut()) != 0
}

fn run(p: Params, stop: &AtomicBool) {
    let _mmcss = ProAudioThread::enter().ok();
    let block_s = p.block as f64 / p.cfg.sample_rate as f64;
    let timeout_ms = ((2.0 * block_s * 1000.0).ceil() as u32).max(10);
    let time_info = supports_time_info(&p.callbacks);
    let can_reset = (p.callbacks.asio_message)(K_SELECTOR_SUPPORTED, K_RESET_REQUEST, null_mut(), null_mut()) != 0;
    let mut reset_requested_for = None;
    let mut prober = Prober::start(stream_name(p.instance), p.cfg);
    let clock = SystemClock::new();
    let mut link: Option<Link> = None;
    p.position.running.store(true, Ordering::Release);
    let mut deadline = now_seconds() + block_s;
    let mut half = 0usize;
    let mut samples = 0i64;
    while !stop.load(Ordering::Acquire) {
        // A bug here must never take the DAW's audio thread down with it.
        let step = catch_unwind(AssertUnwindSafe(|| {
            let tick = match link.as_mut().map(|l| l.next_block(&p, half, timeout_ms)) {
                Some(Some(t)) => t,
                Some(None) => {
                    // A link that ends while this driver still owned it (engine
                    // closed or restarted) gets first claim on the next stream.
                    let was_owner = link
                        .as_ref()
                        .is_some_and(|l| l.claim.client.header().client_active.load(Ordering::Acquire) == l.claim.id);
                    prober.shared.was_owner.store(was_owner, Ordering::Release);
                    link = None;
                    prober.look_again();
                    silence(&p, half);
                    Tick::Timer
                }
                None => {
                    silence(&p, half);
                    Tick::Timer
                }
            };
            let now = now_seconds();
            match tick {
                Tick::Engine => deadline = now + block_s,
                Tick::Timer => {
                    if deadline > now {
                        std::thread::sleep(Duration::from_secs_f64(deadline - now));
                    }
                    // Far behind (e.g. the machine was suspended): don't burst to catch up.
                    deadline = (deadline + block_s).max(now_seconds() - 2.0 * block_s);
                }
            }
            if link.is_none() {
                if let Some(claim) = prober.take() {
                    link = Some(Link::claimed(claim));
                }
                // The engine runs another shape than the DAW opened: ask the DAW
                // to re-initialise (once per engine generation, if it supports that).
                let differs = prober.shared.shape_differs.load(Ordering::Acquire);
                if differs != 0 && reset_requested_for != Some(differs) {
                    reset_requested_for = Some(differs);
                    if can_reset {
                        (p.callbacks.asio_message)(K_RESET_REQUEST, 0, null_mut(), null_mut());
                    }
                }
            }
            let system_ns = clock.nanos(now_seconds());
            p.position.samples.store(samples, Ordering::Release);
            p.position.nanos.store(system_ns, Ordering::Release);
            if time_info {
                // SAFETY: plain-old-data struct; all-zero is a valid value.
                let mut t: AsioTime = unsafe { std::mem::zeroed() };
                t.time_info.speed = 1.0;
                t.time_info.system_time = AsioTimeStamp::from_value(system_ns);
                t.time_info.sample_position = AsioSamples::from_value(samples);
                t.time_info.sample_rate = p.cfg.sample_rate as f64;
                t.time_info.flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID;
                (p.callbacks.buffer_switch_time_info)(&mut t, half as i32, ASIO_TRUE);
            } else {
                (p.callbacks.buffer_switch)(half as i32, ASIO_TRUE);
            }
            if let (Tick::Engine, Some(l)) = (tick, link.as_mut()) {
                l.send_outputs(&p, half);
            }
            if let Some(l) = link.as_ref() {
                // Alive even while the engine is away, so a waiting DAW never
                // mistakes an engine stall for this DAW having died.
                l.claim.client.header().client_alive.fetch_add(1, Ordering::Release);
            }
            samples += p.block as i64;
            half ^= 1;
        }));
        if step.is_err() {
            std::thread::sleep(Duration::from_secs_f64(block_s));
        }
    }
    p.position.running.store(false, Ordering::Release);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use confluence_shm::{Layout, Server};

    #[test]
    fn a_claim_the_stream_thread_never_took_is_given_back() {
        let name = format!("VASIO.unit.{}.claim", std::process::id());
        let cfg = InstanceConfig::default();
        let layout = Layout {
            sample_rate: f64::from(cfg.sample_rate),
            block: cfg.block,
            to_client_channels: cfg.daw_inputs,
            from_client_channels: cfg.daw_outputs,
            capacity_frames: cfg.block * 4,
        };
        let server = Server::create(&name, layout).unwrap();
        let prober = Prober::start(name, cfg);
        let give_up = now_seconds() + 5.0;
        while server.header().client_active.load(Ordering::Acquire) == 0 {
            assert!(now_seconds() < give_up, "the prober never claimed the stream");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The DAW stops before the stream thread took the claimed stream.
        drop(prober);
        assert_eq!(server.header().client_active.load(Ordering::Acquire), 0, "the instance looks owned");
    }
}
