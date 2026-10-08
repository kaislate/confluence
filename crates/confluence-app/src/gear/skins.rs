//! The gear finishes: graphite (the default), silver, and candy (each device
//! moulded in its own colour). Values from the design study's mockup.

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
/// and the strengths of the knob light and shade and the panel's bevels.
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
}

/// The colour candy panels take when no device colour is given.
pub const CANDY_DEFAULT: Color32 = Color32::from_rgb(0xd9, 0x5f, 0x7f);

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

impl GearSkin {
    pub fn preset(finish: Finish) -> GearSkin {
        match finish {
            Finish::Graphite => GearSkin {
                finish,
                p1: hex(0x4a4c53),
                p2: hex(0x34363c),
                p3: hex(0x26272c),
                ink: hex(0xe8e8ec),
                oled: hex(0xffcf5a),
                light_k: 1.0,
                shade_k: 0.68,
                hl: 0.10,
                sh: 0.55,
                band: true,
                mould: false,
            },
            Finish::Silver => GearSkin {
                finish,
                p1: hex(0xcaccce),
                p2: hex(0xbbbdbf),
                p3: hex(0xaeafb1),
                ink: hex(0x3a3c40),
                oled: hex(0xffcf5a),
                light_k: 0.62,
                shade_k: 0.68,
                hl: 0.85,
                sh: 0.28,
                band: false,
                mould: false,
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
            oled: hex(0xffcf5a),
            light_k: 0.62,
            shade_k: 0.68,
            hl: 0.45,
            sh: 0.32,
            band: false,
            mould: true,
        }
    }

    /// This finish for a device of `color`: candy panels take the colour.
    pub fn for_device(self, color: Option<Color32>) -> GearSkin {
        match (self.mould, color) {
            (true, Some(c)) => GearSkin::moulded(c),
            _ => self,
        }
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
}
