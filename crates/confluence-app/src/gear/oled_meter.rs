//! White-pixel OLED meters, in the manner of a hardware interface's front
//! panel (spec: meter bridge §4.1). Every channel has a bar of fixed width
//! (bars never stretch to fill), silent channels stay as dim ladders so the
//! channel count is plain, and each bar has its own clip block on top.
//! Three styles (segments, dot-matrix, solid) share one layout.

use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, Response, Sense, Ui, Vec2};
use egui::epaint::{Mesh, Shape};
use serde::{Deserialize, Serialize};

use super::motion::{Motion, SILENT_DB};
use super::pixel_font;

/// How the bars are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MeterStyle {
    /// Thin white segments, like a hardware interface's front panel.
    #[default]
    Segments,
    /// Square pixels on a visible grid, numbers included.
    DotMatrix,
    /// Smooth bars: RMS body, peak extension, hold line.
    Solid,
}

impl MeterStyle {
    pub fn all() -> [MeterStyle; 3] {
        [MeterStyle::Segments, MeterStyle::DotMatrix, MeterStyle::Solid]
    }

    pub fn name(self) -> &'static str {
        match self {
            MeterStyle::Segments => "Segments",
            MeterStyle::DotMatrix => "Dot-matrix",
            MeterStyle::Solid => "Solid",
        }
    }
}

/// The user's choices for how meters look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterLook {
    pub style: MeterStyle,
    /// The held peak as two lines instead of one.
    pub double_peak: bool,
    /// Clip blocks in red instead of white.
    pub clip_red: bool,
}

/// One channel as the engine reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chan {
    /// Shown under its bar (1-based).
    pub number: u32,
    /// Shown on hover.
    pub name: String,
    pub peak_db: f32,
    pub rms_db: f32,
    pub clipped: bool,
}

impl Chan {
    pub fn silent() -> Chan {
        Chan { number: 1, name: String::new(), peak_db: SILENT_DB, rms_db: SILENT_DB, clipped: false }
    }
}

/// A run of channels drawn together under one label ("IN 23").
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub label: String,
    pub channels: Vec<Chan>,
}

/// Sizes that differ between a card's meter and the bridge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    pub bar_w: f32,
    pub gap: f32,
    pub group_gap: f32,
    /// The dB scale on the left, if there is room.
    pub scale: bool,
    /// The loudest peak as digits on the right, if there is room.
    pub readout: bool,
    /// Points per font pixel for the readout.
    pub readout_px: f32,
    /// Dot columns per bar in the dot-matrix style.
    pub dot_cols: usize,
    /// Set by [`resolve`](Self::resolve) for the dot-matrix: (dot px, gap px,
    /// pixels per point). Bars are then whole dot columns, a whole number of
    /// dot cells apart.
    pub dot: Option<(u32, u32, f32)>,
}

impl Geom {
    pub fn card() -> Geom {
        Geom {
            bar_w: 8.0,
            gap: 3.0,
            group_gap: 10.0,
            scale: false,
            readout: true,
            readout_px: 1.0,
            dot_cols: 3,
            dot: None,
        }
    }

    pub fn bridge() -> Geom {
        Geom {
            bar_w: 11.0,
            gap: 3.0,
            group_gap: 12.0,
            scale: true,
            readout: false,
            readout_px: 2.0,
            dot_cols: 4,
            dot: None,
        }
    }

    /// This geometry for `style` on a screen with `ppp` pixels per point,
    /// bars about `height` tall: the dot-matrix takes its bar width and gaps
    /// from whole dot cells; the other styles are unchanged.
    pub fn resolve(self, style: MeterStyle, ppp: f32, height: f32) -> Geom {
        if style != MeterStyle::DotMatrix {
            return Geom { dot: None, ..self };
        }
        let (d, g) = PixelGrid { ppp }.dots_px(height);
        self.with_dots(self.dot_cols, d, g, ppp, true)
    }

    /// Bars `cols` dots wide; `spaced` leaves one empty dot column between bars.
    fn with_dots(self, cols: usize, d: u32, g: u32, ppp: f32, spaced: bool) -> Geom {
        let (dot, gap) = (d as f32 / ppp, g as f32 / ppp);
        let cell = dot + gap;
        let bar_w = dot_bar_width(cols, dot, gap);
        let bar_gap = if spaced { gap + cell } else { gap };
        // Groups start a whole number of cells after the previous group's last bar.
        let cells = ((bar_w + self.group_gap) / cell).ceil().max(2.0);
        Geom { bar_w, gap: bar_gap, group_gap: cells * cell - bar_w, dot_cols: cols, dot: Some((d, g, ppp)), ..self }
    }
}

/// The width the bars of `groups` take with `g`.
fn bars_width(groups: &[Group], g: &Geom) -> f32 {
    let counts: Vec<usize> = groups.iter().map(|gr| gr.channels.len()).filter(|&n| n > 0).collect();
    counts.iter().map(|&n| n as f32 * (g.bar_w + g.gap) - g.gap).sum::<f32>()
        + g.group_gap * counts.len().saturating_sub(1) as f32
}

/// `g` if the meter fits `width`; otherwise narrower bars that do. The
/// dot-matrix drops dot columns (down to one), then the empty column between
/// bars, then shrinks to one-pixel dots; the other styles narrow their bars
/// evenly (down to one point) with tighter gaps.
pub fn fit_geom(groups: &[Group], width: f32, g: Geom) -> Geom {
    let fits = |c: &Geom| bars_width(groups, c) <= width + 0.01;
    if fits(&g) {
        return g;
    }
    if let Some((d, gp, ppp)) = g.dot {
        let mut tries: Vec<Geom> = (1..g.dot_cols).rev().map(|cols| g.with_dots(cols, d, gp, ppp, true)).collect();
        tries.push(g.with_dots(1, d, gp, ppp, false));
        tries.push(g.with_dots(1, 1, 1, ppp, false));
        let last = tries[tries.len() - 1];
        return tries.into_iter().find(|c| fits(c)).unwrap_or(last);
    }
    let n: usize = groups.iter().map(|gr| gr.channels.len()).sum();
    let k = groups.iter().filter(|gr| !gr.channels.is_empty()).count();
    if n == 0 {
        return g;
    }
    let gap = g.gap.min(1.0);
    let group_gap = g.group_gap.min(4.0);
    let room = width - (n - k) as f32 * gap - k.saturating_sub(1) as f32 * group_gap;
    let bar_w = ((room / n as f32 * 4.0).floor() / 4.0).clamp(1.0, g.bar_w);
    Geom { bar_w, gap, group_gap, ..g }
}

/// One bar: which group and channel, and where.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub group: usize,
    pub chan: usize,
    pub rect: Rect,
}

/// Where everything of a meter goes in its rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct MeterLayout {
    pub bars: Vec<Bar>,
    /// One clip block above each bar, in the same order.
    pub clips: Vec<Rect>,
    /// Channel numbers: top-left of the text, and the text.
    pub numbers: Vec<(Pos2, String)>,
    pub group_labels: Vec<(Pos2, String)>,
    /// dB scale marks: (y, label), left of the bars.
    pub scale: Vec<(f32, String)>,
    pub scale_x: f32,
    /// Where the peak readout goes, if it has room.
    pub readout: Option<Rect>,
    /// Width the bars take (fixed by the channel count, not the space).
    pub width_used: f32,
    /// The bars need more width than there is.
    pub overflow: bool,
    /// The dot-matrix's (dot px, gap px), from the geometry.
    pub dot: Option<(u32, u32)>,
}

/// Rows of the layout, top to bottom, in points.
const LABEL_H: f32 = 5.0;
const CLIP_H: f32 = 3.0;
const ROW_GAP: f32 = 2.0;
const NUMBER_H: f32 = 5.0;
/// Space for the scale labels ("-48") left of the bars.
const SCALE_W: f32 = 16.0;
/// The scale marks, in dB.
const MARKS: [i32; 5] = [0, -6, -12, -24, -48];

/// Lays out `groups` in `r` (pure: no painting).
pub fn meter_layout(groups: &[Group], r: Rect, g: &Geom) -> MeterLayout {
    let pitch = g.bar_w + g.gap;
    let counts: Vec<usize> = groups.iter().map(|gr| gr.channels.len()).collect();
    let bars_w: f32 = counts.iter().map(|&n| if n == 0 { 0.0 } else { n as f32 * pitch - g.gap }).sum::<f32>()
        + g.group_gap * counts.iter().filter(|&&n| n > 0).count().saturating_sub(1) as f32;
    let readout_w = pixel_font::width("-00.0", g.readout_px) + 6.0;
    let mut left = r.left();
    let scale_x = left;
    let with_scale = g.scale && bars_w + SCALE_W <= r.width();
    if with_scale {
        left += SCALE_W;
    }
    let room_after = r.right() - (left + bars_w);
    let readout = (g.readout && room_after >= readout_w).then(|| {
        let h = pixel_font::H as f32 * g.readout_px;
        Rect::from_min_size(
            Pos2::new(r.right() - readout_w + 6.0, r.bottom() - NUMBER_H - h - ROW_GAP),
            Vec2::new(readout_w - 6.0, h),
        )
    });
    let bar_top = r.top() + LABEL_H + ROW_GAP + CLIP_H + ROW_GAP;
    let bar_bottom = (r.bottom() - NUMBER_H - ROW_GAP).max(bar_top + 4.0);
    let number_y = bar_bottom + ROW_GAP;
    let mut bars = Vec::new();
    let mut clips = Vec::new();
    let mut numbers = Vec::new();
    let mut group_labels = Vec::new();
    let mut x = left;
    for (gi, gr) in groups.iter().enumerate() {
        if gr.channels.is_empty() {
            continue;
        }
        group_labels.push((Pos2::new(x, r.top()), gr.label.clone()));
        let widest = gr.channels.iter().map(|c| pixel_font::width(&c.number.to_string(), 1.0)).fold(0.0, f32::max);
        let step = [1usize, 2, 4, 8, 16, 32].into_iter().find(|s| *s as f32 * pitch >= widest + 1.0).unwrap_or(64);
        let last = gr.channels.len() - 1;
        let mut shown: Vec<(Pos2, String)> = Vec::new();
        for (ci, c) in gr.channels.iter().enumerate() {
            let rect = Rect::from_min_max(Pos2::new(x, bar_top), Pos2::new(x + g.bar_w, bar_bottom));
            bars.push(Bar { group: gi, chan: ci, rect });
            clips.push(Rect::from_min_size(Pos2::new(x, bar_top - ROW_GAP - CLIP_H), Vec2::new(g.bar_w, CLIP_H)));
            if ci % step == 0 || ci == last {
                let text = c.number.to_string();
                let w = pixel_font::width(&text, 1.0);
                let at = Pos2::new(x + (g.bar_w - w) / 2.0, number_y);
                // The last one always shows: it displaces a neighbour too close to it.
                if ci == last {
                    while shown.last().is_some_and(|(p, t)| p.x + pixel_font::width(t, 1.0) >= at.x) {
                        shown.pop();
                    }
                }
                shown.push((at, text));
            }
            x += pitch;
        }
        numbers.extend(shown);
        x += g.group_gap - g.gap;
    }
    let scale = if with_scale {
        let h = bar_bottom - bar_top;
        MARKS
            .iter()
            .map(|&db| (bar_bottom - h * frac(db as f32), db.to_string()))
            .filter(|(y, _)| *y >= bar_top - 1.0)
            .collect()
    } else {
        Vec::new()
    };
    MeterLayout {
        bars,
        clips,
        numbers,
        group_labels,
        scale,
        scale_x,
        readout,
        width_used: bars_w,
        overflow: left + bars_w > r.right() + 0.01,
        dot: g.dot.map(|(d, gap, _)| (d, gap)),
    }
}

/// A level as the fraction of a bar: −60 dB empty, 0 dB full.
pub fn frac(db: f32) -> f32 {
    if db.is_finite() {
        ((db + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The segment(s) the held peak lights in a bar of `n` segments: the one at
/// `hold` (a fraction), and with `double` the one below it too.
pub fn hold_segments(hold: f32, n: usize, double: bool) -> Option<(usize, Option<usize>)> {
    if hold <= 0.0 || n == 0 {
        return None;
    }
    let top = ((hold * n as f32).round() as usize).clamp(1, n) - 1;
    Some((top, (double && top > 0).then(|| top - 1)))
}

/// The loudest held peak as digits ("-3.3", "+0.4"), or "-INF" for silence.
pub fn readout(holds: &[f32]) -> String {
    let m = holds.iter().copied().filter(|h| h.is_finite() && *h > SILENT_DB + 0.5).fold(f32::NEG_INFINITY, f32::max);
    if !m.is_finite() {
        "-INF".into()
    } else if m >= 0.0 {
        format!("+{m:.1}")
    } else {
        format!("{m:.1}")
    }
}

const LIT: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
const RED: Color32 = Color32::from_rgb(0xff, 0x3b, 0x30);

fn dim(k: f32) -> Color32 {
    Color32::from_white_alpha((k * 255.0) as u8)
}

/// The most rows a segment or dot ladder has: taller bars space them out.
pub const MAX_ROWS: usize = 120;

/// The screen's physical pixels, for geometry that must land on whole pixels
/// (the dot-matrix: square dots with real gaps at any display scale).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelGrid {
    /// Physical pixels per point.
    pub ppp: f32,
}

impl PixelGrid {
    /// `v` points as a whole number of pixels (at least one), in points.
    pub fn snap(&self, v: f32) -> f32 {
        (v * self.ppp).round().max(1.0) / self.ppp
    }

    /// The pixel boundary nearest the coordinate `x`.
    pub fn snap_pos(&self, x: f32) -> f32 {
        (x * self.ppp).round() / self.ppp
    }

    /// The dot and gap sizes in whole pixels for a ladder `height` tall:
    /// 2-point dots with 1-point gaps, grown together while the ladder would
    /// have more than [`MAX_ROWS`] rows.
    pub fn dots_px(&self, height: f32) -> (u32, u32) {
        let mut k = 1.0f32;
        loop {
            let dot = (2.0 * k * self.ppp).round().max(2.0);
            let gap = (k * self.ppp).round().max(1.0);
            if height * self.ppp / (dot + gap) <= MAX_ROWS as f32 || k > 64.0 {
                return (dot as u32, gap as u32);
            }
            k += 0.5;
        }
    }

    /// [`dots_px`](Self::dots_px) in points: (dot, gap).
    pub fn dots(&self, height: f32) -> (f32, f32) {
        let (d, g) = self.dots_px(height);
        (d as f32 / self.ppp, g as f32 / self.ppp)
    }
}

/// The width of a dot-matrix bar `cols` dots wide (at least one).
pub fn dot_bar_width(cols: usize, dot: f32, gap: f32) -> f32 {
    let cols = cols.max(1) as f32;
    cols * dot + (cols - 1.0) * gap
}

/// How many dot columns a bar `width` points wide holds. Counted from the
/// width, not the edges: a bar's left edge rounds onto the dot grid, and an
/// edge test would then drop its last column.
pub fn dot_columns(width: f32, ppp: f32, dot_px: u32, gap_px: u32) -> usize {
    ((width * ppp + gap_px as f32) / (dot_px + gap_px) as f32).round().max(1.0) as usize
}

/// The (dot, gap) in pixels the background grid uses behind a meter laid out
/// with `geom`: the geometry's own dots, so lit dots sit on the grid.
pub fn grid_dots(geom: &Geom, ppp: f32, height: f32) -> (u32, u32) {
    geom.dot.map(|(d, g, _)| (d, g)).unwrap_or_else(|| PixelGrid { ppp }.dots_px(height))
}

/// The faint pixel grid of an OLED behind the dot-matrix, over `r`: one quad
/// with a repeating one-dot texture, anchored to the screen so it lines up
/// with the meters' dots.
pub fn dot_grid(p: &egui::Painter, r: Rect, (dot, gap): (u32, u32)) {
    let grid = PixelGrid { ppp: p.ctx().pixels_per_point() };
    let pitch = (dot + gap) as usize;
    let key = Id::new(("oled-dot-grid", dot, gap));
    let tex = p.ctx().data_mut(|d| d.get_temp::<egui::TextureHandle>(key)).unwrap_or_else(|| {
        let mut img = egui::ColorImage::filled([pitch, pitch], Color32::TRANSPARENT);
        for y in 0..dot as usize {
            for x in 0..dot as usize {
                img[(x, y)] = Color32::WHITE;
            }
        }
        let t = p.ctx().load_texture("oled-dot-grid", img, egui::TextureOptions::NEAREST_REPEAT);
        p.ctx().data_mut(|d| d.insert_temp(key, t.clone()));
        t
    });
    let r = Rect::from_min_max(
        Pos2::new(grid.snap_pos(r.left()), grid.snap_pos(r.top())),
        Pos2::new(grid.snap_pos(r.right()), grid.snap_pos(r.bottom())),
    );
    // One tile is `pitch` pixels; tiles start at the screen's origin.
    let tile = pitch as f32 / grid.ppp;
    let uv = Rect::from_min_max((r.min.to_vec2() / tile).to_pos2(), (r.max.to_vec2() / tile).to_pos2());
    let mut m = Mesh::with_texture(tex.id());
    m.add_rect_with_uv(r, uv, Color32::from_white_alpha(9));
    p.add(Shape::mesh(m));
}

/// The row pitch of a ladder `height` tall: fine on small meters, coarser on
/// very tall ones (a maximized pop-out) so the mesh stays small.
pub fn ladder_pitch(height: f32, dot: bool) -> f32 {
    let fine: f32 = if dot { 2.0 } else { 3.0 };
    fine.max(height / MAX_ROWS as f32)
}

/// Paints a laid-out meter. `levels` holds each bar's (level, held peak, rms)
/// in dB, in bar order.
pub fn paint_meter(p: &egui::Painter, l: &MeterLayout, groups: &[Group], levels: &[(f32, f32, f32)], look: MeterLook) {
    let dot = look.style == MeterStyle::DotMatrix;
    let grid = PixelGrid { ppp: p.ctx().pixels_per_point() };
    let mut m = Mesh::default();
    let clip_on = if look.clip_red { RED } else { LIT };
    for (i, b) in l.bars.iter().enumerate() {
        let (level, hold, rms) = levels.get(i).copied().unwrap_or((SILENT_DB, SILENT_DB, SILENT_DB));
        let (lv, hv) = (frac(level), frac(hold));
        let r = b.rect;
        match look.style {
            MeterStyle::DotMatrix => {
                // Square dots on the screen's pixel grid (the same grid as
                // `dot_grid`): cells `pitch` pixels apart from the origin.
                let (dot_px, gap_px) = l.dot.unwrap_or_else(|| grid.dots_px(r.height()));
                let pitch_px = (dot_px + gap_px) as f32;
                let ppp = grid.ppp;
                let cell = |v: f32| v * pitch_px / ppp;
                let (dot_pt, x0) = (dot_px as f32 / ppp, (r.left() * ppp / pitch_px).round());
                let cols = dot_columns(r.width(), ppp, dot_px, gap_px);
                let top = (r.top() * ppp / pitch_px).ceil();
                let bottom = ((r.bottom() * ppp - dot_px as f32) / pitch_px).floor();
                let n = ((bottom - top) + 1.0).max(1.0) as usize;
                let held = hold_segments(hv, n, look.double_peak);
                for k in 0..n {
                    let y = cell(bottom - k as f32);
                    let on =
                        (k + 1) as f32 / n as f32 <= lv + 1e-4 || held.is_some_and(|(a, b)| k == a || Some(k) == b);
                    let colour = if on { LIT } else { dim(0.10) };
                    for c in 0..cols {
                        let x = cell(x0 + c as f32);
                        m.add_colored_rect(Rect::from_min_size(Pos2::new(x, y), Vec2::splat(dot_pt)), colour);
                    }
                }
            }
            MeterStyle::Segments => {
                let pitch = grid.snap(ladder_pitch(r.height(), false));
                let seg_h = grid.snap(pitch * 2.0 / 3.0);
                let n = (r.height() / pitch).floor().max(1.0) as usize;
                let held = hold_segments(hv, n, look.double_peak);
                let (left, right, bottom) =
                    (grid.snap_pos(r.left()), grid.snap_pos(r.right()), grid.snap_pos(r.bottom()));
                for k in 0..n {
                    let y = bottom - (k + 1) as f32 * pitch + (pitch - seg_h);
                    let on =
                        (k + 1) as f32 / n as f32 <= lv + 1e-4 || held.is_some_and(|(a, b)| k == a || Some(k) == b);
                    let colour = if on { LIT } else { dim(0.10) };
                    m.add_colored_rect(Rect::from_min_max(Pos2::new(left, y), Pos2::new(right, y + seg_h)), colour);
                }
            }
            MeterStyle::Solid => {
                m.add_colored_rect(r, dim(0.08));
                let y = |f: f32| r.bottom() - r.height() * f;
                let rf = frac(rms).min(lv);
                if lv > 0.0 {
                    m.add_colored_rect(Rect::from_min_max(Pos2::new(r.left(), y(lv)), r.right_bottom()), dim(0.45));
                }
                if rf > 0.0 {
                    m.add_colored_rect(Rect::from_min_max(Pos2::new(r.left(), y(rf)), r.right_bottom()), LIT);
                }
                if hv > 0.0 {
                    let hy = y(hv) - 1.0;
                    m.add_colored_rect(Rect::from_min_size(Pos2::new(r.left(), hy), Vec2::new(r.width(), 1.5)), LIT);
                    if look.double_peak {
                        m.add_colored_rect(
                            Rect::from_min_size(Pos2::new(r.left(), hy + 3.5), Vec2::new(r.width(), 1.5)),
                            LIT,
                        );
                    }
                }
            }
        }
        let clipped = groups.get(b.group).and_then(|g| g.channels.get(b.chan)).is_some_and(|c| c.clipped);
        if let Some(c) = l.clips.get(i) {
            let colour = if clipped { clip_on } else { dim(0.06) };
            if dot {
                let (dot_px, gap_px) = l.dot.unwrap_or_else(|| grid.dots_px(r.height()));
                let pitch_px = (dot_px + gap_px) as f32;
                let ppp = grid.ppp;
                let cell = |v: f32| v * pitch_px / ppp;
                let x0 = (c.left() * ppp / pitch_px).round();
                let y = cell((c.top() * ppp / pitch_px).round());
                let cols = ((c.width() * ppp + gap_px as f32) / pitch_px).round().max(1.0) as usize;
                for k in 0..cols {
                    m.add_colored_rect(
                        Rect::from_min_size(Pos2::new(cell(x0 + k as f32), y), Vec2::splat(dot_px as f32 / ppp)),
                        colour,
                    );
                }
            } else {
                m.add_colored_rect(*c, colour);
            }
        }
    }
    // Text: in the pixel font for segments and dots (it is on the same display).
    let text_c = dim(0.62);
    if look.style == MeterStyle::Solid {
        let f = egui::FontId::new(9.0, egui::FontFamily::Monospace);
        for (pos, t) in l.numbers.iter().chain(&l.group_labels) {
            p.text(*pos, Align2::LEFT_TOP, t, f.clone(), text_c);
        }
        for (y, t) in &l.scale {
            p.text(Pos2::new(l.scale_x, *y), Align2::LEFT_CENTER, t, f.clone(), dim(0.5));
        }
    } else {
        for (pos, t) in l.numbers.iter().chain(&l.group_labels) {
            pixel_font::draw(&mut m, *pos, t, 1.0, text_c, dot);
        }
        for (y, t) in &l.scale {
            pixel_font::draw(&mut m, Pos2::new(l.scale_x, *y - 2.0), t, 1.0, dim(0.5), dot);
        }
    }
    if let Some(r) = l.readout {
        let holds: Vec<f32> = levels.iter().map(|x| x.1).collect();
        let text = readout(&holds);
        let w = pixel_font::width(&text, r.height() / pixel_font::H as f32);
        let px = r.height() / pixel_font::H as f32;
        pixel_font::draw(&mut m, Pos2::new(r.right() - w, r.top()), &text, px, LIT, dot);
    }
    p.add(Shape::mesh(m));
}

/// What happened to a meter this frame.
pub struct MeterResponse {
    pub response: Response,
    /// The (group, channel) under the pointer.
    pub hovered: Option<(usize, usize)>,
}

/// A live meter in `rect`: ballistics from `motion` (keyed under `id`), the
/// channel's name and level on hover. Double-click and click are on
/// `response` for the caller to act on.
pub fn meter_widget(
    ui: &mut Ui,
    id: Id,
    rect: Rect,
    groups: &[Group],
    geom: &Geom,
    look: MeterLook,
    motion: &mut Motion,
) -> MeterResponse {
    let layout = meter_layout(groups, rect, geom);
    let levels: Vec<(f32, f32, f32)> = layout
        .bars
        .iter()
        .map(|b| {
            let c = &groups[b.group].channels[b.chan];
            let (level, hold) = motion.ppm(id.with((b.group, b.chan)), c.peak_db);
            (level, hold, c.rms_db)
        })
        .collect();
    let response = ui.interact(rect, id, Sense::click());
    paint_meter(&ui.painter_at(rect.expand(2.0)), &layout, groups, &levels, look);
    let hovered = response.hover_pos().and_then(|p| {
        layout
            .bars
            .iter()
            .position(|b| p.x >= b.rect.left() - geom.gap / 2.0 && p.x <= b.rect.right() + geom.gap / 2.0)
            .map(|i| (layout.bars[i].group, layout.bars[i].chan))
    });
    let response = match hovered {
        Some((g, c)) => {
            let ch = &groups[g].channels[c];
            let i = layout.bars.iter().position(|b| b.group == g && b.chan == c).unwrap_or(0);
            let level = levels.get(i).map_or(SILENT_DB, |l| l.1);
            let db = if level > SILENT_DB + 0.5 { format!("{level:.1} dB") } else { "silent".into() };
            response.on_hover_text(format!(
                "{} {}  {}  ({db})",
                groups[g].label.split(' ').next().unwrap_or(""),
                ch.number,
                ch.name
            ))
        }
        None => response,
    };
    MeterResponse { response, hovered }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(points: f32, ppp: f32) -> bool {
        let px = points * ppp;
        (px - px.round()).abs() < 1e-4
    }

    #[test]
    fn dots_are_whole_square_pixels_at_any_scale() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let g = PixelGrid { ppp };
            let (d, gap) = g.dots(100.0);
            assert!(whole(d, ppp) && whole(gap, ppp), "ppp {ppp}: {d} {gap}");
            assert!(d * ppp >= 2.0 - 1e-4 && gap * ppp >= 1.0 - 1e-4, "ppp {ppp}: {d} {gap}");
            assert!(whole(g.snap(2.3), ppp) && whole(g.snap_pos(10.37), ppp));
            assert!(g.snap(0.01) * ppp >= 1.0 - 1e-4, "never below one pixel");
        }
    }

    #[test]
    fn tall_dot_ladders_grow_the_dots_and_stay_square() {
        for ppp in [1.0, 1.5] {
            let g = PixelGrid { ppp };
            let (d, gap) = g.dots(2000.0);
            assert!(2000.0 / (d + gap) <= MAX_ROWS as f32 + 1e-3, "ppp {ppp}: {d} {gap}");
            assert!(whole(d, ppp) && whole(gap, ppp));
            assert!(d > gap, "dots stay larger than their gaps");
        }
    }

    #[test]
    fn the_background_grid_uses_the_bars_dot_size() {
        // A tall bridge resolves bigger dots; the narrowing fallback ends on 1 px dots.
        let tall = Geom::bridge().resolve(MeterStyle::DotMatrix, 1.0, 600.0);
        assert_eq!(grid_dots(&tall, 1.0, 296.0), (3, 2), "from the geometry, not the line height");
        let g = {
            let chans = (0..64).map(|i| Chan { number: i + 1, ..Chan::silent() }).collect();
            vec![Group { label: "IN 64".into(), channels: chans }]
        };
        let tiny = fit_geom(&g, 150.0, Geom::card().resolve(MeterStyle::DotMatrix, 1.0, 50.0));
        assert_eq!(grid_dots(&tiny, 1.0, 50.0), (1, 1));
        let seg = Geom::card().resolve(MeterStyle::Segments, 1.0, 50.0);
        assert_eq!(grid_dots(&seg, 1.0, 50.0), PixelGrid { ppp: 1.0 }.dots_px(50.0), "no dots: the default size");
    }

    #[test]
    fn a_dot_bar_draws_every_column_it_was_laid_out_with() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let (d, g) = PixelGrid { ppp }.dots_px(60.0);
            for cols in 1..=4 {
                let w = dot_bar_width(cols, d as f32 / ppp, g as f32 / ppp);
                assert_eq!(dot_columns(w, ppp, d, g), cols, "ppp {ppp}, {cols} columns");
            }
        }
    }

    #[test]
    fn a_dot_bar_is_whole_dot_columns() {
        assert_eq!(dot_bar_width(3, 2.0, 1.0), 8.0);
        assert_eq!(dot_bar_width(4, 2.0, 1.0), 11.0);
        assert_eq!(dot_bar_width(0, 2.0, 1.0), 2.0, "at least one column");
    }

    #[test]
    fn tall_ladders_keep_a_bounded_number_of_rows() {
        for dot in [true, false] {
            let short = ladder_pitch(120.0, dot);
            assert_eq!(short, if dot { 2.0 } else { 3.0 }, "short bars keep the fine pitch");
            let rows = (1000.0 / ladder_pitch(1000.0, dot)).floor();
            assert!(rows <= MAX_ROWS as f32, "{rows}");
        }
    }

    fn groups(ins: usize, outs: usize) -> Vec<Group> {
        let chans = |n: usize, word: &str| -> Vec<Chan> {
            (0..n).map(|i| Chan { number: i as u32 + 1, name: format!("{word} {}", i + 1), ..Chan::silent() }).collect()
        };
        let mut g = Vec::new();
        if ins > 0 {
            g.push(Group { label: format!("IN {ins}"), channels: chans(ins, "In") });
        }
        if outs > 0 {
            g.push(Group { label: format!("OUT {outs}"), channels: chans(outs, "Out") });
        }
        g
    }

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(w, h))
    }

    #[test]
    fn bars_keep_their_width_and_do_not_stretch_to_fill() {
        let g = groups(4, 2);
        let narrow = meter_layout(&g, rect(240.0, 60.0), &Geom::card());
        let wide = meter_layout(&g, rect(900.0, 60.0), &Geom::card());
        for l in [&narrow, &wide] {
            assert_eq!(l.bars.len(), 6);
            assert!(l.bars.iter().all(|b| (b.rect.width() - Geom::card().bar_w).abs() < 1e-4));
        }
        assert_eq!(narrow.width_used, wide.width_used, "spare width stays empty");
        // Evenly pitched within a group; a wider gap between groups.
        let x = |i: usize| narrow.bars[i].rect.left();
        let pitch = x(1) - x(0);
        assert!((x(3) - x(2) - pitch).abs() < 1e-4);
        assert!(x(4) - x(3) > pitch + 1.0, "IN and OUT are apart");
    }

    #[test]
    fn card_and_bridge_bars_are_8_and_11_points() {
        let g = groups(2, 2);
        let seg = |geom: Geom| meter_layout(&g, rect(400.0, 60.0), &geom.resolve(MeterStyle::Segments, 1.0, 60.0));
        assert_eq!(seg(Geom::card()).bars[0].rect.width(), 8.0);
        assert_eq!(seg(Geom::bridge()).bars[0].rect.width(), 11.0);
        // Dot-matrix: whole columns of 2 px dots and 1 px gaps (3 and 4 of them).
        let dot = |geom: Geom| meter_layout(&g, rect(400.0, 60.0), &geom.resolve(MeterStyle::DotMatrix, 1.0, 60.0));
        assert_eq!(dot(Geom::card()).bars[0].rect.width(), 8.0);
        assert_eq!(dot(Geom::bridge()).bars[0].rect.width(), 11.0);
        // Bars are a whole number of dot cells apart, so every bar sits on the grid.
        let l = dot(Geom::card());
        let step = l.bars[1].rect.left() - l.bars[0].rect.left();
        assert!((step / 3.0 - (step / 3.0).round()).abs() < 1e-4, "{step}");
    }

    #[test]
    fn a_64_channel_meter_narrows_to_fit_and_never_overflows() {
        let g = groups(32, 32);
        for w in [196.0, 432.0] {
            for style in MeterStyle::all() {
                for ppp in [1.0, 1.5] {
                    let geom = fit_geom(&g, w, Geom::card().resolve(style, ppp, 60.0));
                    let l = meter_layout(&g, rect(w, 60.0), &geom);
                    assert!(!l.overflow, "{w} {style:?} {ppp}: {geom:?}");
                    assert!(l.bars[0].rect.width() * ppp >= 1.0 - 1e-4, "{w} {style:?} {ppp}");
                }
            }
        }
        // A meter that fits keeps its full bars.
        let small = groups(2, 2);
        let full = Geom::card().resolve(MeterStyle::Segments, 1.0, 60.0);
        assert_eq!(fit_geom(&small, 196.0, full), full);
    }

    #[test]
    fn channel_numbers_thin_out_but_keep_the_first_and_last() {
        let g = groups(23, 0);
        // Narrow bars (a meter squeezed by `fit_geom`): 6 pt apart.
        let l = meter_layout(&g, rect(400.0, 60.0), &Geom { bar_w: 4.0, gap: 2.0, ..Geom::card() });
        let numbers: Vec<&str> = l.numbers.iter().map(|n| n.1.as_str()).collect();
        assert!(numbers.len() < 23, "two-digit numbers do not fit every 6 px");
        assert_eq!(numbers.first(), Some(&"1"));
        assert_eq!(numbers.last(), Some(&"23"));
        // No two numbers overlap.
        let mut spans: Vec<(f32, f32)> =
            l.numbers.iter().map(|(p, t)| (p.x, p.x + crate::gear::pixel_font::width(t, 1.0))).collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!(spans.windows(2).all(|w| w[0].1 < w[1].0), "{spans:?}");
        // Plenty of room: every number shows.
        let roomy = meter_layout(&groups(8, 0), rect(400.0, 60.0), &Geom::bridge());
        assert_eq!(roomy.numbers.len(), 8);
    }

    #[test]
    fn each_bar_has_a_clip_block_above_it_inside_the_meter() {
        let r = rect(240.0, 64.0);
        let l = meter_layout(&groups(2, 2), r, &Geom::card());
        assert_eq!(l.clips.len(), 4);
        for (b, c) in l.bars.iter().zip(&l.clips) {
            assert!(c.bottom() < b.rect.top(), "above the bar");
            assert!((c.left() - b.rect.left()).abs() < 1e-4 && (c.width() - b.rect.width()).abs() < 1e-4);
            assert!(r.contains_rect(*c));
        }
    }

    #[test]
    fn the_peak_marker_is_one_segment_or_two_adjacent_ones() {
        assert_eq!(hold_segments(0.5, 20, false), Some((9, None)));
        assert_eq!(hold_segments(0.5, 20, true), Some((9, Some(8))));
        assert_eq!(hold_segments(1.0, 20, true), Some((19, Some(18))));
        assert_eq!(hold_segments(0.04, 20, true), Some((0, None)), "no segment below the bottom one");
        assert_eq!(hold_segments(0.0, 20, false), None, "silence shows no marker");
    }

    #[test]
    fn levels_map_minus_60_to_0_db_and_silence_is_empty() {
        assert_eq!(frac(0.0), 1.0);
        assert_eq!(frac(-60.0), 0.0);
        assert_eq!(frac(f32::NEG_INFINITY), 0.0);
        assert!((frac(-30.0) - 0.5).abs() < 1e-6);
        assert_eq!(frac(6.0), 1.0);
    }

    #[test]
    fn the_readout_shows_the_loudest_held_peak() {
        assert_eq!(readout(&[-12.04, -3.26, -90.0]), "-3.3");
        assert_eq!(readout(&[-90.0, f32::NEG_INFINITY]), "-INF");
        assert_eq!(readout(&[]), "-INF");
        assert_eq!(readout(&[0.4]), "+0.4");
    }
}
