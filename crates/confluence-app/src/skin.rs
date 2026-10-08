//! Skins: every colour, size and image the window draws comes from the active
//! skin. A skin is a folder with `skin.toml` and PNG files; anything a skin
//! leaves out (or gets wrong) falls back to the built-in skin, with a warning.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use confluence_api::PointState;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, TextureHandle,
    TextureOptions,
};
use serde::Deserialize;

use crate::matrix::{CELL_DEFAULT, CELL_MAX, CELL_MIN};
use crate::theme;

/// Colours a skin can set.
pub const COLOR_KEYS: [&str; 9] =
    ["accent", "empty_cell", "offline", "warn", "error", "text", "panel", "background", "selection"];
/// Image slots a skin can fill.
pub const IMAGE_KEYS: [&str; 8] =
    ["background", "top_bar", "panel", "cell_empty", "cell_routed", "cell_pending", "cell_selected", "band"];

/// `skin.toml` as written. Unknown top-level keys are ignored (newer skins
/// may carry keys for part 3 features).
#[derive(Debug, Default, Deserialize)]
pub struct SkinFile {
    pub name: Option<String>,
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
    #[serde(default)]
    pub slot_colors: Vec<String>,
    #[serde(default)]
    pub sizes: BTreeMap<String, f32>,
    #[serde(default)]
    pub images: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Colors {
    pub accent: Color32,
    pub empty_cell: Color32,
    pub offline: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub text: Color32,
    pub panel: Color32,
    pub background: Color32,
    pub selection: Color32,
}

impl Colors {
    /// Sets the colour named `key`; false for an unknown name.
    fn set(&mut self, key: &str, c: Color32) -> bool {
        let slot = match key {
            "accent" => &mut self.accent,
            "empty_cell" => &mut self.empty_cell,
            "offline" => &mut self.offline,
            "warn" => &mut self.warn,
            "error" => &mut self.error,
            "text" => &mut self.text,
            "panel" => &mut self.panel,
            "background" => &mut self.background,
            "selection" => &mut self.selection,
            _ => return false,
        };
        *slot = c;
        true
    }
}

/// A resolved skin: every value set, from the file or the built-in skin.
#[derive(Clone, Debug, PartialEq)]
pub struct Skin {
    pub name: String,
    pub colors: Colors,
    pub slot_colors: Vec<Color32>,
    /// Default cell size (the user still zooms).
    pub cell: f32,
    pub font: f32,
    pub rounding: f32,
    /// Image slot → PNG file.
    pub images: BTreeMap<String, PathBuf>,
}

impl Default for Skin {
    fn default() -> Self {
        Skin {
            name: "Built-in".into(),
            colors: Colors {
                accent: theme::ACCENT,
                empty_cell: theme::EMPTY_CELL,
                offline: theme::OFFLINE,
                warn: theme::WARN,
                error: theme::ERROR,
                text: Color32::from_gray(220),
                panel: Color32::from_gray(27),
                background: Color32::from_gray(18),
                selection: Color32::YELLOW,
            },
            slot_colors: theme::SLOT_COLORS.to_vec(),
            cell: CELL_DEFAULT,
            font: 13.0,
            rounding: 3.0,
            images: BTreeMap::new(),
        }
    }
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<Color32> {
    let hex = s.trim().strip_prefix('#')?;
    let byte = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
    match hex.len() {
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

const BAD_COLOR: &str = "is not #rrggbb or #rrggbbaa";

/// The skin `file` describes (images relative to `dir`). Every problem is a
/// warning and keeps the built-in value for that item.
pub fn resolve(file: &SkinFile, dir: &Path) -> (Skin, Vec<String>) {
    let mut skin = Skin::default();
    let mut warnings = Vec::new();
    if let Some(name) = &file.name {
        skin.name = name.clone();
    }
    for (key, value) in &file.colors {
        match parse_color(value) {
            Some(c) => {
                if !skin.colors.set(key, c) {
                    warnings.push(format!("unknown colour '{key}'"));
                }
            }
            None => warnings.push(format!("colour '{key}': '{value}' {BAD_COLOR}")),
        }
    }
    let mut slots = Vec::new();
    for value in &file.slot_colors {
        match parse_color(value) {
            Some(c) => slots.push(c),
            None => warnings.push(format!("slot colour '{value}' {BAD_COLOR}")),
        }
    }
    if !slots.is_empty() {
        skin.slot_colors = slots;
    }
    for (key, &value) in &file.sizes {
        let valid = value.is_finite() && value >= 0.0;
        match (key.as_str(), valid) {
            ("cell", true) => skin.cell = value.clamp(CELL_MIN, CELL_MAX),
            ("font", true) => skin.font = value.clamp(8.0, 32.0),
            ("rounding", true) => skin.rounding = value.min(16.0),
            ("cell" | "font" | "rounding", false) => warnings.push(format!("size '{key}' must be zero or more")),
            _ => warnings.push(format!("unknown size '{key}'")),
        }
    }
    for (key, file_name) in &file.images {
        if !IMAGE_KEYS.contains(&key.as_str()) {
            warnings.push(format!("unknown image '{key}'"));
            continue;
        }
        let path = dir.join(file_name);
        if path.is_file() {
            skin.images.insert(key.clone(), path);
        } else {
            warnings.push(format!("image '{key}': {} not found", path.display()));
        }
    }
    (skin, warnings)
}

/// Reads `dir/skin.toml`; an unreadable or invalid file leaves the built-in skin.
pub fn read(dir: &Path) -> (Skin, Vec<String>) {
    let path = dir.join("skin.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return (Skin::default(), vec![format!("cannot read {}: {e}", path.display())]),
    };
    match toml::from_str::<SkinFile>(&text) {
        Ok(file) => resolve(&file, dir),
        Err(e) => (Skin::default(), vec![format!("{} is not a valid skin: {e}", path.display())]),
    }
}

pub fn decode_png(path: &Path) -> Result<ColorImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|e| format!("{} is not a PNG image: {e}", path.display()))?
        .to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// The dark edge under every white state mark (mute, invert, pending).
const MARK_OUTLINE: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 200);

fn full_uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

/// The active skin and its loaded images; everything the window paints goes through it.
pub struct Look {
    pub skin: Skin,
    textures: BTreeMap<String, TextureHandle>,
}

impl Look {
    pub fn builtin() -> Look {
        Look { skin: Skin::default(), textures: BTreeMap::new() }
    }

    /// Loads the skin in `dir` and its images. Warnings are prefixed `Skin: `.
    pub fn load(ctx: &egui::Context, dir: &Path) -> (Look, Vec<String>) {
        let (skin, mut warnings) = read(dir);
        let mut textures = BTreeMap::new();
        // egui panics on a texture larger than the GPU accepts.
        let max_side = ctx.input(|i| i.max_texture_side);
        for (key, path) in &skin.images {
            match decode_png(path) {
                Ok(img) if img.size[0] > max_side || img.size[1] > max_side => warnings.push(format!(
                    "image '{key}': {}×{} is too large (this GPU allows {max_side}×{max_side})",
                    img.size[0], img.size[1]
                )),
                Ok(img) => {
                    textures.insert(key.clone(), ctx.load_texture(format!("skin-{key}"), img, TextureOptions::LINEAR));
                }
                Err(e) => warnings.push(format!("image '{key}': {e}")),
            }
        }
        (Look { skin, textures }, warnings.into_iter().map(|w| format!("Skin: {w}")).collect())
    }

    pub fn has_image(&self, key: &str) -> bool {
        self.textures.contains_key(key)
    }

    /// egui's own widgets take the skin's colours, font size and rounding.
    pub fn apply(&self, ctx: &egui::Context) {
        let c = &self.skin.colors;
        let mut v = egui::Visuals::dark();
        v.override_text_color = Some(c.text);
        v.panel_fill = c.panel;
        v.window_fill = c.panel;
        v.extreme_bg_color = c.background;
        v.selection.bg_fill = c.accent;
        let r = CornerRadius::same(self.skin.rounding.round().clamp(0.0, 16.0) as u8);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = r;
        }
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.set_visuals_of(egui::Theme::Dark, v);
        let font = self.skin.font;
        ctx.style_mut_of(egui::Theme::Dark, |style| {
            for (text_style, id) in style.text_styles.iter_mut() {
                id.size = match text_style {
                    egui::TextStyle::Heading => font * 1.4,
                    egui::TextStyle::Small => font * 0.75,
                    _ => font,
                };
            }
        });
    }

    /// A routed cell's fill: the accent, brightness by gain.
    pub fn routed(&self, db: f32) -> Color32 {
        theme::scale(self.skin.colors.accent, theme::gain_brightness(db))
    }

    /// A slot's colour, cycling through the skin's list by id.
    pub fn slot(&self, id: u32) -> Color32 {
        match self.skin.slot_colors.len() {
            0 => self.skin.colors.accent,
            n => self.skin.slot_colors[id as usize % n],
        }
    }

    /// A band's colour: its device's chosen colour, else the default for it.
    pub fn band(&self, band: &crate::matrix::Band) -> Color32 {
        match band.color {
            Some([r, g, b]) => Color32::from_rgb(r, g, b),
            None => self.slot(band.palette),
        }
    }

    /// Warn from 70 % DSP load, error from 90 %.
    pub fn dsp_color(&self, load: f32) -> Option<Color32> {
        if load >= 0.9 {
            Some(self.skin.colors.error)
        } else if load >= 0.7 {
            Some(self.skin.colors.warn)
        } else {
            None
        }
    }

    /// Draws the image for `key` stretched over `rect`; false if the skin has none.
    fn image(&self, p: &Painter, key: &str, rect: Rect, tint: Color32) -> bool {
        match self.textures.get(key) {
            Some(t) => {
                p.image(t.id(), rect, full_uv(), tint);
                true
            }
            None => false,
        }
    }

    /// A background area (`background`, `top_bar`, `panel`): image, else flat `fill`.
    pub fn paint_surface(&self, p: &Painter, rect: Rect, key: &str, fill: Color32) {
        if !self.image(p, key, rect, Color32::WHITE) {
            p.rect_filled(rect, 0.0, fill);
        }
    }

    /// One matrix cell. Mute and invert glyphs are drawn over any image.
    pub fn paint_cell(
        &self,
        p: &Painter,
        rect: Rect,
        cur: Option<&PointState>,
        pending: bool,
        selected: bool,
        dim: bool,
    ) {
        let r = rect.shrink(1.0);
        let rounding = self.skin.rounding.min(r.width() / 4.0);
        let dimmed = |c: Color32| if dim { c.gamma_multiply(0.5) } else { c };
        match cur {
            None => {
                if !self.image(p, "cell_empty", r, dimmed(Color32::WHITE)) {
                    p.rect_filled(r, rounding, dimmed(self.skin.colors.empty_cell));
                }
            }
            Some(pt) => {
                let tint = dimmed(theme::scale(Color32::WHITE, theme::gain_brightness(pt.gain_db)));
                if !self.image(p, "cell_routed", r, tint) {
                    p.rect_filled(r, rounding, dimmed(self.routed(pt.gain_db)));
                }
                // White marks over a dark outline: visible on any skin image.
                if pt.mute {
                    let slash = [r.left_bottom(), r.right_top()];
                    p.line_segment(slash, Stroke::new(3.5, MARK_OUTLINE));
                    p.line_segment(slash, Stroke::new(1.5, Color32::WHITE));
                }
                if pt.invert {
                    let font = FontId::proportional(r.height() * 0.7);
                    for d in
                        [egui::vec2(1.0, 1.0), egui::vec2(-1.0, -1.0), egui::vec2(1.0, -1.0), egui::vec2(-1.0, 1.0)]
                    {
                        p.text(r.center() + d, Align2::CENTER_CENTER, "Ø", font.clone(), MARK_OUTLINE);
                    }
                    p.text(r.center(), Align2::CENTER_CENTER, "Ø", font, Color32::WHITE);
                }
            }
        }
        if pending && !self.image(p, "cell_pending", r, Color32::WHITE) {
            p.rect_stroke(r, rounding, Stroke::new(3.0, MARK_OUTLINE), StrokeKind::Inside);
            p.rect_stroke(r, rounding, Stroke::new(1.0, Color32::WHITE), StrokeKind::Inside);
        }
        if selected && !self.image(p, "cell_selected", rect, Color32::WHITE) {
            p.rect_stroke(rect, 0.0, Stroke::new(2.0, self.skin.colors.selection), StrokeKind::Inside);
        }
    }

    /// A slot header band: the skin's band image tinted by `colour`, else a rounded fill.
    pub fn paint_band(&self, p: &Painter, rect: Rect, colour: Color32) {
        if !self.image(p, "band", rect, colour) {
            p.rect_filled(rect, self.skin.rounding, colour);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path) {
        image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 100, 50, 255]))
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    fn file(toml_text: &str) -> SkinFile {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn colours_parse_with_and_without_alpha() {
        assert_eq!(parse_color("#40a0ff"), Some(Color32::from_rgb(64, 160, 255)));
        assert_eq!(parse_color(" #40a0ff80 "), Some(Color32::from_rgba_unmultiplied(64, 160, 255, 128)));
        assert_eq!(parse_color("40a0ff"), None);
        assert_eq!(parse_color("#40a0f"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
    }

    #[test]
    fn an_empty_skin_is_the_builtin_one() {
        let (skin, warnings) = resolve(&SkinFile::default(), Path::new("."));
        assert_eq!(skin, Skin::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn given_keys_override_and_missing_keys_fall_back() {
        let f = file(
            "name = \"Midnight\"\nslot_colors = [\"#010203\"]\n[colors]\naccent = \"#ff0000\"\n[sizes]\ncell = 24.0\nrounding = 0.0\n",
        );
        let (skin, warnings) = resolve(&f, Path::new("."));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skin.name, "Midnight");
        assert_eq!(skin.colors.accent, Color32::from_rgb(255, 0, 0));
        assert_eq!(skin.colors.warn, theme::WARN, "not given: built-in");
        assert_eq!(skin.cell, 24.0);
        assert_eq!(skin.rounding, 0.0);
        assert_eq!(skin.font, Skin::default().font);
        assert_eq!(skin.slot_colors, vec![Color32::from_rgb(1, 2, 3)]);
    }

    #[test]
    fn problems_are_warnings_and_keep_the_builtin_values() {
        let dir = tempfile::tempdir().unwrap();
        let f = file(
            "slot_colors = [\"#zz\"]\n[colors]\naccent = \"blue\"\nglow = \"#ffffff\"\n[sizes]\nzoom = 2.0\nfont = -1.0\n[images]\ncell_routed = \"missing.png\"\nsparkle = \"x.png\"\n",
        );
        let (skin, warnings) = resolve(&f, dir.path());
        assert_eq!(warnings.len(), 7, "{warnings:?}");
        for key in ["#zz", "accent", "glow", "zoom", "font", "cell_routed", "sparkle"] {
            assert!(warnings.iter().any(|w| w.contains(key)), "no warning about {key}: {warnings:?}");
        }
        assert_eq!(skin, Skin::default());
    }

    #[test]
    fn a_folder_without_skin_toml_or_with_a_broken_one_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (skin, warnings) = read(dir.path());
        assert_eq!(skin, Skin::default());
        assert!(warnings[0].contains("cannot read"), "{warnings:?}");
        std::fs::write(dir.path().join("skin.toml"), "name = ").unwrap();
        let (_, warnings) = read(dir.path());
        assert!(warnings[0].contains("not a valid skin"), "{warnings:?}");
    }

    #[test]
    fn valid_images_become_textures_and_bad_ones_warn() {
        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("cell_on.png"));
        std::fs::write(dir.path().join("bad.png"), b"not a png").unwrap();
        std::fs::write(dir.path().join("skin.toml"), "[images]\ncell_routed = \"cell_on.png\"\nband = \"bad.png\"\n")
            .unwrap();
        let ctx = egui::Context::default();
        let (look, warnings) = Look::load(&ctx, dir.path());
        assert!(look.has_image("cell_routed"));
        assert!(!look.has_image("band"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("Skin: ") && warnings[0].contains("band"), "{warnings:?}");
    }

    /// An image larger than the GPU accepts would bring the window down when
    /// uploaded: it must be a warning instead.
    #[test]
    fn an_image_too_large_for_the_gpu_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let max = ctx.input(|i| i.max_texture_side) as u32;
        image::RgbaImage::from_pixel(max + 1, 1, image::Rgba([0, 0, 0, 255]))
            .save_with_format(dir.path().join("wide.png"), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(
            dir.path().join("skin.toml"),
            "[images]
background = \"wide.png\"
",
        )
        .unwrap();
        let (look, warnings) = Look::load(&ctx, dir.path());
        assert!(!look.has_image("background"));
        assert!(warnings.iter().any(|w| w.contains("background") && w.contains("too large")), "{warnings:?}");
    }

    /// Mute, invert and pending must stay visible on any skin image, including
    /// a light one: each white mark gets a dark outline under it.
    #[test]
    fn state_marks_have_a_dark_outline_for_any_background() {
        use eframe::egui::Shape;
        let look = Look::builtin();
        let ctx = egui::Context::default();
        let pt = PointState { input: 0, output: 0, gain_db: 0.0, mute: true, invert: true };
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let rect = Rect::from_min_size(Pos2::new(10.0, 10.0), egui::vec2(20.0, 20.0));
            look.paint_cell(ui.painter(), rect, Some(&pt), true, false, false);
        });
        out.textures_delta.clear(); // egui requires a frame's texture changes to be handled
        let dark = |c: Color32| c.a() > 0 && c.r() < 64 && c.g() < 64 && c.b() < 64;
        let (mut dark_line, mut texts, mut dark_outline) = (false, 0, false);
        for clipped in &out.shapes {
            match &clipped.shape {
                Shape::LineSegment { stroke, .. } if dark(stroke.color) => dark_line = true,
                Shape::Text(_) => texts += 1,
                Shape::Rect(r) if r.stroke.width > 0.0 && dark(r.stroke.color) => dark_outline = true,
                _ => {}
            }
        }
        assert!(dark_line, "the mute slash has a dark outline");
        assert!(texts >= 2, "the invert glyph has a dark shadow under it ({texts} text shapes)");
        assert!(dark_outline, "the pending outline has a dark edge");
    }

    #[test]
    fn routed_cells_and_slots_follow_the_skin() {
        let mut skin = Skin::default();
        skin.colors.accent = Color32::from_rgb(255, 0, 0);
        skin.slot_colors = vec![Color32::from_rgb(1, 1, 1), Color32::from_rgb(2, 2, 2)];
        let look = Look { skin, textures: BTreeMap::new() };
        assert_eq!(look.routed(12.0), Color32::from_rgb(255, 0, 0));
        assert_eq!(look.routed(-60.0).r(), 140, "55 % at the quietest shown gain");
        assert_eq!(look.slot(3), Color32::from_rgb(2, 2, 2));
        assert_eq!(look.dsp_color(0.95), Some(look.skin.colors.error));
    }
}
