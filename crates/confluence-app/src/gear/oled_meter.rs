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
}

impl Geom {
    pub fn card() -> Geom {
        Geom { bar_w: 4.0, gap: 2.0, group_gap: 7.0, scale: false, readout: true, readout_px: 1.0 }
    }

    pub fn bridge() -> Geom {
        Geom { bar_w: 5.0, gap: 2.0, group_gap: 9.0, scale: true, readout: false, readout_px: 2.0 }
    }
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
    let mut m = Mesh::default();
    let clip_on = if look.clip_red { RED } else { LIT };
    for (i, b) in l.bars.iter().enumerate() {
        let (level, hold, rms) = levels.get(i).copied().unwrap_or((SILENT_DB, SILENT_DB, SILENT_DB));
        let (lv, hv) = (frac(level), frac(hold));
        let r = b.rect;
        match look.style {
            MeterStyle::Segments | MeterStyle::DotMatrix => {
                let pitch = ladder_pitch(r.height(), dot);
                let seg_h = pitch * if dot { 0.8 } else { 2.0 / 3.0 };
                let n = (r.height() / pitch).floor().max(1.0) as usize;
                let held = hold_segments(hv, n, look.double_peak);
                for k in 0..n {
                    let y = r.bottom() - (k + 1) as f32 * pitch + (pitch - seg_h);
                    let on =
                        (k + 1) as f32 / n as f32 <= lv + 1e-4 || held.is_some_and(|(a, b)| k == a || Some(k) == b);
                    let colour = if on { LIT } else { dim(0.10) };
                    if dot {
                        let mut x = r.left();
                        while x + 1.0 <= r.right() + 0.01 {
                            m.add_colored_rect(Rect::from_min_size(Pos2::new(x, y), Vec2::new(1.6, seg_h)), colour);
                            x += 2.0;
                        }
                    } else {
                        m.add_colored_rect(
                            Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(r.width(), seg_h)),
                            colour,
                        );
                    }
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
            m.add_colored_rect(*c, if clipped { clip_on } else { dim(0.06) });
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
    fn a_33_channel_interface_fits_a_card_at_the_narrow_bar() {
        let g = groups(23, 10);
        assert!(!meter_layout(&g, rect(216.0, 60.0), &Geom::card()).overflow);
        let fat = Geom { bar_w: 8.0, ..Geom::card() };
        assert!(meter_layout(&g, rect(216.0, 60.0), &fat).overflow);
    }

    #[test]
    fn channel_numbers_thin_out_but_keep_the_first_and_last() {
        let g = groups(23, 0);
        let l = meter_layout(&g, rect(400.0, 60.0), &Geom::card());
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
