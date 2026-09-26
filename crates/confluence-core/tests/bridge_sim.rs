//! Simulated-time tests: a drifting device and the master engine exchange a
//! sine through a bridge. After settling there must be no xruns and no
//! discontinuities (a dropped or repeated sample shows up as a large
//! second difference).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::needless_range_loop)]

use std::f64::consts::TAU;

use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, soft_output, BridgeConfig};
use confluence_core::buffer::PlanarBuffer;

const MASTER_RATE: f64 = 48_000.0;
const TONE_HZ: f64 = 997.0;
const SETTLE_S: f64 = 30.0;

fn config(device_rate: f64, device_block: usize, master_block: usize) -> BridgeConfig {
    BridgeConfig {
        channels: 2,
        device_rate,
        device_block,
        master_rate: MASTER_RATE,
        master_block,
        quality: AsrcQuality::Sinc64,
        margin_frames: 24,
    }
}

/// Deterministic jitter in [-amp, amp).
struct Jitter(u64, f64);
impl Jitter {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * self.1
    }
}

/// Tracks the largest |x[n+1] - 2x[n] + x[n-1]| once `armed`.
#[derive(Default)]
struct Continuity {
    prev: [f32; 2],
    max: f32,
    armed: bool,
}
impl Continuity {
    fn push(&mut self, x: f32) {
        if self.armed {
            self.max = self.max.max((x - 2.0 * self.prev[1] + self.prev[0]).abs());
        }
        self.prev = [self.prev[1], x];
    }
}

/// A clean 997 Hz sine at amplitude 0.5 has second differences of at most
/// 0.5 * (2π·997/48000)² ≈ 0.0085.
const MAX_SECOND_DIFF: f32 = 0.02;

fn run_input(device_rate: f64, device_ppm: f64, device_block: usize, master_block: usize, seconds: f64) {
    let cfg = config(device_rate, device_block, master_block);
    let (mut dev, mut eng, stats) = soft_input(cfg).unwrap();
    let dev_rate = cfg.device_rate * (1.0 + device_ppm * 1e-6);
    let mut jitter = Jitter(0x2545_F491_4F6C_DD1D, 0.0002);
    let mut out = PlanarBuffer::new(2, master_block);
    let mut dev_buf = vec![0.0f32; device_block * 2];
    let (mut dev_frames, mut master_blocks) = (0u64, 0u64);
    let mut cont = Continuity::default();
    let mut xruns_at_settle = None;
    loop {
        let t_dev = (dev_frames + device_block as u64) as f64 / dev_rate;
        let t_master = (master_blocks + 1) as f64 * master_block as f64 / MASTER_RATE;
        if t_master > seconds {
            break;
        }
        if t_dev <= t_master {
            for n in 0..device_block {
                let s = (0.5 * (TAU * TONE_HZ * (dev_frames + n as u64) as f64 / dev_rate).sin()) as f32;
                dev_buf[2 * n] = s;
                dev_buf[2 * n + 1] = -s;
            }
            dev.write_interleaved(&dev_buf, t_dev + jitter.next());
            dev_frames += device_block as u64;
        } else {
            eng.read(&mut out, 0, t_master, 0.0);
            master_blocks += 1;
            if t_master >= SETTLE_S && xruns_at_settle.is_none() {
                let h = stats.snapshot();
                xruns_at_settle = Some(h.underruns + h.overruns);
                cont.armed = true;
            }
            for &s in out.channel(0) {
                cont.push(s);
            }
        }
    }
    let h = stats.snapshot();
    assert_eq!(xruns_at_settle, Some(0), "xruns while settling: {h:?}");
    assert_eq!(h.underruns + h.overruns, 0, "xruns after settling: {h:?}");
    assert!(cont.max < MAX_SECOND_DIFF, "discontinuity {} ({h:?})", cont.max);
    assert!((h.device_ppm - device_ppm).abs() < 5.0, "ppm estimate {h:?}");
    assert!((h.fill_frames - h.target_frames).abs() < device_block as f64 + master_block as f64, "{h:?}");
}

fn run_output(device_rate: f64, device_ppm: f64, device_block: usize, master_block: usize, seconds: f64) {
    let cfg = config(device_rate, device_block, master_block);
    let (mut eng, mut dev, stats) = soft_output(cfg).unwrap();
    let dev_rate = cfg.device_rate * (1.0 + device_ppm * 1e-6);
    let mut jitter = Jitter(0x9E37_79B9_7F4A_7C15, 0.0002);
    let mut block = PlanarBuffer::new(2, master_block);
    let mut dev_buf = vec![0.0f32; device_block * 2];
    let (mut dev_frames, mut master_blocks) = (0u64, 0u64);
    let mut cont = Continuity::default();
    let mut xruns_at_settle = None;
    loop {
        let t_dev = (dev_frames + device_block as u64) as f64 / dev_rate;
        let t_master = (master_blocks + 1) as f64 * master_block as f64 / MASTER_RATE;
        if t_master > seconds {
            break;
        }
        if t_master <= t_dev {
            let base = master_blocks * master_block as u64;
            for n in 0..master_block {
                let s = (0.5 * (TAU * TONE_HZ * (base + n as u64) as f64 / MASTER_RATE).sin()) as f32;
                block.channel_mut(0)[n] = s;
                block.channel_mut(1)[n] = -s;
            }
            eng.write(&block, 0, t_master, 0.0);
            master_blocks += 1;
        } else {
            dev.read_interleaved(&mut dev_buf, t_dev + jitter.next());
            dev_frames += device_block as u64;
            if t_dev >= SETTLE_S && xruns_at_settle.is_none() {
                let h = stats.snapshot();
                xruns_at_settle = Some(h.underruns + h.overruns);
                cont.armed = true;
            }
            for n in 0..device_block {
                cont.push(dev_buf[2 * n]);
            }
        }
    }
    let h = stats.snapshot();
    assert_eq!(xruns_at_settle, Some(0), "xruns while settling: {h:?}");
    assert_eq!(h.underruns + h.overruns, 0, "xruns after settling: {h:?}");
    assert!(cont.max < MAX_SECOND_DIFF, "discontinuity {} ({h:?})", cont.max);
    assert!((h.device_ppm - device_ppm).abs() < 5.0, "ppm estimate {h:?}");
}

#[test]
fn input_tracks_fast_device() {
    run_input(48_000.0, 300.0, 128, 256, 120.0);
}

#[test]
fn input_tracks_slow_device_with_mismatched_blocks() {
    run_input(48_000.0, -500.0, 480, 128, 120.0);
}

#[test]
fn input_converts_44k1_device_with_drift() {
    run_input(44_100.0, 200.0, 441, 256, 120.0);
}

#[test]
fn output_tracks_fast_device() {
    run_output(48_000.0, 500.0, 128, 256, 120.0);
}

#[test]
fn output_tracks_slow_device() {
    run_output(48_000.0, -300.0, 441, 128, 120.0);
}

#[test]
fn output_converts_to_44k1_device_with_drift() {
    run_output(44_100.0, -200.0, 512, 128, 120.0);
}

/// A device that stops delivering for 1 s (unplug/replug) causes xruns only
/// during the gap; afterwards the bridge re-primes and runs clean again.
#[test]
fn input_recovers_after_device_stall() {
    let (master_block, device_block) = (256usize, 128usize);
    let cfg = config(48_000.0, device_block, master_block);
    let (mut dev, mut eng, stats) = soft_input(cfg).unwrap();
    let dev_rate = 48_000.0 * (1.0 + 100e-6);
    let mut out = PlanarBuffer::new(2, master_block);
    let mut dev_buf = vec![0.0f32; device_block * 2];
    let (mut dev_frames, mut master_blocks) = (0u64, 0u64);
    let mut cont = Continuity::default();
    let mut xruns_before_stall = None;
    let mut xruns_after_recovery = None;
    let mut silent_during_stall = true;
    loop {
        let t_dev = (dev_frames + device_block as u64) as f64 / dev_rate;
        let t_master = (master_blocks + 1) as f64 * master_block as f64 / MASTER_RATE;
        if t_master > 120.0 {
            break;
        }
        if t_dev <= t_master {
            let stalled = (40.0..41.0).contains(&t_dev);
            if !stalled {
                for n in 0..device_block {
                    let s = (0.5 * (TAU * TONE_HZ * (dev_frames + n as u64) as f64 / dev_rate).sin()) as f32;
                    dev_buf[2 * n] = s;
                    dev_buf[2 * n + 1] = -s;
                }
                dev.write_interleaved(&dev_buf, t_dev);
            }
            dev_frames += device_block as u64;
        } else {
            eng.read(&mut out, 0, t_master, 0.0);
            master_blocks += 1;
            let h = stats.snapshot();
            if t_master < 40.0 {
                xruns_before_stall = Some(h.underruns + h.overruns);
            }
            if (40.2..40.9).contains(&t_master) && out.channel(0).iter().any(|&s| s != 0.0) {
                silent_during_stall = false;
            }
            if t_master >= 70.0 && xruns_after_recovery.is_none() {
                xruns_after_recovery = Some(h.underruns + h.overruns);
                cont.armed = true;
            }
            for &s in out.channel(0) {
                cont.push(s);
            }
        }
    }
    let h = stats.snapshot();
    assert_eq!(xruns_before_stall, Some(0), "{h:?}");
    assert!(xruns_after_recovery.unwrap() >= 1, "the stall was noticed: {h:?}");
    assert_eq!(h.underruns + h.overruns, xruns_after_recovery.unwrap(), "clean after recovery: {h:?}");
    assert!(silent_during_stall, "outputs silence, not garbage, while the device is gone");
    assert!(cont.max < MAX_SECOND_DIFF, "discontinuity {} after recovery", cont.max);
    assert!((h.device_ppm - 100.0).abs() < 5.0, "rate estimate recovered: {h:?}");
}
