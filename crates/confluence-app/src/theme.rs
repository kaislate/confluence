//! Colours, and the gain → brightness mapping of matrix cells.

use eframe::egui::Color32;

/// The engine's gain range.
pub const GAIN_MIN_DB: f32 = -100.0;
pub const GAIN_MAX_DB: f32 = 24.0;
/// The range drawn as brightness and offered by the inspector's slider.
pub use confluence_api::taper::{SHOWN_MAX_DB, SHOWN_MIN_DB};

pub const ACCENT: Color32 = Color32::from_rgb(64, 156, 255);
pub const EMPTY_CELL: Color32 = Color32::from_gray(34);
pub const OFFLINE: Color32 = Color32::from_gray(90);
pub const WARN: Color32 = Color32::from_rgb(230, 170, 40);
pub const ERROR: Color32 = Color32::from_rgb(230, 70, 60);

/// The default device colours: the gear palette, so a device's bands match
/// its card on every finish.
pub const SLOT_COLORS: [Color32; 8] = crate::gear::skins::DEVICE_PALETTE;

pub use confluence_api::taper::{fader_db, fader_pos, FADER_KNEE, FADER_KNEE_DB};

/// Clamps to the engine's range; a non-number becomes 0 dB.
pub fn clamp_gain(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(GAIN_MIN_DB, GAIN_MAX_DB)
    } else {
        0.0
    }
}

/// Cell brightness: 0.55 at −60 dB (and below) to 1.0 at +12 dB (and
/// above), so quiet routes stay visible.
pub fn gain_brightness(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.55;
    }
    let t = ((db - SHOWN_MIN_DB) / (SHOWN_MAX_DB - SHOWN_MIN_DB)).clamp(0.0, 1.0);
    0.55 + 0.45 * t
}

/// `c` with its colour channels scaled by `brightness` (alpha kept).
pub fn scale(c: Color32, brightness: f32) -> Color32 {
    let ch = |v: u8| (f32::from(v) * brightness.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(ch(c.r()), ch(c.g()), ch(c.b()), c.a())
}

/// The fill of a routed cell in the built-in skin.
pub fn routed_color(db: f32) -> Color32 {
    scale(ACCENT, gain_brightness(db))
}

/// Each slot's colour, fixed by its id.
pub fn slot_color(id: u32) -> Color32 {
    SLOT_COLORS[id as usize % SLOT_COLORS.len()]
}

/// Amber from 70 % DSP load, red from 90 %.
pub fn dsp_color(load: f32) -> Option<Color32> {
    if load >= 0.9 {
        Some(ERROR)
    } else if load >= 0.7 {
        Some(WARN)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_spans_half_to_full_over_the_shown_range() {
        assert_eq!(gain_brightness(SHOWN_MIN_DB), 0.55);
        assert_eq!(gain_brightness(SHOWN_MAX_DB), 1.0);
        assert_eq!(gain_brightness(GAIN_MIN_DB), 0.55, "clamped below");
        assert_eq!(gain_brightness(GAIN_MAX_DB), 1.0, "clamped above");
        assert!(gain_brightness(-24.0) > gain_brightness(-30.0));
        assert_eq!(gain_brightness(f32::NAN), 0.55);
    }

    #[test]
    fn gains_are_clamped_to_the_engine_range() {
        assert_eq!(clamp_gain(30.0), GAIN_MAX_DB);
        assert_eq!(clamp_gain(-150.0), GAIN_MIN_DB);
        assert_eq!(clamp_gain(-6.5), -6.5);
        assert_eq!(clamp_gain(f32::NAN), 0.0);
    }

    #[test]
    fn slot_colours_are_fixed_per_id_and_differ_between_neighbours() {
        assert_eq!(slot_color(3), slot_color(3));
        assert_ne!(slot_color(1), slot_color(2));
    }

    #[test]
    fn dsp_load_turns_amber_then_red() {
        assert_eq!(dsp_color(0.5), None);
        assert_eq!(dsp_color(0.7), Some(WARN));
        assert_eq!(dsp_color(0.95), Some(ERROR));
    }

    #[test]
    fn the_fader_is_fine_near_zero_db_and_coarse_at_the_bottom() {
        assert_eq!(fader_db(0.0), SHOWN_MIN_DB);
        assert_eq!(fader_db(1.0), SHOWN_MAX_DB);
        assert_eq!(fader_db(FADER_KNEE), FADER_KNEE_DB);
        let zero = fader_pos(0.0);
        assert!((0.75..0.85).contains(&zero), "0 dB sits high on the travel: {zero}");
        for i in 0..=100 {
            let pos = i as f32 / 100.0;
            assert!((fader_pos(fader_db(pos)) - pos).abs() < 1e-4, "round trip at {pos}");
        }
        let near_top = fader_db(0.81) - fader_db(0.80);
        let near_bottom = fader_db(0.11) - fader_db(0.10);
        assert!(near_top > 0.0 && near_top < near_bottom / 2.0, "{near_top} vs {near_bottom} dB per step");
        assert_eq!(fader_pos(-100.0), 0.0, "below the travel: the bottom");
    }

    #[test]
    fn louder_routes_are_brighter() {
        let quiet = routed_color(-50.0);
        let loud = routed_color(6.0);
        assert!(loud.b() > quiet.b());
    }
}
