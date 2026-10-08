//! Painting the gear: the grained ground, raised panels, OLED wells, glass
//! pills, recessed trays, LEDs, etched labels, the computed S-slope knob,
//! level meters and the matrix's pins (spec: slot model §5; the Kaigen
//! Phantom recipe; the design review's depth levels).

use std::collections::HashMap;

use eframe::egui;
use std::sync::{Arc, Mutex, OnceLock};

use egui::epaint::Shadow;
use egui::text::{LayoutJob, TextFormat};
use egui::TextWrapMode;
use egui::{
    Align2, Color32, ColorImage, CornerRadius, FontFamily, FontId, Id, Mesh, Painter, Pos2, Rect, Response, Sense,
    Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, TextureWrapMode, Ui, Vec2,
};

use super::knob_maps::{self, KnobMaps, CAP_FRACTION};
use super::skins::GearSkin;

/// Corner radius of raised panels (cards).
pub const PANEL_RADIUS: u8 = 16;
/// Pill height and radius.
pub const PILL_H: f32 = 24.0;
/// OLED text sizes: VT323 is a pixel face, drawn at 24 and 16.
pub const OLED_L: f32 = 24.0;
pub const OLED_S: f32 = 16.0;

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
pub fn alpha(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

/// How far in from the side a corner of `radius` starts, `dy` from the top
/// (or bottom) edge.
pub fn corner_inset(radius: f32, dy: f32) -> f32 {
    if dy >= radius {
        return 0.0;
    }
    let k = radius - dy.max(0.0);
    radius - (radius * radius - k * k).max(0.0).sqrt()
}

/// A strip fading from `from` at one edge to `to` at the other: `vertical`
/// fades top to bottom, otherwise left to right.
pub fn fade(p: &Painter, r: Rect, from: Color32, to: Color32, vertical: bool) {
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

/// `a` to `b` by `t` (0..1), per channel.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()), m(a.a(), b.a()))
}

/// The outline of a rounded rectangle, clockwise from the top-left corner,
/// with each point's outward normal.
fn rounded_outline(r: Rect, radius: f32) -> Vec<(Pos2, Vec2)> {
    const STEPS: usize = 8;
    let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
    let corners = [
        (Pos2::new(r.left() + rad, r.top() + rad), 180.0f32),
        (Pos2::new(r.right() - rad, r.top() + rad), 270.0),
        (Pos2::new(r.right() - rad, r.bottom() - rad), 0.0),
        (Pos2::new(r.left() + rad, r.bottom() - rad), 90.0),
    ];
    let mut out = Vec::with_capacity(4 * (STEPS + 1));
    for (c, start) in corners {
        for k in 0..=STEPS {
            let a = (start + 90.0 * k as f32 / STEPS as f32).to_radians();
            let n = Vec2::new(a.cos(), a.sin());
            out.push((c + n * rad, n));
        }
    }
    out
}

/// Inward offsets of the rings a rounded mesh is built from: dense at the
/// edge, where insets, bevels and reflections change fastest.
const RING_OFFSETS: [f32; 12] = [0.0, 1.0, 2.0, 3.0, 4.5, 6.0, 8.0, 10.0, 13.0, 17.0, 24.0, 36.0];

/// A mesh covering a rounded rectangle exactly: concentric rounded rings
/// (each `RING_OFFSETS` step further in, the corner radius shrinking with
/// it), a fan to the centre, and a one-pixel feather outside the edge so
/// the curve is antialiased. `vertex` gives each point's colour from its
/// position, its distance in from the edge and its outward normal; `uv`
/// maps positions to texture coordinates (for a textured mesh).
fn ring_mesh(
    p: &Painter,
    r: Rect,
    radius: f32,
    texture: Option<egui::TextureId>,
    uv: impl Fn(Pos2) -> Pos2,
    vertex: impl Fn(Pos2, f32, Vec2) -> Color32,
) -> Mesh {
    let half = (r.width().min(r.height()) / 2.0).max(0.0);
    let rings: Vec<f32> = RING_OFFSETS.iter().copied().filter(|d| *d < half - 0.25).collect();
    let mut m = match texture {
        Some(t) => Mesh::with_texture(t),
        None => Mesh::default(),
    };
    let add = |m: &mut Mesh, pos: Pos2, colour: Color32| {
        m.vertices.push(egui::epaint::Vertex { pos, uv: uv(pos), color: colour });
    };
    let mut n = 0u32;
    for &d in &rings {
        let outline = rounded_outline(r.shrink(d), (radius - d).max(0.0));
        n = outline.len() as u32;
        for (pt, normal) in outline {
            add(&mut m, pt, vertex(pt, d, normal));
        }
    }
    let feather = p.ctx().pixels_per_point().recip();
    for (pt, normal) in rounded_outline(r, radius.max(0.0)) {
        let q = pt + normal * feather;
        add(&mut m, q, Color32::TRANSPARENT);
    }
    let c = r.center();
    add(&mut m, c, vertex(c, half, Vec2::ZERO));
    let k = rings.len() as u32;
    let at = |ring: u32, i: u32| ring * n + (i % n);
    let (feather_ring, centre) = (k, (k + 1) * n);
    let quad = |m: &mut Mesh, a: u32, b: u32| {
        for i in 0..n {
            let (p0, p1, p2, p3) = (at(a, i), at(a, i + 1), at(b, i + 1), at(b, i));
            m.add_triangle(p0, p1, p2);
            m.add_triangle(p0, p2, p3);
        }
    };
    quad(&mut m, feather_ring, 0);
    for ring in 0..k.saturating_sub(1) {
        quad(&mut m, ring, ring + 1);
    }
    for i in 0..n {
        m.add_triangle(centre, at(k - 1, i), at(k - 1, i + 1));
    }
    m
}

/// A rounded rectangle filled with `colour_at` (smooth gradients need no
/// texture), its edge antialiased.
fn rounded_fill(p: &Painter, r: Rect, radius: f32, colour_at: impl Fn(Pos2) -> Color32) {
    p.add(Shape::mesh(plain_mesh(p, r, radius, |q, _, _| colour_at(q))));
}

/// An untextured ring mesh. It draws with the font atlas, so every vertex
/// takes the atlas's white texel (anything else picks up glyph pixels).
fn plain_mesh(p: &Painter, r: Rect, radius: f32, vertex: impl Fn(Pos2, f32, Vec2) -> Color32) -> Mesh {
    ring_mesh(p, r, radius, None, |_| egui::epaint::WHITE_UV, vertex)
}

/// A rounded rectangle shaded by `vertex` (position, distance in from the
/// edge, outward normal).
pub fn rounded_shade(p: &Painter, r: Rect, radius: f32, vertex: impl Fn(Pos2, f32, Vec2) -> Color32) {
    p.add(Shape::mesh(plain_mesh(p, r, radius, vertex)));
}

/// A texture painted over a rounded rectangle and clipped to its corners:
/// `uv` maps each point to the texture (outside 0..1 samples the texture's
/// edge, so give it a transparent border), `tint` multiplies it.
pub fn textured_rounded(
    p: &Painter,
    r: Rect,
    radius: f32,
    texture: egui::TextureId,
    uv: impl Fn(Pos2) -> Pos2,
    tint: Color32,
) {
    p.add(Shape::mesh(ring_mesh(p, r, radius, Some(texture), uv, |_, _, _| tint)));
}

/// White at `white` over black at `black` (both 0..1), premultiplied.
fn light_dark(white: f32, black: f32) -> Color32 {
    let w = (white.clamp(0.0, 1.0) * 255.0).round();
    let b = (black.clamp(0.0, 1.0) * 255.0).round();
    Color32::from_rgba_premultiplied(w as u8, w as u8, w as u8, (w + b).min(255.0) as u8)
}

/// A glass reflection over the top `reach` (0..1) of a rounded shape,
/// `strength` at the top edge fading to nothing.
fn reflection(p: &Painter, r: Rect, radius: f32, strength: f32, reach: f32, top_shade: f32) {
    let h = r.height().max(1.0);
    rounded_shade(p, r, radius, move |q, d, n| {
        let y = (q.y - r.top()) / h;
        let glass = strength * (1.0 - y / reach).max(0.0);
        let shade = top_shade * (-n.y).max(0.0) * (1.0 - d / 4.0).max(0.0);
        light_dark(glass, shade)
    });
}

/// The panel's radial gradient at `q`: light at 28% / 18% of the panel,
/// stops p1 at 0, p2 at 0.48 and p3 at 1.1 of 1.1 x its larger side.
fn panel_gradient(r: Rect, s: &GearSkin) -> impl Fn(Pos2) -> Color32 + '_ {
    let centre = r.min + Vec2::new(r.width() * 0.28, r.height() * 0.18);
    let reach = 1.1 * r.width().max(r.height());
    move |q: Pos2| {
        let d = (q - centre).length() / reach;
        if d < 0.48 {
            mix(s.p1, s.p2, d / 0.48)
        } else {
            mix(s.p2, s.p3, (d - 0.48) / (1.1 - 0.48))
        }
    }
}

/// A raised panel: the finish's radial gradient, light from the top-left
/// and a soft drop shadow to the bottom-right. `tint` washes it in a colour.
pub fn panel(p: &Painter, r: Rect, s: &GearSkin, tint: Option<Color32>) {
    panel_lifted(p, r, s, tint, 0.0, PANEL_RADIUS);
}

/// A raised panel lifted by `lift` (0 resting, 1 hovered: the shadow grows
/// and moves) with corner `radius`.
pub fn panel_lifted(p: &Painter, r: Rect, s: &GearSkin, tint: Option<Color32>, lift: f32, radius: u8) {
    let cr = CornerRadius::same(radius);
    let lift = lift.clamp(0.0, 1.0);
    let hl = Shadow { offset: [-5, -5], blur: 14, spread: 0, color: Color32::from_white_alpha((s.hl * 120.0) as u8) };
    let sh = Shadow {
        offset: [8 + (2.0 * lift) as i8, 10 + (2.0 * lift) as i8],
        blur: 22 + (4.0 * lift) as u8,
        spread: 0,
        color: Color32::from_black_alpha((s.sh * 255.0 * (1.0 + 0.1 * lift)) as u8),
    };
    p.add(hl.as_shape(r, cr));
    p.add(sh.as_shape(r, cr));
    rounded_fill(p, r, radius as f32, panel_gradient(r, s));
    if let Some(c) = tint {
        p.rect_filled(r, cr, alpha(c, 0.14));
    }
    // The bevel: a light rim, strongest along the top-left.
    let rim = |q: Pos2| {
        let d = (q - r.center()).normalized();
        let facing = (-(d.x + d.y) * std::f32::consts::FRAC_1_SQRT_2).max(0.0);
        Color32::from_white_alpha((facing * (40.0 + 80.0 * s.hl)) as u8)
    };
    let outline = rounded_outline(r.shrink(0.75), radius as f32 - 0.75);
    for w in outline.windows(2) {
        let (a, b) = (w[0].0, w[1].0);
        p.line_segment([a, b], Stroke::new(1.0, rim(a + (b - a) * 0.5)));
    }
}

/// A floating panel (pickers, toasts): a deeper cast shadow and an edge light.
pub fn floating(p: &Painter, r: Rect, s: &GearSkin, radius: u8) {
    let cr = CornerRadius::same(radius);
    let sh = Shadow {
        offset: [12, 16],
        blur: 36,
        spread: 0,
        color: Color32::from_black_alpha(((s.sh + 0.10) * 255.0).min(255.0) as u8),
    };
    p.add(sh.as_shape(r, cr));
    rounded_fill(p, r, radius as f32, panel_gradient(r, s));
    p.rect_stroke(r, cr, Stroke::new(1.0, Color32::from_white_alpha((60.0 + 100.0 * s.hl) as u8)), StrokeKind::Inside);
}

/// A pseudo-random grain tile, white (or black) at random alpha, repeated
/// over the ground at one texel per device pixel.
fn grain_texture(ctx: &egui::Context, light: bool) -> TextureHandle {
    const SIDE: usize = 128;
    let key = Id::new(("gear-grain", light));
    if let Some(t) = ctx.data(|d| d.get_temp::<TextureHandle>(key)) {
        return t;
    }
    let mut x: u32 = if light { 0x9e37_79b9 } else { 0x7f4a_7c15 };
    let mut pixels = Vec::with_capacity(SIDE * SIDE);
    for _ in 0..SIDE * SIDE {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let a = (x >> 24) as u8;
        pixels.push(if light { Color32::from_white_alpha(a) } else { Color32::from_black_alpha(a) });
    }
    let options = TextureOptions { wrap_mode: TextureWrapMode::Repeat, ..TextureOptions::NEAREST };
    let t = ctx.load_texture(
        if light { "gear-grain-light" } else { "gear-grain-dark" },
        ColorImage::new([SIDE, SIDE], pixels),
        options,
    );
    ctx.data_mut(|d| d.insert_temp(key, t.clone()));
    t
}

/// The ground: the finish's flat colour with a faint grain and an 8 %
/// vignette toward the corners. Screens, rails and the rack panel sit on it.
pub fn ground(p: &Painter, r: Rect, s: &GearSkin) {
    p.rect_filled(r, CornerRadius::ZERO, s.ground);
    grain(p, r, s, s.grain);
    vignette(p, r, 0.08);
}

/// Grain over `r` at alpha `k`.
pub fn grain(p: &Painter, r: Rect, s: &GearSkin, k: f32) {
    if k <= 0.0 {
        return;
    }
    let ctx = p.ctx();
    let tex = grain_texture(ctx, s.grain_light);
    let ppp = ctx.pixels_per_point();
    let scale = ppp / 128.0;
    let uv =
        Rect::from_min_max(Pos2::new(r.min.x * scale, r.min.y * scale), Pos2::new(r.max.x * scale, r.max.y * scale));
    let tint = if s.grain_light {
        Color32::from_white_alpha((k * 255.0) as u8)
    } else {
        Color32::from_black_alpha((k * 255.0) as u8)
    };
    p.image(tex.id(), r, uv, tint);
}

/// Grain over a rounded rectangle (it stays inside the corners).
pub fn grain_rounded(p: &Painter, r: Rect, radius: f32, s: &GearSkin, k: f32) {
    if k <= 0.0 {
        return;
    }
    let ctx = p.ctx();
    let tex = grain_texture(ctx, s.grain_light);
    let scale = ctx.pixels_per_point() / 128.0;
    let tint = if s.grain_light {
        Color32::from_white_alpha((k * 255.0) as u8)
    } else {
        Color32::from_black_alpha((k * 255.0) as u8)
    };
    let mesh = ring_mesh(p, r, radius, Some(tex.id()), |q| Pos2::new(q.x * scale, q.y * scale), |_, _, _| tint);
    p.add(Shape::mesh(mesh));
}

/// Darkens toward the corners of `r` by `k` at the farthest corner.
pub fn vignette(p: &Painter, r: Rect, k: f32) {
    let c = r.center();
    let reach = (r.size() / 2.0).length().max(1.0);
    rounded_fill(p, r, 0.0, move |q| {
        let d = ((q - c).length() / reach).clamp(0.0, 1.0);
        Color32::from_black_alpha((k * d * d * 255.0) as u8)
    });
}

/// A seam between two faces of the rack: a dark line with a light one below.
pub fn seam(p: &Painter, from: Pos2, to: Pos2, s: &GearSkin) {
    let below = Vec2::new(0.0, 1.0);
    p.line_segment([from, to], Stroke::new(1.0, Color32::from_black_alpha(if s.light() { 70 } else { 160 })));
    p.line_segment([from + below, to + below], Stroke::new(1.0, Color32::from_white_alpha((s.etch * 160.0) as u8)));
}

/// Insets a recess with corner `radius`: dark along the top and left edges,
/// light along the bottom and right, fading in over `depth` and following
/// the corners' curve.
fn inset(p: &Painter, r: Rect, radius: f32, depth: f32, dark: f32, light: f32) {
    let depth = depth.max(1.0);
    rounded_shade(p, r, radius, move |_, d, n| {
        let t = (1.0 - d / depth).max(0.0);
        let t = t * t;
        let shadowed = ((-n.x).max(0.0) + (-n.y).max(0.0)).min(1.0);
        let lit = (n.x.max(0.0) + n.y.max(0.0)).min(1.0);
        light_dark(light * lit * t, dark * shadowed * t)
    });
}

/// A recessed tray inside a panel (meters sit in one).
pub fn tray(p: &Painter, r: Rect, s: &GearSkin) {
    p.rect_filled(r, CornerRadius::same(6), s.well);
    inset(p, r, 6.0, 10.0, 0.22, 0.35 * s.hl.max(0.15));
}

/// A recess cut into the ground (an empty position, the matrix bed).
pub fn recess(p: &Painter, r: Rect, s: &GearSkin, radius: u8) {
    p.rect_filled(r, CornerRadius::same(radius), s.bed);
    grain_rounded(p, r, radius as f32, s, s.grain * 0.6);
    inset(p, r, radius as f32, 12.0, if s.light() { 0.14 } else { 0.35 }, 0.25 * s.etch);
    p.rect_stroke(
        r,
        CornerRadius::same(radius),
        Stroke::new(1.0, Color32::from_black_alpha(if s.light() { 40 } else { 110 })),
        StrokeKind::Inside,
    );
}

/// The well an OLED sits in, without its text.
pub fn oled_well(p: &Painter, r: Rect, s: &GearSkin) {
    let radius = CornerRadius::same(8);
    p.rect_filled(r.expand(3.0), CornerRadius::same(10), alpha(Color32::BLACK, 0.35));
    p.rect_filled(r, radius, Color32::from_rgb(6, 6, 8));
    // The rings: dark, light, dark.
    p.rect_stroke(
        r.expand(2.5),
        CornerRadius::same(10),
        Stroke::new(1.5, alpha(Color32::BLACK, 0.55)),
        StrokeKind::Middle,
    );
    p.rect_stroke(
        r.expand(1.25),
        CornerRadius::same(9),
        Stroke::new(0.75, alpha(Color32::WHITE, 0.10 + 0.25 * s.hl)),
        StrokeKind::Middle,
    );
    p.rect_stroke(r, radius, Stroke::new(1.0, alpha(Color32::BLACK, 0.8)), StrokeKind::Inside);
    // The inner shadow: four inset strokes, fading.
    for (i, a) in [0.5f32, 0.32, 0.18, 0.08].iter().enumerate() {
        let k = i as f32 + 1.0;
        p.rect_stroke(
            r.shrink(k),
            CornerRadius::same(8u8.saturating_sub(i as u8)),
            Stroke::new(1.0, alpha(Color32::BLACK, *a)),
            StrokeKind::Inside,
        );
    }
    // Scanlines.
    let mut y = r.top() + 2.0;
    while y < r.bottom() - 1.0 {
        let x = 2.0 + corner_inset(8.0, (y - r.top()).min(r.bottom() - y));
        p.line_segment(
            [Pos2::new(r.left() + x, y), Pos2::new(r.right() - x, y)],
            Stroke::new(1.0, Color32::from_white_alpha(8)),
        );
        y += 3.0;
    }
}

/// Glowing OLED text at `pos` (left-centre), pixel-snapped so the pixel
/// face stays crisp.
pub fn oled_text(p: &Painter, pos: Pos2, text: &str, size: f32, color: Color32) {
    if text.is_empty() {
        return;
    }
    let f = font(p.ctx(), "oled", size);
    let pos = pos.round();
    for d in [Vec2::new(-1.0, 0.0), Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)] {
        p.text(pos + d, Align2::LEFT_CENTER, text, f.clone(), alpha(color, 0.22));
    }
    p.text(pos, Align2::LEFT_CENTER, text, f, color);
}

/// An OLED screen in a ringed well: line 1 at 24 px, line 2 at 16 px.
pub fn oled(p: &Painter, r: Rect, s: &GearSkin, line1: &str, line2: &str, color: Color32) {
    oled_well(p, r, s);
    let p = p.with_clip_rect(r.shrink(2.0));
    let x = r.left() + 10.0;
    if line2.is_empty() {
        oled_text(&p, Pos2::new(x, r.center().y), line1, OLED_L, color);
    } else {
        oled_text(&p, Pos2::new(x, r.top() + 15.0), line1, OLED_L, color);
        oled_text(&p, Pos2::new(x, r.bottom() - 11.0), line2, OLED_S, alpha(color, 0.82));
    }
}

/// A one-line OLED (the rail's engine readout).
pub fn oled_line(p: &Painter, r: Rect, s: &GearSkin, text: &str, size: f32, color: Color32) {
    oled_well(p, r, s);
    let p = p.with_clip_rect(r.shrink(2.0));
    oled_text(&p, Pos2::new(r.left() + 8.0, r.center().y), text, size, color);
}

/// The body of a glass pill over `r`: `lit` fills it with the accent.
fn pill_body(p: &Painter, r: Rect, s: &GearSkin, pressed: f32, lit: bool) {
    let radius = CornerRadius::same((r.height() / 2.0) as u8);
    let light = s.light();
    if lit {
        p.rect_filled(r, radius, s.accent);
        reflection(p, r, r.height() / 2.0, 70.0 / 255.0, 0.5, 0.0);
        p.rect_stroke(r, radius, Stroke::new(1.0, Color32::from_black_alpha(90)), StrokeKind::Inside);
        return;
    }
    // Body: a touch darker than the panel on light finishes, lighter on dark.
    let body = if light {
        Color32::from_black_alpha((14.0 + 20.0 * pressed) as u8)
    } else {
        Color32::from_white_alpha((16.0 - 8.0 * pressed) as u8)
    };
    p.rect_filled(r, radius, body);
    // The glass reflection on the top half, and the top shade (the pill
    // sits in a recess), both inside the pill's rounded ends.
    let glass = s.glass * (1.0 - 0.5 * pressed);
    reflection(p, r, r.height() / 2.0, glass, 0.5, (50.0 + 50.0 * pressed) / 255.0);
    p.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, Color32::from_black_alpha(if light { 60 } else { 120 })),
        StrokeKind::Inside,
    );
    // The light below its bottom edge.
    p.line_segment(
        [Pos2::new(r.left() + 10.0, r.bottom() + 0.5), Pos2::new(r.right() - 10.0, r.bottom() + 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha((s.etch * 200.0 * (1.0 - pressed)) as u8)),
    );
}

/// A glass pill button.
pub fn pill(ui: &mut Ui, label: &str, s: &GearSkin) -> Response {
    pill_labeled(ui, label, label, s)
}

/// A glass pill showing `label`, announced to screen readers (and tests) as
/// `accessible` (e.g. "Turn on" on the VASIO B card: "Turn on VASIO B").
pub fn pill_labeled(ui: &mut Ui, label: &str, accessible: &str, s: &GearSkin) -> Response {
    pill_full(ui, label, accessible, s, false, None)
}

/// A pill that is lit (filled with the accent) while `lit`: tabs, scenes.
pub fn pill_lit(ui: &mut Ui, label: &str, accessible: &str, lit: bool, s: &GearSkin) -> Response {
    pill_full(ui, label, accessible, s, lit, None)
}

/// A pill `tint`ed in a colour (the ✓ / ✕ of an in-place confirmation).
pub fn pill_tinted(ui: &mut Ui, label: &str, accessible: &str, tint: Color32, s: &GearSkin) -> Response {
    pill_full(ui, label, accessible, s, false, Some(tint))
}

fn pill_full(ui: &mut Ui, label: &str, accessible: &str, s: &GearSkin, lit: bool, tint: Option<Color32>) -> Response {
    let f = font(ui.ctx(), "label-bold", 12.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), f.clone(), s.ink);
    let size = Vec2::new(galley.size().x + 18.0, PILL_H);
    let (r, resp) = ui.allocate_exact_size(size, Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, accessible));
    if !ui.is_rect_visible(r) {
        return resp;
    }
    let p = ui.painter();
    let pressed = ui.ctx().animate_value_with_time(
        resp.id.with("press"),
        if resp.is_pointer_button_down_on() { 1.0 } else { 0.0 },
        0.08,
    );
    let hover = ui.ctx().animate_value_with_time(resp.id.with("hover"), if resp.hovered() { 1.0 } else { 0.0 }, 0.12);
    let r = r.translate(Vec2::new(0.0, pressed));
    pill_body(p, r, s, pressed, lit);
    if let Some(t) = tint {
        p.rect_filled(r, CornerRadius::same((PILL_H / 2.0) as u8), alpha(t, 0.35));
    }
    let ink = if lit {
        super::skins::ink_on(s.accent)
    } else if !enabled {
        alpha(s.ink, 0.4)
    } else {
        alpha(s.ink, 0.82 + 0.18 * hover)
    };
    p.galley(r.center() - galley.size() / 2.0, galley, ink);
    resp
}

/// A small round colour swatch button.
pub fn swatch(ui: &mut Ui, color: Color32, accessible: &str, s: &GearSkin) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(PILL_H), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, accessible));
    let p = ui.painter();
    let c = r.center();
    pill_body(p, r, s, 0.0, false);
    p.circle_filled(c, 7.0, color);
    p.circle_filled(c - Vec2::new(2.0, 2.5), 2.5, Color32::from_white_alpha(90));
    p.circle_stroke(c, 7.0, Stroke::new(1.0, Color32::from_black_alpha(100)));
    resp
}

/// An LED: a small dome, lit (with a glow that blooms by `glow`, 0..1) or dark.
pub fn led(p: &Painter, center: Pos2, s: &GearSkin, color: Color32, lit: bool) {
    led_glow(p, center, s, color, if lit { 1.0 } else { 0.0 });
}

/// An LED whose light is at `glow` (0 dark, 1 lit, above 1 blooming).
pub fn led_glow(p: &Painter, center: Pos2, s: &GearSkin, color: Color32, glow: f32) {
    let r = 6.0;
    let glow = glow.max(0.0);
    if glow > 0.02 {
        for (k, a) in [(2.6f32, 0.08f32), (1.9, 0.14), (1.4, 0.22)] {
            p.circle_filled(center, r * k * (0.8 + 0.2 * glow.min(2.0)), alpha(color, a * glow.min(1.0)));
        }
    }
    p.circle_filled(center + Vec2::new(0.0, 1.0), r + 1.0, alpha(Color32::WHITE, 0.2 * s.hl + 0.05));
    p.circle_filled(center - Vec2::new(0.0, 0.5), r + 1.0, alpha(Color32::BLACK, 0.45));
    let dark = Color32::from_rgb(color.r() / 5, color.g() / 5, color.b() / 5);
    let body = mix(dark, color, glow.min(1.0));
    let edge =
        Color32::from_rgb((body.r() as f32 * 0.6) as u8, (body.g() as f32 * 0.6) as u8, (body.b() as f32 * 0.6) as u8);
    p.circle_filled(center, r, edge);
    p.circle_filled(center, r * 0.75, body);
    let hot = 0.4 + 0.35 * glow.min(1.0);
    p.circle_filled(center - Vec2::new(1.5, 1.5), r * 0.32, alpha(Color32::WHITE, hot));
}

/// A label etched in the panel: a light copy below, then the ink.
pub fn etched(p: &Painter, pos: Pos2, text: &str, s: &GearSkin) {
    etched_text(p, pos, Align2::LEFT_CENTER, text, s, s.ink, 11.0, true, 0.12, 0.8);
}

/// Etched text: `bold`, letter-spaced by `tracking` em, at `ink_alpha`.
#[allow(clippy::too_many_arguments)]
pub fn etched_text(
    p: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    s: &GearSkin,
    ink: Color32,
    size: f32,
    bold: bool,
    tracking: f32,
    ink_alpha: f32,
) -> Rect {
    let ctx = p.ctx();
    let f = font(ctx, if bold { "label-bold" } else { "label" }, size);
    let light = Color32::from_white_alpha((s.etch * 255.0) as u8);
    let job = |color: Color32| {
        let mut job = LayoutJob::default();
        job.append(
            text,
            0.0,
            TextFormat { font_id: f.clone(), color, extra_letter_spacing: tracking * size, ..Default::default() },
        );
        job
    };
    let below = p.layout_job(job(light));
    let main = p.layout_job(job(alpha(ink, ink_alpha)));
    let rect = align.anchor_size(pos, main.size());
    if !s.light() || s.etch > 0.2 {
        p.galley(rect.min + Vec2::new(0.0, 1.0), below, light);
    }
    p.galley(rect.min, main.clone(), alpha(ink, ink_alpha));
    rect
}

/// A small uppercase etched tag ("ASIO 1", "MASTER").
pub fn tag(p: &Painter, pos: Pos2, align: Align2, text: &str, s: &GearSkin, ink_alpha: f32) -> Rect {
    etched_text(p, pos, align, &text.to_uppercase(), s, s.ink, 10.5, true, 0.12, ink_alpha)
}

/// Text in one line, cut with an ellipsis at `max_width`.
pub fn truncated(p: &Painter, pos: Pos2, align: Align2, text: &str, f: FontId, color: Color32, max_width: f32) -> Rect {
    let mut job = LayoutJob::simple_singleline(text.to_string(), f, color);
    job.wrap = egui::text::TextWrapping::from_wrap_mode_and_width(TextWrapMode::Truncate, max_width.max(8.0));
    let galley = p.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    p.galley(rect.min, galley, color);
    rect
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

/// The S-slope knob: computed light and shade over the panel, the cap with
/// an etched pointer, and the value arc. Drag up/down or scroll to change
/// `value` (0..1). `size` is the side of the square it takes, its cast
/// shadow included.
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
    // The shown value follows the real one on a spring, so wheel steps feel geared.
    let shown = ui.ctx().animate_value_with_time(resp.id.with("value"), *value, 0.12);
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
    let dragging = resp.dragged();
    arc(p, c, arc_r, 0.0, 1.0, Stroke::new(3.0, alpha(s.ink, 0.10)));
    arc(p, c, arc_r, 0.0, shown, Stroke::new(if dragging { 9.0 } else { 7.0 }, alpha(arc_color, 0.28)));
    arc(p, c, arc_r, 0.0, shown, Stroke::new(3.0, arc_color));
    // The pointer: an etched line from 55 % to 85 % of the cap radius.
    let cap_dark = super::skins::luminance(cap) < 0.4;
    let (a, b) = (arc_point(c, cap_r * 0.55, shown), arc_point(c, cap_r * 0.85, shown));
    p.line_segment(
        [a + Vec2::new(0.0, 1.0), b + Vec2::new(0.0, 1.0)],
        Stroke::new(2.0, if cap_dark { Color32::from_black_alpha(120) } else { Color32::from_white_alpha(160) }),
    );
    p.line_segment(
        [a, b],
        Stroke::new(2.0, if cap_dark { Color32::from_white_alpha(140) } else { Color32::from_black_alpha(140) }),
    );
    let f = font(ui.ctx(), "label-bold", (cap_r * 0.42).clamp(9.0, 22.0));
    let ink = if cap_dark { Color32::from_rgb(0xf0, 0xf0, 0xf4) } else { Color32::from_rgb(0x30, 0x32, 0x36) };
    p.text(c, Align2::CENTER_CENTER, label, f, ink);
    resp
}

const METER_GREEN: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x64);
const METER_YELLOW: Color32 = Color32::from_rgb(0xff, 0xcf, 0x3a);
const METER_RED: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);

/// The fraction of a meter's travel for `db`: −60 dB empty, 0 dB full.
pub fn meter_frac(db: f32) -> f32 {
    if db.is_finite() {
        ((db + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// A level meter in a tray, `vertical` or along its width: green to −12,
/// yellow to −3, red above; `level` is the ballistic level and `hold` the
/// held peak (dB).
pub fn meter(p: &Painter, r: Rect, level_db: f32, hold_db: f32, vertical: bool) {
    let zones = [
        (0.0, meter_frac(-12.0), METER_GREEN),
        (meter_frac(-12.0), meter_frac(-3.0), METER_YELLOW),
        (meter_frac(-3.0), 1.0, METER_RED),
    ];
    let level = meter_frac(level_db);
    let span = |lo: f32, hi: f32| {
        if vertical {
            Rect::from_min_max(
                Pos2::new(r.left(), r.bottom() - hi * r.height()),
                Pos2::new(r.right(), r.bottom() - lo * r.height()),
            )
        } else {
            Rect::from_min_max(
                Pos2::new(r.left() + lo * r.width(), r.top()),
                Pos2::new(r.left() + hi * r.width(), r.bottom()),
            )
        }
    };
    for (lo, hi, col) in zones {
        p.rect_filled(span(lo, hi), CornerRadius::ZERO, alpha(col, 0.16));
        let top = hi.min(level);
        if top > lo {
            p.rect_filled(span(lo, top), CornerRadius::ZERO, col);
        }
    }
    let hold = meter_frac(hold_db);
    if hold > 0.0 {
        let col = if hold_db > -3.0 {
            METER_RED
        } else if hold_db > -12.0 {
            METER_YELLOW
        } else {
            Color32::WHITE
        };
        let seg = if vertical {
            let y = r.bottom() - hold * r.height();
            [Pos2::new(r.left(), y), Pos2::new(r.right(), y)]
        } else {
            let x = r.left() + hold * r.width();
            [Pos2::new(x, r.top()), Pos2::new(x, r.bottom())]
        };
        p.line_segment(seg, Stroke::new(2.0, col));
    }
}

/// An empty crosspoint: a small dot.
pub fn pin(p: &Painter, center: Pos2, ink: Color32, k: f32) {
    p.circle_filled(center, 1.2, alpha(ink, k));
}

/// A routed crosspoint: a raised square in `color`, lit from the top-left.
pub fn raised(p: &Painter, r: Rect, color: Color32, radius: f32) {
    let cr = CornerRadius::same(radius as u8);
    p.rect_filled(r.translate(Vec2::new(1.0, 1.5)), cr, Color32::from_black_alpha(90));
    p.rect_filled(r, cr, color);
    reflection(p, r, radius, 60.0 / 255.0, 0.5, 0.0);
    p.line_segment(
        [r.left_top() + Vec2::new(radius, 0.5), r.right_top() + Vec2::new(-radius, 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha(110)),
    );
    p.line_segment(
        [r.left_bottom() + Vec2::new(radius, -0.5), r.right_bottom() + Vec2::new(-radius, -0.5)],
        Stroke::new(1.0, Color32::from_black_alpha(90)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Distance by which `q` lies outside the rounded rectangle (0 inside).
    fn outside(r: Rect, radius: f32, q: Pos2) -> f32 {
        let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
        let inner = r.shrink(rad);
        let dx = (inner.left() - q.x).max(q.x - inner.right()).max(0.0);
        let dy = (inner.top() - q.y).max(q.y - inner.bottom()).max(0.0);
        ((dx * dx + dy * dy).sqrt() - rad).max(0.0)
    }

    #[test]
    fn shading_meshes_stay_inside_their_rounded_corners() {
        let ctx = egui::Context::default();
        let p = Painter::new(ctx, egui::LayerId::background(), Rect::EVERYTHING);
        for (r, radius) in [
            (Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(236.0, 188.0)), 16.0),
            (Rect::from_min_size(Pos2::ZERO, Vec2::new(80.0, 24.0)), 12.0),
            (Rect::from_min_size(Pos2::ZERO, Vec2::new(212.0, 30.0)), 6.0),
            (Rect::from_min_size(Pos2::ZERO, Vec2::new(18.0, 18.0)), 4.0),
        ] {
            let m = ring_mesh(&p, r, radius, None, |_| egui::epaint::WHITE_UV, |_, _, _| Color32::WHITE);
            let feather = 1.0 + 1e-3;
            for v in &m.vertices {
                let out = outside(r, radius, v.pos);
                assert!(out <= feather, "{:?} is {out} px outside {r:?} r{radius}", v.pos);
                if v.color != Color32::TRANSPARENT {
                    assert!(out <= 1e-3, "a coloured vertex {:?} is {out} px outside", v.pos);
                }
            }
            assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        }
    }

    #[test]
    fn plain_shading_samples_the_white_texel() {
        let ctx = egui::Context::default();
        let p = Painter::new(ctx, egui::LayerId::background(), Rect::EVERYTHING);
        let r = Rect::from_min_size(Pos2::new(40.0, 30.0), Vec2::new(236.0, 188.0));
        let m = plain_mesh(&p, r, 16.0, |_, _, _| Color32::RED);
        assert!(m.vertices.iter().all(|v| v.uv == egui::epaint::WHITE_UV));
    }

    #[test]
    fn scanlines_stop_where_the_corner_curves() {
        assert_eq!(corner_inset(8.0, 8.0), 0.0);
        assert_eq!(corner_inset(8.0, 20.0), 0.0);
        assert!((corner_inset(8.0, 0.0) - 8.0).abs() < 1e-4);
        assert!(corner_inset(8.0, 2.0) > 1.5 && corner_inset(8.0, 2.0) < 8.0);
    }

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

    #[test]
    fn meters_span_sixty_db() {
        assert_eq!(meter_frac(-60.0), 0.0);
        assert_eq!(meter_frac(0.0), 1.0);
        assert_eq!(meter_frac(f32::NEG_INFINITY), 0.0);
        assert!((meter_frac(-30.0) - 0.5).abs() < 1e-6);
    }

    /// A long device name is cut with an ellipsis, not clipped mid-glyph.
    #[test]
    fn long_text_is_truncated_with_an_ellipsis() {
        let ctx = egui::Context::default();
        let mut widths = Vec::new();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let f = FontId::proportional(14.0);
            let r = truncated(
                ui.painter(),
                Pos2::new(10.0, 10.0),
                Align2::LEFT_TOP,
                "Broadcast Stream Mix (4- TC-HELICON GoXLR)",
                f,
                Color32::WHITE,
                120.0,
            );
            widths.push(r.width());
        });
        out.textures_delta.clear(); // a frame's texture changes must be handled
        assert!(widths[0] <= 121.0, "{}", widths[0]);
    }
}
