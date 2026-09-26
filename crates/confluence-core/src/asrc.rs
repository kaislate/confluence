//! Asynchronous sample-rate converter with a continuously adjustable ratio.
//! Wraps `rubato` behind a crate-local type so the backend can be replaced.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, PolynomialDegree, Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Largest relative ratio change supported (±1 %, far beyond real clock drift).
pub const MAX_RELATIVE_RATIO: f64 = 1.01;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AsrcQuality {
    /// 4-point polynomial: ~2 samples delay, for talkback.
    Cubic,
    /// 64-tap windowed sinc: the default.
    #[default]
    Sinc64,
    /// 256-tap windowed sinc: critical listening.
    Sinc256,
}

/// Which side of the converter has a fixed block size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedSide {
    Input,
    Output,
}

#[derive(Debug)]
pub struct AsrcError(pub String);

impl std::fmt::Display for AsrcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "asrc: {}", self.0)
    }
}

impl std::error::Error for AsrcError {}

pub struct Asrc {
    inner: Async<f32>,
    channels: usize,
}

impl Asrc {
    /// `nominal_ratio` = output rate / input rate. `chunk` is the fixed side's block size.
    pub fn new(
        quality: AsrcQuality,
        nominal_ratio: f64,
        chunk: usize,
        channels: usize,
        fixed: FixedSide,
    ) -> Result<Self, AsrcError> {
        let fixed = match fixed {
            FixedSide::Input => FixedAsync::Input,
            FixedSide::Output => FixedAsync::Output,
        };
        let inner = match quality {
            AsrcQuality::Cubic => {
                Async::new_poly(nominal_ratio, MAX_RELATIVE_RATIO, PolynomialDegree::Cubic, chunk, channels, fixed)
            }
            AsrcQuality::Sinc64 | AsrcQuality::Sinc256 => {
                let len = if quality == AsrcQuality::Sinc64 { 64 } else { 256 };
                let mut params = SincInterpolationParameters::new(len, WindowFunction::BlackmanHarris2);
                params.interpolation = SincInterpolationType::Linear;
                params.oversampling_factor = 256;
                Async::new_sinc(nominal_ratio, MAX_RELATIVE_RATIO, &params, chunk, channels, fixed)
            }
        }
        .map_err(|e| AsrcError(e.to_string()))?;
        Ok(Self { inner, channels })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Sets the ratio relative to the nominal one (1.0 = nominal), clamped to the supported range.
    pub fn set_relative_ratio(&mut self, relative: f64) {
        let r = relative.clamp(1.0 / MAX_RELATIVE_RATIO, MAX_RELATIVE_RATIO);
        if let Some(adj) = self.inner.as_adjustable() {
            // Only fails outside the range we just clamped to.
            let _ = adj.set_resample_ratio_relative(r, true);
        }
    }

    pub fn input_frames_next(&self) -> usize {
        self.inner.input_frames_next()
    }

    pub fn output_frames_next(&self) -> usize {
        self.inner.output_frames_next()
    }

    pub fn input_frames_max(&self) -> usize {
        self.inner.input_frames_max()
    }

    pub fn output_frames_max(&self) -> usize {
        self.inner.output_frames_max()
    }

    /// Converter delay in output frames.
    pub fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }

    /// Converts `input_frames_next()` frames from `input` (one Vec per channel)
    /// into `output`. Returns (frames consumed, frames produced). Does not allocate.
    pub fn process(&mut self, input: &[Vec<f32>], output: &mut [Vec<f32>]) -> Result<(usize, usize), AsrcError> {
        let need_in = self.inner.input_frames_next();
        let need_out = self.inner.output_frames_next();
        let inp = SequentialSliceOfVecs::new(input, self.channels, need_in).map_err(|e| AsrcError(e.to_string()))?;
        let mut out =
            SequentialSliceOfVecs::new_mut(output, self.channels, need_out).map_err(|e| AsrcError(e.to_string()))?;
        self.inner.process_into_buffer(&inp, &mut out, None).map_err(|e| AsrcError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_output_consumes_input_at_inverse_ratio() {
        let mut a = Asrc::new(AsrcQuality::Sinc64, 1.0, 256, 1, FixedSide::Output).unwrap();
        a.set_relative_ratio(0.999);
        let input = vec![vec![0.0f32; a.input_frames_max()]];
        let mut output = vec![vec![0.0f32; a.output_frames_max()]];
        let (mut consumed, mut produced) = (0usize, 0usize);
        for _ in 0..100 {
            let (i, o) = a.process(&input, &mut output).unwrap();
            consumed += i;
            produced += o;
        }
        assert_eq!(produced, 25_600);
        // 25 600 / 0.999 ≈ 25 625.6 input frames (plus start-up lookahead).
        let extra = consumed as i64 - 25_600;
        assert!((20..=40).contains(&extra), "consumed {consumed}");
    }

    #[test]
    fn sinc64_delay_is_under_a_millisecond_at_48k() {
        let a = Asrc::new(AsrcQuality::Sinc64, 1.0, 128, 2, FixedSide::Output).unwrap();
        assert!(a.output_delay() < 48, "delay {}", a.output_delay());
    }

    #[test]
    fn dc_passes_through_at_unity() {
        let mut a = Asrc::new(AsrcQuality::Sinc64, 1.0, 128, 1, FixedSide::Output).unwrap();
        let input = vec![vec![0.5f32; a.input_frames_max()]];
        let mut output = vec![vec![0.0f32; a.output_frames_max()]];
        for _ in 0..4 {
            a.process(&input, &mut output).unwrap();
        }
        assert!(output[0][..128].iter().all(|&s| (s - 0.5).abs() < 1e-3));
    }
}
