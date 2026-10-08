//! The gear visual system: hardware-like panels, OLED wells, glass pills,
//! recessed trays, LEDs and computed S-slope knobs, in three finishes
//! (spec: slot model §5), and the motion that makes them respond.

pub mod knob_maps;
pub mod motion;
pub mod paint;
pub mod skins;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontFamily, Stroke};
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

use skins::GearSkin;

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

/// egui's own widgets (text fields, sliders, menus, the Scripts window) take
/// the finish's ground and ink, so nothing on screen is stock grey.
pub fn apply_visuals(ctx: &egui::Context, s: &GearSkin) {
    let light = s.light();
    let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.override_text_color = Some(s.ground_ink);
    v.panel_fill = s.ground;
    v.window_fill = s.ground;
    v.extreme_bg_color = s.bed;
    v.faint_bg_color = if light { Color32::from_black_alpha(10) } else { Color32::from_white_alpha(8) };
    v.window_stroke =
        Stroke::new(1.0, if light { Color32::from_black_alpha(60) } else { Color32::from_white_alpha(30) });
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.selection.bg_fill = s.accent;
    v.selection.stroke = Stroke::new(1.0, skins::ink_on(s.accent));
    v.hyperlink_color = s.accent;
    v.warn_fg_color = if light { Color32::from_rgb(0x9a, 0x5a, 0x00) } else { skins::AMBER };
    v.error_fg_color = if light { Color32::from_rgb(0xb0, 0x20, 0x20) } else { skins::RED };
    let (body, hover, active) = if light {
        (Color32::from_black_alpha(18), Color32::from_black_alpha(30), Color32::from_black_alpha(45))
    } else {
        (Color32::from_white_alpha(14), Color32::from_white_alpha(26), Color32::from_white_alpha(40))
    };
    let outline = if light { Color32::from_black_alpha(50) } else { Color32::from_white_alpha(28) };
    let r = CornerRadius::same(8);
    for (w, fill) in [
        (&mut v.widgets.noninteractive, Color32::TRANSPARENT),
        (&mut v.widgets.inactive, body),
        (&mut v.widgets.hovered, hover),
        (&mut v.widgets.active, active),
        (&mut v.widgets.open, hover),
    ] {
        w.corner_radius = r;
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, outline);
        w.fg_stroke = Stroke::new(1.0, s.ground_ink);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, outline);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, s.ground_ink);
    v.widgets.active.fg_stroke = Stroke::new(2.0, s.ground_ink);
    v.window_shadow = egui::epaint::Shadow {
        offset: [8, 12],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha((s.sh * 255.0) as u8),
    };
    v.popup_shadow = v.window_shadow;
    let theme = if light { egui::Theme::Light } else { egui::Theme::Dark };
    ctx.set_theme(if light { egui::ThemePreference::Light } else { egui::ThemePreference::Dark });
    ctx.set_visuals_of(theme, v);
    ctx.style_mut_of(theme, |style| {
        for (text_style, id) in style.text_styles.iter_mut() {
            id.size = match text_style {
                egui::TextStyle::Heading => 18.0,
                egui::TextStyle::Small => 10.0,
                _ => 13.0,
            };
        }
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}
