//! Clock-domain bridges between a soft-clocked device and the master-clocked engine.
//!
//! Each bridge is split into a device side (called from the device's own
//! callback thread) and an engine side (called once per master block). Audio
//! crosses in an interleaved SPSC ring; device timestamps cross in a second
//! ring and feed a [`RateEstimator`]. A [`FillController`] trims the resampling
//! ratio so the ring stays near its target fill.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::asrc::{Asrc, AsrcError, AsrcQuality, FixedSide};
use crate::buffer::PlanarBuffer;
use crate::clock::{FillController, RateEstimator, DEFAULT_RATE_BANDWIDTH_HZ};

/// Minimum running time after (re)start before the fill controller's slew
/// limit may engage. It also needs a settled rate estimate and a small fill error.
const LOCK_AFTER_S: f64 = 5.0;
/// Clean running time after which an enlarged target shrinks one step.
const DECAY_EVERY_S: f64 = 10.0;
/// Upper bound on adaptive target growth, in device blocks above the base target.
const MAX_EXTRA_BLOCKS: f64 = 8.0;
const STAMP_QUEUE: usize = 256;

/// (frames transferred since the previous callback, callback time in seconds).
type Stamp = (u32, f64);
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
    /// Safety margin added to the base target fill (spec default 0.5 ms).
    pub margin_frames: usize,
}

impl BridgeConfig {
    /// Base target: one device block + one master block + margin.
    pub fn base_target(&self) -> f64 {
        (self.device_block + self.master_block + self.margin_frames) as f64
    }

    fn ring_frames(&self) -> usize {
        (self.base_target() as usize + self.device_block) * 2
            + MAX_EXTRA_BLOCKS as usize * self.device_block
            + self.master_block * 4
    }
}

/// Health counters shared between both sides and the control thread.
#[derive(Default)]
pub struct BridgeStats {
    underruns: AtomicU64,
    overruns: AtomicU64,
    fill_bits: AtomicU64,
    target_bits: AtomicU64,
    device_ppm_bits: AtomicU64,
    correction_ppm_bits: AtomicU64,
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
    run_time: f64,
    clean_time: f64,
    /// Time of the newest device timestamp, if any.
    last_stamp: Option<f64>,
    /// Low-pass filtered fill; `None` until the first measurement after (re)start.
    fill_filtered: Option<f64>,
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
            run_time: 0.0,
            clean_time: 0.0,
            last_stamp: None,
            fill_filtered: None,
            stats,
        }
    }

    fn dt(&self) -> f64 {
        self.cfg.master_block as f64 / self.cfg.master_rate
    }

    fn target(&self) -> f64 {
        self.fixed_target.unwrap_or(self.target)
    }

    fn drain_stamps(&mut self) {
        while let Ok((frames, time)) = self.stamps.pop() {
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
    fn correction(&mut self, fill: f64) -> f64 {
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
        let block = self.cfg.device_block as f64;
        if self.ctl.is_locked() && err.abs() > block {
            self.ctl.unlock();
        } else if !self.ctl.is_locked()
            && self.run_time >= LOCK_AFTER_S
            && self.device_est.is_settled()
            && err.abs() < block / 4.0
        {
            self.ctl.lock();
        }
        let corr = self.ctl.update(err, dt);
        self.run_time += dt;
        self.clean_time += dt;
        if self.clean_time >= DECAY_EVERY_S {
            self.clean_time = 0.0;
            let base = self.cfg.base_target();
            self.target = (self.target - self.cfg.device_block as f64 / 4.0).max(base);
        }
        self.stats.publish(fill, self.target(), self.device_est.ppm(), corr);
        corr
    }

    /// Records an xrun: stop, raise the target by one device block, restart the controller.
    fn xrun(&mut self) {
        self.running = false;
        self.run_time = 0.0;
        self.clean_time = 0.0;
        self.fill_filtered = None;
        self.ctl.reset();
        let max = self.cfg.base_target() + MAX_EXTRA_BLOCKS * self.cfg.device_block as f64;
        self.target = (self.target + self.cfg.device_block as f64).min(max);
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
            let excess = avail as f64 + t.frames_since_stamp(now) - t.target();
            if excess < 0.0 {
                silence(out, first_channel, ch);
                return;
            }
            // Start exactly at the target: drop what accumulated while priming
            // (the output was silent anyway), so the loop starts without error.
            let drop = (excess as usize).min(avail);
            if let Ok(chunk) = self.samples.read_chunk(drop * ch) {
                chunk.commit_all();
                avail -= drop;
            }
            t.running = true;
        }
        let fill = avail as f64 + t.frames_since_stamp(now);
        let corr = t.correction(fill);
        let rel = (1.0 + master_ppm * 1e-6) / (1.0 + t.device_est.ppm() * 1e-6) * (1.0 - corr * 1e-6);
        self.asrc.set_relative_ratio(rel);

        let need = self.asrc.input_frames_next();
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
        let ring = ((self.ring_slots - self.samples.slots()) / ch) as f64;
        t.running = true;
        let fill = ring - t.frames_since_stamp(now);
        let corr = t.correction(fill);
        let rel = (1.0 + t.device_est.ppm() * 1e-6) / (1.0 + master_ppm * 1e-6) * (1.0 - corr * 1e-6);
        self.asrc.set_relative_ratio(rel);

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
        let _ = self.stamps.push((frames as u32, time));
        let avail = self.samples.slots() / self.channels;
        if !self.primed {
            if avail < self.prime_frames {
                data.fill(0.0);
                return;
            }
            self.primed = true;
        }
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

    #[test]
    fn output_device_hears_silence_until_primed() {
        let (_eng, mut dev, stats) = soft_output(cfg()).unwrap();
        let mut buf = vec![1.0f32; 256];
        dev.read_interleaved(&mut buf, 0.0);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert_eq!(stats.snapshot().underruns, 0, "priming is not an underrun");
    }
}
