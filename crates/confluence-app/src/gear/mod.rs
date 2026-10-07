//! The gear visual system: hardware-like panels, OLED wells, glass pills,
//! recessed trays, LEDs and computed S-slope knobs, in three finishes
//! (spec: slot model §5).

pub mod knob_maps;
pub mod paint;
pub mod skins;

use eframe::egui::{self, FontData, FontFamily};
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

/// OLED text (VT323) and etched labels (Space Grotesk), both under the SIL
/// Open Font License (see `assets/fonts`).
pub fn install_fonts(ctx: &egui::Context) {
    let fonts: [(&str, &'static [u8], &str); 3] = [
        ("VT323", include_bytes!("../../assets/fonts/VT323-Regular.ttf"), "oled"),
        ("SpaceGrotesk-Medium", include_bytes!("../../assets/fonts/SpaceGrotesk-Medium.ttf"), "label"),
        ("SpaceGrotesk-Bold", include_bytes!("../../assets/fonts/SpaceGrotesk-Bold.ttf"), "label-bold"),
    ];
    for (name, bytes, family) in fonts {
        ctx.add_font(FontInsert::new(
            name,
            FontData::from_static(bytes),
            vec![InsertFontFamily { family: FontFamily::Name(family.into()), priority: FontPriority::Highest }],
        ));
    }
}
