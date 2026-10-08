//! The gear finishes: graphite (the default), silver, and candy (each device
//! moulded in its own colour). Panel values from the design study's mockup;
//! the ground, bed and accent tokens from the design review.

use eframe::egui::Color32;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Finish {
    #[default]
    Graphite,
    Silver,
    Candy,
}

impl Finish {
    pub fn all() -> [Finish; 3] {
        [Finish::Graphite, Finish::Silver, Finish::Candy]
    }

    pub fn name(self) -> &'static str {
        match self {
            Finish::Graphite => "Graphite",
            Finish::Silver => "Silver",
            Finish::Candy => "Candy",
        }
    }

    /// The finish called `name` (as [`name`](Self::name) gives it).
    pub fn from_name(name: &str) -> Option<Finish> {
        Finish::all().into_iter().find(|f| f.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// How a finish paints: the panel's gradient stops, ink and OLED colours,
/// the ground the panels sit on, and the strengths of the knob light and
/// shade and the panel's bevels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GearSkin {
    pub finish: Finish,
    /// Panel gradient: light corner, middle, dark corner.
    pub p1: Color32,
    pub p2: Color32,
    pub p3: Color32,
    /// Labels etched in the panel.
    pub ink: Color32,
    /// OLED text.
    pub oled: Color32,
    /// Knob highlight strength (proportional light).
    pub light_k: f32,
    /// Knob shade opacity (multiply).
    pub shade_k: f32,
    /// Panel bevel: highlight (top-left) and shadow (bottom-right) alphas.
    pub hl: f32,
    pub sh: f32,
    /// A machined band round the knob cap.
    pub band: bool,
    /// Panels take the device's colour.
    pub mould: bool,
    /// The rack's ground: screens, rails, the inspector.
    pub ground: Color32,
    /// Ink on the ground (the ground's own labels).
    pub ground_ink: Color32,
    /// The matrix bed and other recesses cut into the ground.
    pub bed: Color32,
    /// Trays cut into a panel (the meter tray).
    pub well: Color32,
    /// The lit tab, the selection ring.
    pub accent: Color32,
    /// Grain over the ground: alpha, and whether it is light (on dark) or dark.
    pub grain: f32,
    pub grain_light: bool,
    /// The light copy under etched text.
    pub etch: f32,
    /// The glass reflection on pills.
    pub glass: f32,
}

/// The colour candy panels take when no device colour is given (the first
/// of the device palette).
pub const CANDY_DEFAULT: Color32 = DEVICE_PALETTE[0];

/// The eight default device colours, ordered so neighbours separate under
/// deuteranopia; red is kept for faults. Candy moulds a device in its
/// colour, graphite and silver mark it with a dot, and the matrix bands take
/// the same one, so a device looks the same on every screen.
pub const DEVICE_PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x4f, 0x8f, 0xe6),
    Color32::from_rgb(0xe8, 0x96, 0x3a),
    Color32::from_rgb(0x2f, 0xbf, 0xa6),
    Color32::from_rgb(0xc8, 0x6b, 0xd1),
    Color32::from_rgb(0xe6, 0xc8, 0x4a),
    Color32::from_rgb(0x8f, 0x8f, 0xf0),
    Color32::from_rgb(0xe0, 0x6a, 0x8f),
    Color32::from_rgb(0x7c, 0xc0, 0x5a),
];

/// The default colour of the device with palette index `n` (its lowest
/// slot id, or its position's index when it has no slots yet).
pub fn palette_color(palette: &[Color32], n: u32) -> Color32 {
    if palette.is_empty() {
        CANDY_DEFAULT
    } else {
        palette[n as usize % palette.len()]
    }
}

pub const GREEN: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x64);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb0, 0x3a);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);
pub const OLED_AMBER: Color32 = Color32::from_rgb(0xff, 0xcf, 0x5a);
pub const OLED_CYAN: Color32 = Color32::from_rgb(0x8f, 0xf0, 0xff);

fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// `c` moved `amount` (0..1) toward white (positive) or black (negative).
pub fn shift(c: Color32, amount: f32) -> Color32 {
    let f = |v: u8| {
        let v = v as f32;
        let out = if amount >= 0.0 { v + (255.0 - v) * amount } else { v * (1.0 + amount) };
        out.round().clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// `c` with its saturation reduced by `amount` (0..1) toward its own grey.
pub fn desaturate(c: Color32, amount: f32) -> Color32 {
    let grey = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    let f = |v: u8| (v as f32 + (grey - v as f32) * amount.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Relative luminance (0..1) of an opaque colour.
pub fn luminance(c: Color32) -> f32 {
    let lin = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// Black or white, whichever reads better on `c`.
pub fn ink_on(c: Color32) -> Color32 {
    if luminance(c) > 0.36 {
        hex(0x1c1d21)
    } else {
        Color32::WHITE
    }
}

impl GearSkin {
    pub fn preset(finish: Finish) -> GearSkin {
        match finish {
            Finish::Graphite => GearSkin {
                finish,
                p1: hex(0x4a4c53),
                p2: hex(0x34363c),
                p3: hex(0x26272c),
                ink: hex(0xe8e8ec),
                oled: OLED_AMBER,
                light_k: 1.0,
                shade_k: 0.68,
                hl: 0.10,
                sh: 0.55,
                band: true,
                mould: false,
                ground: hex(0x1f2024),
                ground_ink: hex(0xd8d8de),
                bed: hex(0x17181b),
                well: hex(0x1e1f23),
                accent: hex(0xf3bf3d),
                grain: 0.035,
                grain_light: true,
                etch: 0.16,
                glass: 0.14,
            },
            Finish::Silver => GearSkin {
                finish,
                p1: hex(0xcaccce),
                p2: hex(0xbbbdbf),
                p3: hex(0xaeafb1),
                ink: hex(0x3a3c40),
                oled: OLED_AMBER,
                light_k: 0.62,
                shade_k: 0.68,
                hl: 0.85,
                sh: 0.28,
                band: false,
                mould: false,
                ground: hex(0xa7a9ad),
                ground_ink: hex(0x303236),
                bed: hex(0x9c9ea1),
                well: hex(0xa2a4a7),
                accent: hex(0x2f6fd6),
                grain: 0.03,
                grain_light: false,
                etch: 0.55,
                glass: 0.08,
            },
            Finish::Candy => GearSkin::moulded(CANDY_DEFAULT),
        }
    }

    /// The candy finish moulded in `color`.
    pub fn moulded(color: Color32) -> GearSkin {
        GearSkin {
            finish: Finish::Candy,
            p1: shift(color, 0.25),
            p2: color,
            p3: shift(color, -0.25),
            ink: shift(color, -0.72),
            oled: OLED_AMBER,
            light_k: 0.62,
            shade_k: 0.68,
            hl: 0.45,
            sh: 0.22,
            band: false,
            mould: true,
            ground: hex(0xefe9e4),
            ground_ink: hex(0x3d3d42),
            bed: hex(0xd8d2cf),
            well: shift(color, -0.38),
            accent: hex(0x3d3d42),
            grain: 0.025,
            grain_light: false,
            etch: 0.45,
            glass: 0.10,
        }
    }

    /// This finish for a device of `color`: candy panels take the colour.
    pub fn for_device(self, color: Option<Color32>) -> GearSkin {
        match (self.mould, color) {
            (true, Some(c)) => GearSkin::moulded(c),
            _ => self,
        }
    }

    /// This finish with the power off: the panel desaturated and darkened,
    /// the ink dimmed.
    pub fn powered_off(self) -> GearSkin {
        let off = |c: Color32| shift(desaturate(c, 0.6), -0.18);
        GearSkin {
            p1: off(self.p1),
            p2: off(self.p2),
            p3: off(self.p3),
            ink: self.ink.gamma_multiply(0.55),
            well: off(self.well),
            hl: self.hl * 0.6,
            ..self
        }
    }

    /// Whether the finish is light (ink is dark).
    pub fn light(&self) -> bool {
        luminance(self.ground) > 0.3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_carry_the_settled_strengths() {
        assert_eq!(GearSkin::preset(Finish::Graphite).light_k, 1.0);
        assert_eq!(GearSkin::preset(Finish::Silver).light_k, 0.62);
        assert_eq!(GearSkin::preset(Finish::Candy).light_k, 0.62);
        for f in Finish::all() {
            assert_eq!(GearSkin::preset(f).shade_k, 0.68);
        }
        assert_eq!(Finish::default(), Finish::Graphite);
    }

    #[test]
    fn candy_sits_on_a_warm_off_white_and_moulds_each_device() {
        let candy = GearSkin::preset(Finish::Candy);
        assert!(candy.light());
        assert!(candy.ground.r() > candy.ground.b(), "warm: more red than blue");
        let blue = candy.for_device(Some(DEVICE_PALETTE[0]));
        assert_eq!(blue.p2, DEVICE_PALETTE[0]);
        assert_eq!(blue.ground, candy.ground, "the ground does not take the device colour");
        assert_eq!(GearSkin::preset(Finish::Graphite).for_device(Some(DEVICE_PALETTE[1])).p2, hex(0x34363c));
    }

    #[test]
    fn the_palette_cycles_and_its_neighbours_differ() {
        assert_eq!(palette_color(&DEVICE_PALETTE, 8), DEVICE_PALETTE[0]);
        for w in DEVICE_PALETTE.windows(2) {
            assert_ne!(w[0], w[1]);
        }
        assert_eq!(palette_color(&[], 3), CANDY_DEFAULT);
    }

    #[test]
    fn ink_is_chosen_by_luminance() {
        assert_eq!(ink_on(Color32::from_rgb(0xe6, 0xc8, 0x4a)), hex(0x1c1d21), "dark on yellow");
        assert_eq!(ink_on(Color32::from_rgb(0x2f, 0x6f, 0xd6)), Color32::WHITE, "white on blue");
        assert!((luminance(Color32::WHITE) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn powered_off_is_greyer_and_darker() {
        let on = GearSkin::preset(Finish::Candy).for_device(Some(DEVICE_PALETTE[6]));
        let off = on.powered_off();
        assert!(luminance(off.p2) < luminance(on.p2));
        let sat = |c: Color32| c.r().max(c.g()).max(c.b()) - c.r().min(c.g()).min(c.b());
        assert!(sat(off.p2) < sat(on.p2) / 2);
    }
}
