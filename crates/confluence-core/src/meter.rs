//! Level meters measured on the audio thread, read by the control side
//! (spec §4). Lock- and allocation-free on the audio side.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

pub struct MeterBank {
    peak: Box<[AtomicU32]>,
    rms: Box<[AtomicU32]>,
    clip: Box<[AtomicBool]>,
}

pub fn rms_coeff(block: usize, rate: f64, tau_s: f64) -> f32 {
    (1.0 - (-(block as f64) / (rate * tau_s)).exp()) as f32
}

impl MeterBank {
    pub fn new(channels: usize) -> Arc<MeterBank> {
        let atoms = |n| (0..n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>().into_boxed_slice();
        Arc::new(MeterBank {
            peak: atoms(channels),
            rms: atoms(channels),
            clip: (0..channels).map(|_| AtomicBool::new(false)).collect::<Vec<_>>().into_boxed_slice(),
        })
    }
    pub fn channels(&self) -> usize {
        self.peak.len()
    }

    /// One block of channel `ch`. `state` is the running mean square (kept by the caller).
    pub fn measure(&self, ch: usize, samples: &[f32], state: &mut f32, coeff: f32) {
        let (mut peak, mut sum) = (0.0f32, 0.0f32);
        for &s in samples {
            let a = s.abs();
            if a > peak {
                peak = a;
            } // NaN compares false: ignored
            if s.is_finite() {
                sum += s * s;
            }
        }
        let ms = if samples.is_empty() { 0.0 } else { sum / samples.len() as f32 };
        *state += coeff * (ms - *state);
        let Some(p) = self.peak.get(ch) else { return };
        // Non-negative floats order like their bits: a CAS max on the bits is a float max.
        let _ = p.fetch_max(peak.to_bits(), Ordering::Relaxed);
        self.rms[ch].store(state.max(0.0).sqrt().to_bits(), Ordering::Relaxed);
        if peak >= 1.0 {
            self.clip[ch].store(true, Ordering::Relaxed);
        }
    }
    pub fn take_peak(&self, ch: usize) -> f32 {
        self.peak.get(ch).map_or(0.0, |p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }
    pub fn rms(&self, ch: usize) -> f32 {
        self.rms.get(ch).map_or(0.0, |r| f32::from_bits(r.load(Ordering::Relaxed)))
    }
    pub fn clipped(&self, ch: usize) -> bool {
        self.clip.get(ch).is_some_and(|c| c.load(Ordering::Relaxed))
    }
    pub fn clear_clips(&self) {
        for c in self.clip.iter() {
            c.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_is_held_until_read_and_rms_follows_the_level() {
        let m = MeterBank::new(2);
        let mut st = 0.0f32;
        let c = rms_coeff(256, 48_000.0, 0.3);
        for _ in 0..2000 {
            m.measure(0, &[0.25; 256], &mut st, c);
        }
        assert!((m.take_peak(0) - 0.25).abs() < 1e-6);
        assert_eq!(m.take_peak(0), 0.0, "reset by the read");
        assert!((m.rms(0) - 0.25).abs() < 0.01, "rms of a constant 0.25 is 0.25: {}", m.rms(0));
        assert_eq!(m.take_peak(1), 0.0);
    }

    #[test]
    fn a_full_scale_sample_latches_the_clip_until_cleared() {
        let m = MeterBank::new(1);
        let mut st = 0.0;
        m.measure(0, &[0.1, -1.0, 0.1], &mut st, 0.1);
        assert!(m.clipped(0));
        m.measure(0, &[0.1; 3], &mut st, 0.1);
        assert!(m.clipped(0), "latched");
        m.clear_clips();
        assert!(!m.clipped(0));
    }

    #[test]
    fn negative_and_nan_samples_do_not_poison_the_peak() {
        let m = MeterBank::new(1);
        let mut st = 0.0;
        m.measure(0, &[-0.5, f32::NAN, 0.2], &mut st, 0.1);
        assert!((m.take_peak(0) - 0.5).abs() < 1e-6);
        assert!(m.rms(0).is_finite());
    }
}
