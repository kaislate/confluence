//! Clock-domain estimation: a delay-locked loop that filters device
//! timestamps into a sample-rate estimate, and a PI loop on ring fill level.

use std::f64::consts::{PI, SQRT_2};

/// Default rate-estimator loop bandwidth. Estimate noise scales with bandwidth:
/// real devices call back with a millisecond or two of jitter, which at
/// 0.05 Hz swung the estimate by ±16 ppm (±1 ms) and at 0.01 Hz by about ±1.
/// Real clock drift changes over minutes, so the slower loop loses nothing.
pub const DEFAULT_RATE_BANDWIDTH_HZ: f64 = 0.01;
/// Initial acquisition bandwidth of the rate estimator.
const ACQUIRE_BANDWIDTH_HZ: f64 = 2.0;
/// Stream time per halving of the acquisition bandwidth (2 Hz → 0.01 Hz in ~15 s).
const GEAR_STEP_S: f64 = 2.0;
/// Timestamp errors beyond this many update periods (and at least
/// `GAP_MIN_S`) are treated as a discontinuity rather than jitter.
const GAP_BLOCKS: f64 = 4.0;
const GAP_MIN_S: f64 = 0.02;

/// Second-order delay-locked loop (Adriaensen, "Using a DLL to filter time", 2005),
/// generalized to variable frame counts per update.
///
/// Feed it `(frames transferred since the previous update, time of this update)`
/// pairs, with time in seconds from any monotonic clock (QPC, simulated time).
#[derive(Clone, Debug)]
pub struct RateEstimator {
    nominal_rate: f64,
    bandwidth_hz: f64,
    /// Current loop bandwidth: starts wide for fast acquisition, halves every
    /// `GEAR_STEP_S` seconds of stream time until it reaches `bandwidth_hz`.
    current_bw: f64,
    elapsed: f64,
    /// Filtered time of the last update.
    t_last: f64,
    /// Estimated seconds per frame.
    period: f64,
    started: bool,
    updates: u64,
    gaps: u64,
}

impl RateEstimator {
    /// `bandwidth_hz` sets the loop bandwidth; 0.05–1 Hz suits audio device clocks.
    pub fn new(nominal_rate: f64, bandwidth_hz: f64) -> Self {
        Self {
            nominal_rate,
            bandwidth_hz,
            current_bw: ACQUIRE_BANDWIDTH_HZ.max(bandwidth_hz),
            elapsed: 0.0,
            t_last: 0.0,
            period: 1.0 / nominal_rate,
            started: false,
            updates: 0,
            gaps: 0,
        }
    }

    /// Restarts estimation from the nominal rate (e.g. after an xrun or device restart).
    pub fn reset(&mut self) {
        *self = Self::new(self.nominal_rate, self.bandwidth_hz);
    }

    pub fn update(&mut self, frames: u32, time: f64) {
        self.updates += 1;
        if !self.started || frames == 0 {
            self.t_last = time;
            self.started = true;
            return;
        }
        let n = frames as f64;
        self.elapsed += n * self.period;
        self.current_bw = (ACQUIRE_BANDWIDTH_HZ * 0.5f64.powf(self.elapsed / GEAR_STEP_S)).max(self.bandwidth_hz);
        let omega = 2.0 * PI * self.current_bw * n * self.period;
        let (b, c) = (SQRT_2 * omega, omega * omega);
        let predicted = self.t_last + self.period * n;
        let err = time - predicted;
        if err.abs() > (GAP_BLOCKS * n * self.period).max(GAP_MIN_S) {
            // A stall, device restart or clock jump, not jitter: re-anchor on
            // this timestamp and keep the rate learned so far.
            self.t_last = time;
            self.gaps += 1;
            return;
        }
        self.t_last = predicted + b * err;
        self.period += c * err / n;
    }

    /// Estimated frames per second.
    pub fn rate(&self) -> f64 {
        1.0 / self.period
    }

    /// Estimated deviation from nominal, in parts per million.
    pub fn ppm(&self) -> f64 {
        (self.rate() / self.nominal_rate - 1.0) * 1e6
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Number of discontinuities (stalls, restarts) detected.
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// True once acquisition is over and the loop runs at its final bandwidth.
    pub fn is_settled(&self) -> bool {
        self.current_bw <= self.bandwidth_hz
    }
}

/// PI controller that turns ring fill error into a resampling-ratio correction
/// in ppm. Positive output means "consume faster" (the ring is too full).
#[derive(Clone, Debug)]
pub struct FillController {
    kp: f64,
    ki: f64,
    limit_ppm: f64,
    slew_ppm_per_s: f64,
    integral: f64,
    output: f64,
    slew_enabled: bool,
}

impl FillController {
    /// Gains derived for a closed-loop bandwidth of `bandwidth_hz` at `sample_rate`.
    pub fn new(sample_rate: f64, bandwidth_hz: f64, limit_ppm: f64, slew_ppm_per_s: f64) -> Self {
        // Plant: d(fill)/dt = -sample_rate * 1e-6 * u  (u in ppm).
        let wc = 2.0 * PI * bandwidth_hz;
        let kp = wc / (sample_rate * 1e-6);
        let ki = kp * wc / 4.0;
        Self { kp, ki, limit_ppm, slew_ppm_per_s, integral: 0.0, output: 0.0, slew_enabled: false }
    }

    /// Spec defaults: 0.05 Hz, ±1000 ppm, 10 ppm/s once locked.
    pub fn with_defaults(sample_rate: f64) -> Self {
        Self::new(sample_rate, 0.05, 1000.0, 10.0)
    }

    /// Turns on the slew limit; call once the stream has settled.
    pub fn lock(&mut self) {
        self.slew_enabled = true;
    }

    /// Turns the slew limit off again (e.g. after a large disturbance).
    pub fn unlock(&mut self) {
        self.slew_enabled = false;
    }

    pub fn is_locked(&self) -> bool {
        self.slew_enabled
    }

    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.output = 0.0;
        self.slew_enabled = false;
    }

    /// `error_frames` = measured fill âˆ’ target fill; `dt` = seconds since last update.
    pub fn update(&mut self, error_frames: f64, dt: f64) -> f64 {
        let i_limit = self.limit_ppm / self.ki;
        self.integral = (self.integral + error_frames * dt).clamp(-i_limit, i_limit);
        let desired = (self.kp * error_frames + self.ki * self.integral).clamp(-self.limit_ppm, self.limit_ppm);
        if self.slew_enabled {
            let max = self.slew_ppm_per_s * dt;
            let limited = self.output + (desired - self.output).clamp(-max, max);
            if limited != desired {
                // Back-calculation anti-windup: make the integral consistent
                // with the output actually applied.
                self.integral = ((limited - self.kp * error_frames) / self.ki).clamp(-i_limit, i_limit);
            }
            self.output = limited;
        } else {
            self.output = desired;
        }
        self.output
    }

    pub fn output(&self) -> f64 {
        self.output
    }

    /// True while the output is pinned at (or within 10% of) its limit.
    pub fn is_saturated(&self) -> bool {
        self.output.abs() >= 0.9 * self.limit_ppm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift noise in [-1, 1).
    fn noise(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    #[test]
    fn estimator_tracks_drift_through_jitter() {
        let true_rate = 48_000.0 * (1.0 + 150e-6);
        let mut est = RateEstimator::new(48_000.0, DEFAULT_RATE_BANDWIDTH_HZ);
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let block = 128u32;
        let mut frames = 0u64;
        // 120 s of callbacks with ±0.2 ms timestamp jitter.
        while (frames as f64) < true_rate * 120.0 {
            frames += block as u64;
            let t = frames as f64 / true_rate + 0.0002 * noise(&mut rng);
            est.update(block, t);
        }
        assert!((est.ppm() - 150.0).abs() < 3.0, "estimated {} ppm", est.ppm());
    }

    #[test]
    fn estimator_acquires_quickly_then_settles() {
        let true_rate = 48_000.0 * (1.0 - 400e-6);
        let mut est = RateEstimator::new(48_000.0, DEFAULT_RATE_BANDWIDTH_HZ);
        let mut frames = 0u64;
        while (frames as f64) < true_rate * 5.0 {
            frames += 256;
            est.update(256, frames as f64 / true_rate);
        }
        assert!((est.ppm() + 400.0).abs() < 20.0, "after 5 s: {} ppm", est.ppm());
        assert!(!est.is_settled());
        while (frames as f64) < true_rate * 17.0 {
            frames += 256;
            est.update(256, frames as f64 / true_rate);
        }
        assert!(est.is_settled(), "settled by 17 s (2 Hz to 0.01 Hz, halving every 2 s)");
    }

    /// Real devices call back with a millisecond or two of timing jitter
    /// (USB scheduling, system load). The fed-forward drift estimate must stay
    /// steady through it: its noise moves every bridge's resampling ratio.
    #[test]
    fn the_rate_estimate_is_steady_under_callback_jitter() {
        let mut est = RateEstimator::new(48_000.0, DEFAULT_RATE_BANDWIDTH_HZ);
        let true_ppm = 20.0;
        let period = 512.0 / (48_000.0 * (1.0 + true_ppm * 1e-6));
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for n in 1..(180.0 / period) as u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let jitter = ((seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * 0.001; // ±1 ms
            est.update(512, n as f64 * period + jitter);
            if n as f64 * period > 90.0 {
                lo = lo.min(est.ppm());
                hi = hi.max(est.ppm());
            }
        }
        assert!(hi - lo < 5.0, "estimate wanders {:.1} ppm (from {lo:+.1} to {hi:+.1})", hi - lo);
        assert!((lo - true_ppm).abs() < 3.0 && (hi - true_ppm).abs() < 3.0, "[{lo:+.1}, {hi:+.1}] vs {true_ppm}");
    }

    #[test]
    fn a_stall_re_anchors_instead_of_corrupting_the_rate() {
        let rate = 48_000.0 * (1.0 + 100e-6);
        let mut est = RateEstimator::new(48_000.0, DEFAULT_RATE_BANDWIDTH_HZ);
        let mut frames = 0u64;
        let mut t_offset = 0.0;
        while (frames as f64) < rate * 60.0 {
            frames += 128;
            if frames as f64 > rate * 30.0 && t_offset == 0.0 {
                t_offset = 1.0; // the device disappears for one second
            }
            est.update(128, frames as f64 / rate + t_offset);
        }
        assert_eq!(est.gaps(), 1);
        assert!((est.ppm() - 100.0).abs() < 3.0, "{} ppm", est.ppm());
    }

    #[test]
    fn controller_is_proportional_and_clamped() {
        let mut c = FillController::with_defaults(48_000.0);
        let small = c.update(10.0, 0.0);
        assert!(small > 0.0);
        c.reset();
        assert_eq!(c.update(1e9, 0.0), 1000.0);
        c.reset();
        assert_eq!(c.update(-1e9, 0.0), -1000.0);
    }

    #[test]
    fn slew_limit_applies_after_lock() {
        let mut c = FillController::with_defaults(48_000.0);
        c.lock();
        let out = c.update(1e9, 0.1);
        assert!((out - 1.0).abs() < 1e-9, "10 ppm/s * 0.1 s = 1 ppm, got {out}");
    }
}
