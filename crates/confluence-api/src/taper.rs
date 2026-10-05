//! The fader taper shared by the window's gain slider and MIDI controls: like
//! a mixing desk's fader, the upper three quarters of the travel cover
//! −30…+12 dB (fine steps around 0 dB), the bottom quarter −60…−30 dB.

/// The lowest gain the fader reaches (below is only silence).
pub const SHOWN_MIN_DB: f32 = -60.0;
/// The top of the fader.
pub const SHOWN_MAX_DB: f32 = 12.0;
/// The fader's knee: below this position the travel is compressed.
pub const FADER_KNEE: f32 = 0.25;
/// The gain at the knee.
pub const FADER_KNEE_DB: f32 = -30.0;
/// Silence, in dB.
pub const SILENT_DB: f32 = -100.0;

/// The gain at fader position `pos` (0..=1).
pub fn fader_db(pos: f32) -> f32 {
    let pos = pos.clamp(0.0, 1.0);
    if pos >= FADER_KNEE {
        FADER_KNEE_DB + (pos - FADER_KNEE) / (1.0 - FADER_KNEE) * (SHOWN_MAX_DB - FADER_KNEE_DB)
    } else {
        SHOWN_MIN_DB + pos / FADER_KNEE * (FADER_KNEE_DB - SHOWN_MIN_DB)
    }
}

/// The fader position of `db` (the inverse of [`fader_db`]; clamped to the travel).
pub fn fader_pos(db: f32) -> f32 {
    let db = if db.is_finite() { db.clamp(SHOWN_MIN_DB, SHOWN_MAX_DB) } else { 0.0 };
    if db >= FADER_KNEE_DB {
        FADER_KNEE + (db - FADER_KNEE_DB) / (SHOWN_MAX_DB - FADER_KNEE_DB) * (1.0 - FADER_KNEE)
    } else {
        (db - SHOWN_MIN_DB) / (FADER_KNEE_DB - SHOWN_MIN_DB) * FADER_KNEE
    }
}

/// The gain a 7-bit MIDI value (0..=127) sets: 0 is silence, 1..=127 run up
/// the fader.
pub fn cc_to_db(value: u8) -> f32 {
    match value.min(127) {
        0 => SILENT_DB,
        v => (fader_db(f32::from(v) / 127.0) * 10.0).round() / 10.0,
    }
}

/// The 7-bit MIDI value showing a gain (0 for silence or muted).
pub fn db_to_cc(db: f32, muted: bool) -> u8 {
    if muted || db <= SILENT_DB {
        return 0;
    }
    ((fader_pos(db) * 127.0).round() as u8).clamp(1, 127)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_values_follow_the_fader() {
        assert_eq!(cc_to_db(0), SILENT_DB, "0 is silence");
        assert_eq!(cc_to_db(127), SHOWN_MAX_DB);
        assert!((cc_to_db(32) - FADER_KNEE_DB).abs() < 1.0, "a quarter up is the knee: {}", cc_to_db(32));
        assert_eq!(db_to_cc(SILENT_DB, false), 0);
        assert_eq!(db_to_cc(0.0, true), 0, "muted shows as the bottom");
        assert_eq!(db_to_cc(SHOWN_MAX_DB, false), 127);
        for v in 1..=127u8 {
            assert_eq!(db_to_cc(cc_to_db(v), false), v, "round trip at {v}");
        }
    }
}
