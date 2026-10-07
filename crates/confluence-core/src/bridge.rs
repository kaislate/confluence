//! Clock-domain bridges between a soft-clocked device and the master-clocked engine.
//!
//! Each bridge is split into a device side (called from the device's own
//! callback thread) and an engine side (called once per master block). Audio
//! crosses in an interleaved SPSC ring; device timestamps cross in a second
//! ring and feed a [`RateEstimator`]. A [`FillController`] trims the resampling
//! ratio so the ring stays near its target fill.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::asrc::{Asrc, AsrcError, AsrcQuality, FixedSide};
use crate::buffer::PlanarBuffer;
use crate::clock::{FillController, RateEstimator, DEFAULT_RATE_BANDWIDTH_HZ};

/// Minimum running time after (re)start before the fill controller's slew
/// limit may engage. It also needs a settled rate estimate, a small fill error
/// and a steady correction.
const LOCK_AFTER_S: f64 = 5.0;
/// Time constant of the slow average the correction is compared against to
/// decide whether it is steady.
const STEADY_FILTER_S: f64 = 2.0;
/// A correction within this of its slow average (≈ changing by less than
/// 10 ppm/s) is steady. A loop still sweeping through its target is not.
const STEADY_PPM: f64 = 20.0;
/// Headroom (spare frames in the ring when audio is consumed) is evaluated
/// over windows of this length.
const HEADROOM_WINDOW_S: f64 = 5.0;
/// If a window's minimum headroom falls below this, the target grows by a
/// quarter device block before any underrun happens.
const HEADROOM_SAFETY_S: f64 = 0.001;
/// Consecutive windows with generous headroom required before the target
/// shrinks by an eighth of a device block (never below the base target).
const QUIET_WINDOWS_TO_SHRINK: u32 = 3;
/// Upper bound on adaptive target growth, in device blocks above the base target.
const MAX_EXTRA_BLOCKS: f64 = 8.0;
const STAMP_QUEUE: usize = 256;
/// An input's loop starts only after its device has delivered for this long.
const INPUT_WARMUP_S: f64 = 0.5;
/// Rate at which latency left over from a stream's start drains away, in
/// frames per second (at 48 kHz, 2 frames/s needs a steady ~40 ppm correction).
const START_EXCESS_DRAIN_PER_S: f64 = 2.0;

/// (frames transferred since the previous callback, callback time in seconds).
type Stamp = (u32, f64);
/// A stamp with this frame count says the device's timestamps changed base.
const REANCHOR: u32 = u32::MAX;
/// Time constant of the low-pass filter on the measured fill, removing
/// timestamp jitter before it reaches the controller.
const FILL_FILTER_S: f64 = 0.5;

#[derive(Clone, Copy, Debug)]
pub struct BridgeConfig {
    pub channels: usize,
    pub device_rate: f64,
    pub device_block: usize,
    pub master_rate: f64,
    pub master_block: usize,
    pub quality: AsrcQuality,
    /// Safety margin added to the base target fill (spec default 2 ms).
    pub margin_frames: usize,
    /// How far the target may grow above the base when headroom runs short;
    /// `None` = 8 device blocks (enough for USB devices; networks need more).
    pub max_growth_frames: Option<usize>,
}

impl BridgeConfig {
    /// Base target: one device block + one master block + margin.
    pub fn base_target(&self) -> f64 {
        (self.device_block + self.master_block + self.margin_frames) as f64
    }

    /// Frames the target may grow by.
    pub fn max_growth(&self) -> usize {
        self.max_growth_frames.unwrap_or(MAX_EXTRA_BLOCKS as usize * self.device_block)
    }

    fn ring_frames(&self) -> usize {
        (self.base_target() as usize + self.device_block) * 2 + self.max_growth() + self.master_block * 4
    }
}

/// Health counters shared between both sides and the control thread.
pub struct BridgeStats {
    underruns: AtomicU64,
    overruns: AtomicU64,
    fill_bits: AtomicU64,
    target_bits: AtomicU64,
    device_ppm_bits: AtomicU64,
    correction_ppm_bits: AtomicU64,
    /// Output bridges: smallest headroom the device side saw since the engine
    /// side last looked (`i64::MAX` = no reading).
    device_min_headroom: AtomicI64,
    /// Output bridges: the device has called back at least once (it may still
    /// be priming). Until then the engine queues nothing.
    device_started: AtomicBool,
    /// Input bridges: frames above the base target the device side needs
    /// (a network receiver's wait for late packets), as f64 bits.
    floor_bits: AtomicU64,
}

impl Default for BridgeStats {
    fn default() -> Self {
        Self {
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            fill_bits: AtomicU64::new(0),
            target_bits: AtomicU64::new(0),
            device_ppm_bits: AtomicU64::new(0),
            correction_ppm_bits: AtomicU64::new(0),
            device_min_headroom: AtomicI64::new(i64::MAX),
            device_started: AtomicBool::new(false),
            floor_bits: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BridgeHealth {
    pub underruns: u64,
    pub overruns: u64,
    pub fill_frames: f64,
    pub target_frames: f64,
    pub device_ppm: f64,
    pub correction_ppm: f64,
}

impl BridgeStats {
    pub fn snapshot(&self) -> BridgeHealth {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        BridgeHealth {
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            fill_frames: f(&self.fill_bits),
            target_frames: f(&self.target_bits),
            device_ppm: f(&self.device_ppm_bits),
            correction_ppm: f(&self.correction_ppm_bits),
        }
    }

    fn publish(&self, fill: f64, target: f64, device_ppm: f64, correction_ppm: f64) {
        self.fill_bits.store(fill.to_bits(), Ordering::Relaxed);
        self.target_bits.store(target.to_bits(), Ordering::Relaxed);
        self.device_ppm_bits.store(device_ppm.to_bits(), Ordering::Relaxed);
        self.correction_ppm_bits.store(correction_ppm.to_bits(), Ordering::Relaxed);
    }
}

/// Clock-tracking state shared by both bridge directions (engine side).
struct Tracker {
    cfg: BridgeConfig,
    stamps: Consumer<Stamp>,
    device_est: RateEstimator,
    ctl: FillController,
    target: f64,
    fixed_target: Option<f64>,
    running: bool,
    /// The loop has run at least once (the first start adopts the initial fill).
    started: bool,
    /// Fill above the target that the first start found; the effective target
    /// includes it and it drains slowly, so the start needs no large correction.
    start_excess: f64,
    run_time: f64,
    /// Smallest headroom seen in the current window, in device frames.
    window_min_headroom: f64,
    window_time: f64,
    quiet_windows: u32,
    /// Time of the newest device timestamp, if any.
    last_stamp: Option<f64>,
    /// Time of the first device timestamp, if any.
    first_stamp: Option<f64>,
    /// Low-pass filtered fill; `None` until the first measurement after (re)start.
    fill_filtered: Option<f64>,
    /// Slow average of the correction, for judging whether the loop is steady.
    corr_average: Option<f64>,
    stats: Arc<BridgeStats>,
}

impl Tracker {
    fn new(cfg: BridgeConfig, stamps: Consumer<Stamp>, stats: Arc<BridgeStats>) -> Self {
        Self {
            cfg,
            stamps,
            device_est: RateEstimator::new(cfg.device_rate, DEFAULT_RATE_BANDWIDTH_HZ),
            ctl: FillController::with_defaults(cfg.master_rate),
            target: cfg.base_target(),
            fixed_target: None,
            running: false,
            started: false,
            start_excess: 0.0,
            run_time: 0.0,
            window_min_headroom: f64::INFINITY,
            window_time: 0.0,
            quiet_windows: 0,
            last_stamp: None,
            first_stamp: None,
            fill_filtered: None,
            corr_average: None,
            stats,
        }
    }

    fn dt(&self) -> f64 {
        self.cfg.master_block as f64 / self.cfg.master_rate
    }

    fn target(&self) -> f64 {
        self.fixed_target.unwrap_or(self.target + self.start_excess)
    }

    fn drain_stamps(&mut self) {
        while let Ok((frames, time)) = self.stamps.pop() {
            if frames == REANCHOR {
                self.device_est.reanchor();
                continue;
            }
            self.first_stamp.get_or_insert(time);
            self.device_est.update(frames, time);
            self.last_stamp = Some(time);
        }
    }

    /// Frames the device has produced (input) or consumed (output) since its
    /// last callback, estimated from its measured rate. Adding/subtracting this
    /// turns the block-quantized ring level into a continuous-time fill, so the
    /// slow phase slide between device and master blocks is not seen as error.
    fn frames_since_stamp(&self, now: f64) -> f64 {
        match self.last_stamp {
            Some(t) => ((now - t) * self.device_est.rate()).clamp(0.0, self.cfg.device_block as f64),
            None => 0.0,
        }
    }

    /// Returns the fill-controller correction in ppm (positive = ring too full).
    /// The lowest target allowed: the base, raised by the device side's floor.
    fn floor_target(&self) -> f64 {
        let floor = f64::from_bits(self.stats.floor_bits.load(Ordering::Relaxed));
        self.cfg.base_target() + floor.min(self.cfg.max_growth() as f64)
    }

    fn correction(&mut self, fill: f64) -> f64 {
        let floor = self.floor_target();
        if floor > self.target {
            self.move_target(floor);
        }
        let dt = self.dt();
        let alpha = (dt / FILL_FILTER_S).min(1.0);
        let filtered = match self.fill_filtered {
            Some(f) => f + alpha * (fill - f),
            None => fill,
        };
        self.fill_filtered = Some(filtered);
        let fill = filtered;
        let err = fill - self.target();
        // Slew-limit only a converged loop; release it on a large disturbance.
        // Converged means a small error *and* a steady, unsaturated correction:
        // a loop sweeping through its target with a large correction must not
        // lock, or the slew limit stops it unwinding and it overshoots.
        let block = self.cfg.device_block as f64;
        let out = self.ctl.output();
        let average = self.corr_average.map_or(out, |a| a + (dt / STEADY_FILTER_S).min(1.0) * (out - a));
        self.corr_average = Some(average);
        let steady = (out - average).abs() < STEADY_PPM && !self.ctl.is_saturated();
        if self.ctl.is_locked() && err.abs() > block {
            self.ctl.unlock();
        } else if !self.ctl.is_locked()
            && self.run_time >= LOCK_AFTER_S
            && self.device_est.is_settled()
            && err.abs() < block / 4.0
            && steady
        {
            self.ctl.lock();
        }
        let corr = self.ctl.update(err, dt);
        self.start_excess = (self.start_excess - START_EXCESS_DRAIN_PER_S * dt).max(0.0);
        self.run_time += dt;
        self.window_time += dt;
        if self.window_time >= HEADROOM_WINDOW_S {
            self.evaluate_headroom();
        }
        self.stats.publish(fill, self.target(), self.device_est.ppm(), corr);
        corr
    }

    /// Records the spare frames in the ring at the moment audio is consumed.
    fn headroom(&mut self, frames: f64) {
        self.window_min_headroom = self.window_min_headroom.min(frames);
    }

    /// Adaptive latency from measured headroom (spec §6.3): grow before an
    /// underrun when headroom gets thin; shrink only after sustained slack.
    /// Only a converged (locked) loop is judged: while acquiring, headroom
    /// reflects the transient, and moving the target would fight the controller.
    fn evaluate_headroom(&mut self) {
        let block = self.cfg.device_block as f64;
        let safety = HEADROOM_SAFETY_S * self.cfg.device_rate;
        let min = self.window_min_headroom;
        (self.window_min_headroom, self.window_time) = (f64::INFINITY, 0.0);
        let settled = self.fill_filtered.is_some_and(|f| (f - self.target()).abs() < block / 4.0);
        if !min.is_finite() || !self.ctl.is_locked() || !settled {
            self.quiet_windows = 0;
            return;
        }
        if min < safety {
            self.quiet_windows = 0;
            self.grow(block / 4.0);
        } else if min > block / 2.0 + safety {
            self.quiet_windows += 1;
            if self.quiet_windows >= QUIET_WINDOWS_TO_SHRINK {
                self.quiet_windows = 0;
                self.move_target((self.target - block / 8.0).max(self.floor_target()));
            }
        } else {
            self.quiet_windows = 0;
        }
    }

    fn grow(&mut self, frames: f64) {
        let max = self.cfg.base_target() + self.cfg.max_growth() as f64;
        self.move_target((self.target + frames).min(max));
    }

    /// A new target is a deliberate step, not drift to track: the slew limit
    /// would make the loop crawl there and overshoot, so it is released, and
    /// the loop must settle again before it locks.
    fn move_target(&mut self, target: f64) {
        if target != self.target {
            self.target = target;
            self.ctl.unlock();
            self.run_time = 0.0;
        }
    }

    /// Records an xrun: stop, raise the target by one device block, restart the controller.
    fn xrun(&mut self) {
        self.running = false;
        self.run_time = 0.0;
        (self.window_min_headroom, self.window_time, self.quiet_windows) = (f64::INFINITY, 0.0, 0);
        self.fill_filtered = None;
        self.corr_average = None;
        self.ctl.reset();
        self.grow(self.cfg.device_block as f64);
    }
}

struct Rings {
    samples_tx: Producer<f32>,
    samples_rx: Consumer<f32>,
    stamps_tx: Producer<Stamp>,
    stamps_rx: Consumer<Stamp>,
}

fn rings(cfg: &BridgeConfig) -> Rings {
    let (samples_tx, samples_rx) = RingBuffer::new(cfg.ring_frames() * cfg.channels);
    let (stamps_tx, stamps_rx) = RingBuffer::new(STAMP_QUEUE);
    Rings { samples_tx, samples_rx, stamps_tx, stamps_rx }
}

// ---------------------------------------------------------------- input ----

/// Device → engine. Create with [`soft_input`].
pub struct InputDeviceSide {
    samples: Producer<f32>,
    stamps: Producer<Stamp>,
    channels: usize,
    stats: Arc<BridgeStats>,
}

pub struct InputEngineSide {
    samples: Consumer<f32>,
    asrc: Asrc,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
    tracker: Tracker,
}

pub fn soft_input(cfg: BridgeConfig) -> Result<(InputDeviceSide, InputEngineSide, Arc<BridgeStats>), AsrcError> {
    let asrc =
        Asrc::new(cfg.quality, cfg.master_rate / cfg.device_rate, cfg.master_block, cfg.channels, FixedSide::Output)?;
    let r = rings(&cfg);
    let stats = Arc::new(BridgeStats::default());
    let scratch_in = vec![vec![0.0; asrc.input_frames_max()]; cfg.channels];
    let scratch_out = vec![vec![0.0; asrc.output_frames_max()]; cfg.channels];
    let device =
        InputDeviceSide { samples: r.samples_tx, stamps: r.stamps_tx, channels: cfg.channels, stats: stats.clone() };
    let engine = InputEngineSide {
        samples: r.samples_rx,
        asrc,
        scratch_in,
        scratch_out,
        tracker: Tracker::new(cfg, r.stamps_rx, stats.clone()),
    };
    Ok((device, engine, stats))
}

impl InputDeviceSide {
    /// The device side can stall for up to `frames` (e.g. a network receiver
    /// waiting for a late packet): the target is kept at least that far above
    /// its base, within the growth limit.
    /// The device's timestamps now come from another reference (a network
    /// stream picked up afresh): the rate estimate keeps its rate and takes
    /// the next timestamp as its new phase, instead of reading the step as drift.
    pub fn restart_clock(&mut self) {
        let _ = self.stamps.push((REANCHOR, 0.0));
    }

    pub fn set_latency_floor(&self, frames: f64) {
        self.stats.floor_bits.store(frames.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Device callback: `data` holds whole interleaved frames captured ending at `time` (seconds).
    pub fn write_interleaved(&mut self, data: &[f32], time: f64) {
        let frames = data.len() / self.channels;
        let _ = self.stamps.push((frames as u32, time));
        if self.samples.push_entire_slice(&data[..frames * self.channels]).is_err() {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl InputEngineSide {
    /// Engine block: writes `out.frames()` (= master block) frames into
    /// `out` channels `first_channel..first_channel + channels`.
    /// `now` is the block time on the same clock as the device timestamps;
    /// `master_ppm` is the master clock's measured deviation from nominal.
    pub fn read(&mut self, out: &mut PlanarBuffer, first_channel: usize, now: f64, master_ppm: f64) {
        let ch = self.asrc.channels();
        let t = &mut self.tracker;
        t.drain_stamps();
        let mut avail = self.samples.slots() / ch;
        if !t.running {
            // Keep the ring at the target, dropping the oldest audio (the
            // output is silent until the loop runs), so the loop starts
            // without error and the ring never overflows while waiting.
            let excess = avail as f64 + t.frames_since_stamp(now) - t.target();
            if excess > 0.0 {
                let drop = (excess as usize).min(avail);
                if let Ok(chunk) = self.samples.read_chunk(drop * ch) {
                    chunk.commit_all();
                    avail -= drop;
                }
            }
            // Some devices start irregularly (process loopback delivers its
            // first packets in bursts with 20 ms gaps): start only once the
            // device has been delivering for a while.
            let warming = t.first_stamp.is_none_or(|first| now - first < INPUT_WARMUP_S);
            if excess < 0.0 || warming {
                silence(out, first_channel, ch);
                return;
            }
            t.running = true;
        }
        let fill = avail as f64 + t.frames_since_stamp(now);
        let corr = t.correction(fill);
        let rel = (1.0 + master_ppm * 1e-6) / (1.0 + t.device_est.ppm() * 1e-6) * (1.0 - corr * 1e-6);
        self.asrc.set_relative_ratio(rel);

        let need = self.asrc.input_frames_next();
        t.headroom(avail as f64 - need as f64);
        if avail < need {
            t.stats.underruns.fetch_add(1, Ordering::Relaxed);
            t.xrun();
            silence(out, first_channel, ch);
            return;
        }
        if let Ok(chunk) = self.samples.read_chunk(need * ch) {
            let (a, b) = chunk.as_slices();
            for (idx, &s) in a.iter().chain(b).enumerate() {
                self.scratch_in[idx % ch][idx / ch] = s;
            }
            chunk.commit_all();
        }
        match self.asrc.process(&self.scratch_in, &mut self.scratch_out) {
            Ok((_, produced)) => {
                for c in 0..ch {
                    let dst = out.channel_mut(first_channel + c);
                    let n = dst.len().min(produced);
                    dst[..n].copy_from_slice(&self.scratch_out[c][..n]);
                    dst[n..].fill(0.0);
                }
            }
            Err(_) => silence(out, first_channel, ch),
        }
    }

    /// Pins the target fill (frames), or `None` for adaptive.
    pub fn set_fixed_target(&mut self, frames: Option<f64>) {
        self.tracker.fixed_target = frames;
    }
}

// --------------------------------------------------------------- output ----

/// Engine → device. Create with [`soft_output`].
pub struct OutputEngineSide {
    samples: Producer<f32>,
    ring_slots: usize,
    asrc: Asrc,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
    tracker: Tracker,
    /// Device-side underruns already handled by the tracker.
    seen_underruns: u64,
}

pub struct OutputDeviceSide {
    samples: Consumer<f32>,
    stamps: Producer<Stamp>,
    channels: usize,
    prime_frames: usize,
    primed: bool,
    stats: Arc<BridgeStats>,
}

pub fn soft_output(cfg: BridgeConfig) -> Result<(OutputEngineSide, OutputDeviceSide, Arc<BridgeStats>), AsrcError> {
    let asrc =
        Asrc::new(cfg.quality, cfg.device_rate / cfg.master_rate, cfg.master_block, cfg.channels, FixedSide::Input)?;
    let r = rings(&cfg);
    let ring_slots = r.samples_tx.buffer().capacity();
    let stats = Arc::new(BridgeStats::default());
    let scratch_in = vec![vec![0.0; asrc.input_frames_max()]; cfg.channels];
    let scratch_out = vec![vec![0.0; asrc.output_frames_max()]; cfg.channels];
    let engine = OutputEngineSide {
        samples: r.samples_tx,
        ring_slots,
        asrc,
        scratch_in,
        scratch_out,
        tracker: Tracker::new(cfg, r.stamps_rx, stats.clone()),
        seen_underruns: 0,
    };
    let device = OutputDeviceSide {
        samples: r.samples_rx,
        stamps: r.stamps_tx,
        channels: cfg.channels,
        prime_frames: cfg.base_target() as usize,
        primed: false,
        stats: stats.clone(),
    };
    Ok((engine, device, stats))
}

impl OutputEngineSide {
    /// Engine block: takes `block` channels `first_channel..first_channel + channels`.
    /// `now` and `master_ppm` as for [`InputEngineSide::read`].
    pub fn write(&mut self, block: &PlanarBuffer, first_channel: usize, now: f64, master_ppm: f64) {
        let ch = self.asrc.channels();
        let t = &mut self.tracker;
        t.drain_stamps();
        // A device-side underrun is this bridge's xrun too: the engine side
        // raises its target and restarts its loop, like the input side does.
        let seen = t.stats.device_min_headroom.swap(i64::MAX, Ordering::Relaxed);
        if seen != i64::MAX {
            t.headroom(seen as f64);
        }
        let underruns = t.stats.underruns.load(Ordering::Relaxed);
        if underruns != self.seen_underruns {
            self.seen_underruns = underruns;
            t.xrun();
        }
        let ring = ((self.ring_slots - self.samples.slots()) / ch) as f64;
        let fill = ring - t.frames_since_stamp(now);
        if t.last_stamp.is_none() {
            // The device has not consumed anything yet. Until it calls back at
            // all (driver start-up), queue nothing: audio queued now would play
            // late, and dropping some of it later would splice the pre-roll.
            // Once it runs, feed it every block at the nominal ratio while it
            // primes, so it starts on one continuous stream. The controller
            // waits for real consumption: running it now would only integrate
            // the start-up wait into a large, slowly unwinding correction.
            if !t.stats.device_started.load(Ordering::Acquire) {
                return;
            }
            // It primes at the target plus its block; this is a master block
            // beyond that. Only a device that stalls while priming gets here:
            // stop queuing rather than fill the ring and count an overrun on
            // every block.
            if fill >= t.target() + (t.cfg.device_block + t.cfg.master_block) as f64 {
                return;
            }
            self.asrc.set_relative_ratio(1.0 / (1.0 + master_ppm * 1e-6));
        } else {
            if !t.running {
                // After an xrun, wait until the device has drained the ring to the
                // target (re-centring latency). Blocks produced meanwhile are
                // dropped: queuing them would only add delay, and it avoids
                // counting one stall as an overrun on every block.
                if t.started && fill > t.target() {
                    return;
                }
                if !t.started {
                    // First start: begin where the ring is. Priming is block
                    // granular, so the first fill is up to a block above the
                    // target; forcing it down would mean a saturated correction
                    // or dropping audio. Start the loop there instead and let
                    // the excess drain slowly (never below the target).
                    t.started = true;
                    t.start_excess = (fill - t.target).max(0.0);
                }
                t.running = true;
            }
            let corr = t.correction(fill);
            let rel = (1.0 + t.device_est.ppm() * 1e-6) / (1.0 + master_ppm * 1e-6) * (1.0 - corr * 1e-6);
            self.asrc.set_relative_ratio(rel);
        }

        let frames = block.frames().min(self.asrc.input_frames_next());
        for c in 0..ch {
            self.scratch_in[c][..frames].copy_from_slice(&block.channel(first_channel + c)[..frames]);
        }
        let produced = match self.asrc.process(&self.scratch_in, &mut self.scratch_out) {
            Ok((_, produced)) => produced,
            Err(_) => return,
        };
        let Ok(chunk) = self.samples.write_chunk_uninit(produced * ch) else {
            t.stats.overruns.fetch_add(1, Ordering::Relaxed);
            t.xrun();
            return;
        };
        let out = &self.scratch_out;
        chunk.fill_from_iter((0..produced).flat_map(|n| (0..ch).map(move |c| out[c][n])));
    }
}

impl OutputDeviceSide {
    /// Device callback: fills `data` with whole interleaved frames to be played, at `time`.
    pub fn read_interleaved(&mut self, data: &mut [f32], time: f64) {
        let frames = data.len() / self.channels;
        if !self.primed {
            self.stats.device_started.store(true, Ordering::Release);
        }
        let avail = self.samples.slots() / self.channels;
        if !self.primed {
            // Prime to the engine side's current (possibly raised) target plus
            // the block about to be taken, so the ring still holds the target
            // afterwards and the loop starts with zero error (spec §6.3),
            // without splicing silence into the audio.
            let target = f64::from_bits(self.stats.target_bits.load(Ordering::Relaxed));
            if (avail as f64) < target.max(self.prime_frames as f64) + frames as f64 {
                data.fill(0.0);
                return;
            }
            self.primed = true;
        }
        // A stamp means "consumed `frames` at `time`": only sent while actually
        // consuming. Stamps from a device still priming (playing silence and
        // taking nothing) made the engine count phantom consumption and start
        // its loop a block off.
        let _ = self.stamps.push((frames as u32, time));
        self.stats.device_min_headroom.fetch_min(avail as i64 - frames as i64, Ordering::Relaxed);
        if self.samples.pop_entire_slice(&mut data[..frames * self.channels]).is_err() {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.primed = false;
            data.fill(0.0);
        }
    }
}

fn silence(out: &mut PlanarBuffer, first_channel: usize, channels: usize) {
    for c in first_channel..first_channel + channels {
        out.channel_mut(c).fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BridgeConfig {
        BridgeConfig {
            channels: 2,
            device_rate: 48_000.0,
            device_block: 128,
            master_rate: 48_000.0,
            master_block: 256,
            quality: AsrcQuality::Sinc64,
            margin_frames: 24,
            max_growth_frames: None,
        }
    }

    #[test]
    fn partial_frames_from_a_device_are_ignored() {
        let (mut dev, mut eng, stats) = soft_input(cfg()).unwrap();
        // 3 samples on 2 channels = 1 whole frame + 1 stray sample.
        dev.write_interleaved(&[0.1, 0.2, 0.3], 0.0);
        let mut out = PlanarBuffer::new(2, 256);
        eng.read(&mut out, 0, 0.001, 0.0);
        assert_eq!(eng.samples.slots(), 2, "exactly one frame was queued");
        assert_eq!(stats.snapshot().overruns, 0);
    }

    #[test]
    fn a_huge_device_buffer_is_counted_as_overrun_not_a_crash() {
        let (mut dev, _eng, stats) = soft_input(cfg()).unwrap();
        dev.write_interleaved(&vec![0.0; 2 * 1_000_000], 0.0);
        assert_eq!(stats.snapshot().overruns, 1);
    }

    /// A running input bridge, its floor set to `floor` just before one more block.
    fn target_with_floor(floor: Option<f64>) -> f64 {
        let (mut dev, mut eng, stats) = soft_input(cfg()).unwrap();
        let mut out = PlanarBuffer::new(2, 256);
        let block = vec![0.0f32; 2 * 128];
        let mut t = 0.0;
        for i in 0..400 {
            if i == 399 {
                if let Some(f) = floor {
                    dev.set_latency_floor(f);
                }
            }
            dev.write_interleaved(&block, t);
            dev.write_interleaved(&block, t + 128.0 / 48_000.0);
            t += 256.0 / 48_000.0;
            eng.read(&mut out, 0, t, 0.0);
        }
        let h = stats.snapshot();
        assert_eq!(h.underruns, 0, "{h:?}");
        h.target_frames
    }

    #[test]
    fn a_latency_floor_raises_the_target_at_once_within_the_growth_limit() {
        let c = cfg();
        assert_eq!(target_with_floor(None), c.base_target());
        assert_eq!(target_with_floor(Some(500.0)), c.base_target() + 500.0, "raised at once");
        let max = c.base_target() + c.max_growth() as f64;
        assert_eq!(target_with_floor(Some(5_000.0)), max, "never past the growth limit");
    }

    #[test]
    fn output_device_hears_silence_until_primed() {
        let (_eng, mut dev, stats) = soft_output(cfg()).unwrap();
        let mut buf = vec![1.0f32; 256];
        dev.read_interleaved(&mut buf, 0.0);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert_eq!(stats.snapshot().underruns, 0, "priming is not an underrun");
    }
}
