//! dB/linear conversion and per-point parameters.

/// Gains at or below this are treated as silence (−∞ dB).
pub const MIN_DB: f32 = -100.0;
/// Maximum gain a point may apply.
pub const MAX_DB: f32 = 24.0;

/// Converts decibels to a linear coefficient, clamping to `MAX_DB` and
/// mapping anything at or below `MIN_DB` (including NaN) to 0.0.
pub fn db_to_lin(db: f32) -> f32 {
    if db.is_nan() || db <= MIN_DB {
        0.0
    } else {
        10f32.powf(db.min(MAX_DB) / 20.0)
    }
}

/// User-facing state of one matrix point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointParams {
    pub gain_db: f32,
    pub mute: bool,
    pub invert: bool,
}

impl Default for PointParams {
    fn default() -> Self {
        Self { gain_db: 0.0, mute: false, invert: false }
    }
}

impl PointParams {
    /// Mute and phase folded into one signed coefficient: the audio thread sees one number.
    pub fn effective_gain(&self) -> f32 {
        if self.mute {
            0.0
        } else if self.invert {
            -db_to_lin(self.gain_db)
        } else {
            db_to_lin(self.gain_db)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_and_minus_six() {
        assert_eq!(db_to_lin(0.0), 1.0);
        assert!((db_to_lin(-6.0206) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn floor_and_ceiling() {
        assert_eq!(db_to_lin(-100.0), 0.0);
        assert_eq!(db_to_lin(-500.0), 0.0);
        assert_eq!(db_to_lin(f32::NAN), 0.0);
        assert!((db_to_lin(100.0) - db_to_lin(24.0)).abs() < 1e-6);
    }

    #[test]
    fn mute_and_invert_fold_into_gain() {
        let p = PointParams { gain_db: -6.0206, mute: false, invert: true };
        assert!((p.effective_gain() + 0.5).abs() < 1e-4);
        let m = PointParams { mute: true, ..p };
        assert_eq!(m.effective_gain(), 0.0);
    }
}
