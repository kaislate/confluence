//! Painting the gear: raised panels, OLED wells, glass pills, recessed trays,
//! LEDs, etched labels, the computed S-slope knob and level meters
//! (spec: slot model §5; the Kaigen Phantom recipe).

use std::collections::HashMap;

use eframe::egui;
use std::sync::{Arc, Mutex, OnceLock};

use egui::epaint::Shadow;
use egui::{
    Align2, Color32, ColorImage, CornerRadius, FontFamily, FontId, Id, Mesh, Painter, Pos2, Rect, Response, Sense,
    Shape, Stroke, TextureHandle, TextureOptions, Ui, Vec2,
};

use super::knob_maps::{self, KnobMaps, CAP_FRACTION};
use super::skins::GearSkin;

/// Corner radius of raised panels.
pub const PANEL_RADIUS: u8 = 22;

/// `family` at `size`, or the proportional font if `family` is not installed
/// (painting with an unknown family would panic).
pub fn font(ctx: &egui::Context, family: &str, size: f32) -> FontId {
    let fam = FontFamily::Name(family.into());
    if ctx.fonts(|f| f.families().contains(&fam)) {
        FontId::new(size, fam)
    } else {
        FontId::proportional(size)
    }
}

/// `c` with its alpha scaled by `a` (premultiplied).
fn alpha(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

/// A strip fading from `from` at one edge to `to` at the other: `vertical`
/// fades top to bottom, otherwise left to right.
fn fade(p: &Painter, r: Rect, from: Color32, to: Color32, vertical: bool) {
    let mut m = Mesh::default();
    let (a, b, c, d) = (r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom());
    let (ca, cb, cc, cd) = if vertical { (from, from, to, to) } else { (from, to, to, from) };
    m.colored_vertex(a, ca);
    m.colored_vertex(b, cb);
    m.colored_vertex(c, cc);
    m.colored_vertex(d, cd);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

/// A raised panel: the finish's colour with bevel light top-left, shade
/// bottom-right, and a soft drop shadow. `tint` washes it in a device colour.
pub fn panel(p: &Painter, r: Rect, s: &GearSkin, tint: Option<Color32>) {
    let radius = CornerRadius::same(PANEL_RADIUS);
    let hl = Shadow { offset: [-6, -6], blur: 16, spread: 0, color: Color32::from_white_alpha((s.hl * 255.0) as u8) };
    let sh = Shadow { offset: [8, 10], blur: 22, spread: 0, color: Color32::from_black_alpha((s.sh * 255.0) as u8) };
    p.add(hl.as_shape(r, radius));
    p.add(sh.as_shape(r, radius));
    p.rect_filled(r, radius, s.p2);
    if let Some(c) = tint {
        p.rect_filled(r, radius, alpha(c, 0.14));
    }
    // The light corner and the dark one: the radial gradient, in two fades.
    let inset = PANEL_RADIUS as f32;
    let h = r.shrink2(Vec2::new(inset, 0.0));
    let v = r.shrink2(Vec2::new(0.0, inset));
    let lift = |c: Color32, k: f32| alpha(c, k);
    fade(
        p,
        Rect::from_min_max(h.left_top(), Pos2::new(h.right(), r.top() + 16.0)),
        lift(Color32::WHITE, 0.62 * s.hl),
        Color32::TRANSPARENT,
        true,
    );
    fade(
        p,
        Rect::from_min_max(v.left_top(), Pos2::new(r.left() + 12.0, v.bottom())),
        lift(Color32::WHITE, 0.32 * s.hl),
        Color32::TRANSPARENT,
        false,
    );
    fade(
        p,
        Rect::from_min_max(Pos2::new(h.left(), r.bottom() - 14.0), h.right_bottom()),
        Color32::TRANSPARENT,
        lift(Color32::BLACK, 0.10 + 0.1 * s.sh),
        true,
    );
    fade(
        p,
        Rect::from_min_max(Pos2::new(r.right() - 12.0, v.top()), v.right_bottom()),
        Color32::TRANSPARENT,
        lift(Color32::BLACK, 0.07 + 0.1 * s.sh),
        false,
    );
    // A faint wash of the light stop toward the top-left, the dark one bottom-right.
    fade(p, r.shrink(inset * 0.5), alpha(s.p1, 0.35), alpha(s.p3, 0.35), true);
}

/// Insets a recess: dark top and left, light bottom and right.
fn inset(p: &Painter, r: Rect, depth: f32, dark: f32, light: f32) {
    let d = depth.min(r.height() / 2.0).min(r.width() / 2.0);
    let black = Color32::from_black_alpha((dark * 255.0) as u8);
    let white = Color32::from_white_alpha((light * 255.0) as u8);
    fade(p, Rect::from_min_size(r.min, Vec2::new(r.width(), d)), black, Color32::TRANSPARENT, true);
    fade(p, Rect::from_min_size(r.min, Vec2::new(d, r.height())), black, Color32::TRANSPARENT, false);
    fade(p, Rect::from_min_max(Pos2::new(r.left(), r.bottom() - d), r.max), Color32::TRANSPARENT, white, true);
    fade(p, Rect::from_min_max(Pos2::new(r.right() - d, r.top()), r.max), Color32::TRANSPARENT, white, false);
}

/// A recessed tray (meters sit in one).
pub fn tray(p: &Painter, r: Rect, s: &GearSkin) {
    p.rect_filled(r, CornerRadius::same(6), s.p3);
    inset(p, r, 14.0, 0.20, 0.35 * s.hl.max(0.15));
}

/// An OLED screen in a ringed well: two lines of glowing text.
pub fn oled(p: &Painter, r: Rect, s: &GearSkin, line1: &str, line2: &str, color: Color32) {
    let radius = CornerRadius::same(8);
    p.rect_filled(r.expand(3.0), CornerRadius::same(10), alpha(Color32::BLACK, 0.35));
    p.rect_filled(r, radius, Color32::from_rgb(6, 6, 8));
    // The rings: dark, light, dark.
    p.rect_stroke(
        r.expand(2.5),
        CornerRadius::same(10),
        Stroke::new(1.5, alpha(Color32::BLACK, 0.55)),
        egui::StrokeKind::Middle,
    );
    p.rect_stroke(
        r.expand(1.25),
        CornerRadius::same(9),
        Stroke::new(0.75, alpha(Color32::WHITE, 0.10 + 0.25 * s.hl)),
        egui::StrokeKind::Middle,
    );
    p.rect_stroke(r, radius, Stroke::new(1.0, alpha(Color32::BLACK, 0.8)), egui::StrokeKind::Inside);
    // The inner shadow: four inset strokes, fading.
    for (i, a) in [0.5f32, 0.32, 0.18, 0.08].iter().enumerate() {
        let k = i as f32 + 1.0;
        p.rect_stroke(
            r.shrink(k),
            CornerRadius::same(8u8.saturating_sub(i as u8)),
            Stroke::new(1.0, alpha(Color32::BLACK, *a)),
            egui::StrokeKind::Inside,
        );
    }
    // Scanlines.
    let mut y = r.top() + 2.0;
    while y < r.bottom() - 1.0 {
        p.line_segment(
            [Pos2::new(r.left() + 2.0, y), Pos2::new(r.right() - 2.0, y)],
            Stroke::new(1.0, Color32::from_white_alpha(8)),
        );
        y += 3.0;
    }
    let ctx = p.ctx();
    let big = font(ctx, "oled", (r.height() * 0.42).clamp(10.0, 40.0));
    let small = font(ctx, "oled", (r.height() * 0.28).clamp(9.0, 26.0));
    let p = p.with_clip_rect(r.shrink(2.0));
    let glow = |pos: Pos2, text: &str, f: &FontId| {
        for d in [Vec2::new(-1.0, 0.0), Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)] {
            p.text(pos + d, Align2::LEFT_CENTER, text, f.clone(), alpha(color, 0.25));
        }
        p.text(pos, Align2::LEFT_CENTER, text, f.clone(), color);
    };
    let x = r.left() + 10.0;
    if line2.is_empty() {
        glow(Pos2::new(x, r.center().y), line1, &big);
    } else {
        glow(Pos2::new(x, r.top() + r.height() * 0.36), line1, &big);
        glow(Pos2::new(x, r.top() + r.height() * 0.74), line2, &small);
    }
}

/// A glass pill button.
pub fn pill(ui: &mut Ui, label: &str, s: &GearSkin) -> Response {
    let f = font(ui.ctx(), "label", 13.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), f.clone(), s.ink);
    let size = Vec2::new(galley.size().x + 28.0, 26.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    let radius = CornerRadius::same(13);
    let pressed = resp.is_pointer_button_down_on();
    fade(p, r, Color32::from_black_alpha(if pressed { 30 } else { 15 }), Color32::from_black_alpha(5), true);
    p.rect_stroke(r, radius, Stroke::new(1.0, Color32::from_black_alpha(60)), egui::StrokeKind::Inside);
    fade(
        p,
        Rect::from_min_size(r.min + Vec2::new(10.0, 1.0), Vec2::new(r.width() - 20.0, 4.0)),
        Color32::from_black_alpha(50),
        Color32::TRANSPARENT,
        true,
    );
    fade(
        p,
        Rect::from_min_max(Pos2::new(r.left() + 10.0, r.bottom() - 3.0), Pos2::new(r.right() - 10.0, r.bottom() - 1.0)),
        Color32::TRANSPARENT,
        Color32::from_white_alpha((40.0 + 60.0 * s.hl) as u8),
        true,
    );
    p.line_segment(
        [Pos2::new(r.left() + 12.0, r.bottom() + 0.5), Pos2::new(r.right() - 12.0, r.bottom() + 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha((150.0 * s.hl) as u8)),
    );
    let ink = if resp.hovered() { s.ink } else { alpha(s.ink, 0.85) };
    p.text(r.center(), Align2::CENTER_CENTER, label, f, ink);
    resp
}

/// An LED: a small dome, lit (with a glow) or dark.
pub fn led(p: &Painter, center: Pos2, s: &GearSkin, color: Color32, lit: bool) {
    let r = 6.0;
    if lit {
        for (k, a) in [(2.6f32, 0.08f32), (1.9, 0.14), (1.4, 0.22)] {
            p.circle_filled(center, r * k, alpha(color, a));
        }
    }
    p.circle_filled(center + Vec2::new(0.0, 1.0), r + 1.0, alpha(Color32::WHITE, 0.2 * s.hl + 0.05));
    p.circle_filled(center - Vec2::new(0.0, 0.5), r + 1.0, alpha(Color32::BLACK, 0.45));
    let body = if lit { color } else { Color32::from_rgb(color.r() / 5, color.g() / 5, color.b() / 5) };
    // The dome: rings from the darker edge to a light centre.
    let edge =
        Color32::from_rgb((body.r() as f32 * 0.6) as u8, (body.g() as f32 * 0.6) as u8, (body.b() as f32 * 0.6) as u8);
    p.circle_filled(center, r, edge);
    p.circle_filled(center, r * 0.75, body);
    let centre = if lit { Color32::from_rgb(255, 255, 255) } else { alpha(Color32::WHITE, 0.15) };
    p.circle_filled(center - Vec2::new(1.5, 1.5), r * 0.32, alpha(centre, if lit { 0.75 } else { 0.4 }));
}

/// A label etched in the panel: a light copy below, then the ink.
pub fn etched(p: &Painter, pos: Pos2, text: &str, s: &GearSkin) {
    let f = font(p.ctx(), "label", 11.0);
    p.text(pos + Vec2::new(0.0, 1.0), Align2::LEFT_CENTER, text, f.clone(), alpha(Color32::WHITE, 0.5 * s.hl.max(0.2)));
    p.text(pos, Align2::LEFT_CENTER, text, f, alpha(s.ink, 0.8));
}

/// Multiply `base` by `shade` (255 = no change) through black at strength `k`:
/// the CPU twin of the shade texture.
pub fn shade_blend(base: [u8; 3], shade: u8, k: f32) -> [u8; 3] {
    let f = 1.0 - k * (1.0 - shade as f32 / 255.0);
    base.map(|c| (c as f32 * f).round().clamp(0.0, 255.0) as u8)
}

/// Lighten `base` in proportion to itself by `light` (0 = no change) at
/// strength `k`: the CPU twin of the light texture.
pub fn light_blend(base: [u8; 3], light: u8, k: f32) -> [u8; 3] {
    let f = light as f32 / 255.0 * k;
    base.map(|c| (c as f32 + c as f32 * f).round().clamp(0.0, 255.0) as u8)
}

/// Knob maps per pixel size, generated once each.
fn maps(px: usize) -> Arc<KnobMaps> {
    static CACHE: OnceLock<Mutex<HashMap<usize, Arc<KnobMaps>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(m) = cache.lock().ok().and_then(|c| c.get(&px).cloned()) {
        return m;
    }
    let m = Arc::new(knob_maps::generate(px));
    if let Ok(mut c) = cache.lock() {
        c.insert(px, m.clone());
    }
    m
}

/// The knob's textures for a map of `px` pixels on a `panel` coloured panel.
fn knob_textures(ctx: &egui::Context, px: usize, panel: Color32, s: &GearSkin) -> [TextureHandle; 3] {
    let key = Id::new(("gear-knob", px, panel.to_array(), s.light_k.to_bits(), s.shade_k.to_bits()));
    if let Some(t) = ctx.data(|d| d.get_temp::<[TextureHandle; 3]>(key)) {
        return t;
    }
    let m = maps(px);
    let shade: Vec<Color32> = m
        .shade
        .iter()
        .map(|&v| Color32::from_rgba_premultiplied(0, 0, 0, ((255 - v) as f32 * s.shade_k).round() as u8))
        .collect();
    // Alpha 0 with colour: added to what is below (premultiplied blending).
    let light: Vec<Color32> = m
        .light
        .iter()
        .map(|&v| {
            let f = v as f32 / 255.0 * s.light_k;
            let c = |x: u8| (x as f32 * f).round().min(255.0) as u8;
            Color32::from_rgba_premultiplied(c(panel.r()), c(panel.g()), c(panel.b()), 0)
        })
        .collect();
    let cap: Vec<Color32> = m.cap.iter().map(|&v| Color32::from_rgba_premultiplied(v, v, v, v)).collect();
    let size = [px, px];
    let load = |name: &str, pixels: Vec<Color32>| {
        ctx.load_texture(format!("gear-{name}-{px}"), ColorImage::new(size, pixels), TextureOptions::LINEAR)
    };
    let t = [load("shade", shade), load("light", light), load("cap", cap)];
    ctx.data_mut(|d| d.insert_temp(key, t.clone()));
    t
}

/// A point on the value arc: 0 at 135° (bottom-left), 1 at 405° (bottom-right).
fn arc_point(c: Pos2, r: f32, v: f32) -> Pos2 {
    let a = (135.0 + 270.0 * v).to_radians();
    c + Vec2::new(a.cos(), a.sin()) * r
}

fn arc(p: &Painter, c: Pos2, r: f32, from: f32, to: f32, stroke: Stroke) {
    if to <= from {
        return;
    }
    let n = ((to - from) * 64.0).ceil().max(2.0) as usize;
    let pts: Vec<Pos2> = (0..=n).map(|i| arc_point(c, r, from + (to - from) * i as f32 / n as f32)).collect();
    p.add(Shape::line(pts, stroke));
}

/// The S-slope knob: computed light and shade over the panel, the cap, and
/// the value arc. Drag up/down or scroll to change `value` (0..1). `size` is
/// the side of the square it takes, its cast shadow included.
#[allow(clippy::too_many_arguments)]
pub fn knob(
    ui: &mut Ui,
    id: Id,
    size: f32,
    value: &mut f32,
    arc_color: Color32,
    cap: Color32,
    panel: Color32,
    s: &GearSkin,
    label: &str,
) -> Response {
    let (r, mut resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    let resp_id = resp.id;
    let _ = id;
    let before = *value;
    if resp.dragged() {
        *value = (*value - resp.drag_delta().y / 200.0).clamp(0.0, 1.0);
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *value = (*value + scroll.signum() * 0.02).clamp(0.0, 1.0);
        }
    }
    if *value != before {
        resp.mark_changed();
    }
    let p = ui.painter();
    let ppp = ui.ctx().pixels_per_point();
    let px = ((size * ppp * 2.0).round() as usize).clamp(32, 768);
    let [shade, light, cap_tex] = knob_textures(ui.ctx(), px, panel, s);
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    p.image(shade.id(), r, uv, Color32::WHITE);
    p.image(light.id(), r, uv, Color32::WHITE);
    p.image(cap_tex.id(), r, uv, cap);
    let c = r.center();
    let cap_r = size * CAP_FRACTION / 2.0;
    if s.band {
        p.circle_stroke(c, cap_r - 1.5, Stroke::new(1.0, alpha(Color32::WHITE, 0.10)));
    }
    let arc_r = cap_r * 1.62;
    arc(p, c, arc_r, 0.0, 1.0, Stroke::new(3.0, alpha(s.ink, 0.10)));
    arc(p, c, arc_r, 0.0, *value, Stroke::new(7.0, alpha(arc_color, 0.28)));
    arc(p, c, arc_r, 0.0, *value, Stroke::new(3.0, arc_color));
    let f = font(ui.ctx(), "label", (cap_r * 0.42).clamp(9.0, 22.0));
    let ink = if s.light_k >= 1.0 { s.ink } else { Color32::from_rgb(0x30, 0x32, 0x36) };
    p.text(c, Align2::CENTER_CENTER, label, f, ink);
    let _ = resp_id;
    resp
}

/// A vertical level meter in a tray: −60 dB empty, 0 dB full; green to −12,
/// yellow to −3, red above; the peak hold a 2 px line.
pub fn meter(p: &Painter, r: Rect, peak_db: f32, rms_db: f32, hold_db: f32) {
    let frac = |db: f32| if db.is_finite() { ((db + 60.0) / 60.0).clamp(0.0, 1.0) } else { 0.0 };
    let y = |f: f32| r.bottom() - f * r.height();
    let bar = r.shrink2(Vec2::new(1.0, 0.0));
    let zones = [
        (0.0, frac(-12.0), Color32::from_rgb(0x4c, 0xd9, 0x64)),
        (frac(-12.0), frac(-3.0), Color32::from_rgb(0xff, 0xcf, 0x3a)),
        (frac(-3.0), 1.0, Color32::from_rgb(0xff, 0x4d, 0x4d)),
    ];
    let level = frac(rms_db);
    let peak = frac(peak_db);
    for (lo, hi, col) in zones {
        let top = hi.min(peak);
        if top > lo {
            let dim = Rect::from_min_max(Pos2::new(bar.left(), y(top)), Pos2::new(bar.right(), y(lo)));
            p.rect_filled(dim, CornerRadius::ZERO, alpha(col, 0.35));
        }
        let top = hi.min(level);
        if top > lo {
            let lit = Rect::from_min_max(Pos2::new(bar.left(), y(top)), Pos2::new(bar.right(), y(lo)));
            p.rect_filled(lit, CornerRadius::ZERO, col);
        }
    }
    let hold = frac(hold_db);
    if hold > 0.0 {
        let hy = y(hold);
        p.line_segment([Pos2::new(bar.left(), hy), Pos2::new(bar.right(), hy)], Stroke::new(2.0, Color32::WHITE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shade_and_light_blend_like_the_mockup() {
        // multiply by shade through black with alpha (1 - shade) * k
        let base = [0x34u8, 0x36, 0x3c];
        let (shade, k) = (128u8, 0.68f32);
        let out = shade_blend(base, shade, k);
        let want = |c: u8| (c as f32 * (1.0 - k * (1.0 - shade as f32 / 255.0))).round() as u8;
        assert_eq!(out, [want(base[0]), want(base[1]), want(base[2])]);
        // proportional light: base + base * light * k
        let lit = light_blend(base, 100, 1.0);
        assert!(lit[0] > base[0]);
        assert_eq!(light_blend(base, 0, 1.0), base, "no light, no change");
    }
}
