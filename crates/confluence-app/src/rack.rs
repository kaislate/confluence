//! The Devices screen as a rack (spec: round 8): sections of fixed trays in a
//! column grid, tray widths from their channel counts, rows per position
//! group, the order a dragged tray would land in, and section sizes. Pure,
//! so it can be tested without a window.

use eframe::egui::{Pos2, Rect};
use serde::{Deserialize, Serialize};

/// A one-column tray is at least this wide (a group of eight fits one row
/// of a 1600-wide window).
pub const TRAY_MIN_W: f32 = 176.0;
/// Between trays, both ways.
pub const GAP: f32 = 8.0;
/// A tray's text part (LED, tag, strip, sub-line) at its narrowest.
pub const INFO_W: f32 = 120.0;
/// The most columns a tray spans.
pub const MAX_SPAN: usize = 4;

/// The columns a bed `bed_w` wide holds (at least one).
pub fn columns(bed_w: f32) -> usize {
    (((bed_w + GAP) / (TRAY_MIN_W + GAP)).floor() as usize).max(1)
}

/// One column's width when `cols` share `bed_w`.
pub fn col_width(bed_w: f32, cols: usize) -> f32 {
    let cols = cols.max(1) as f32;
    ((bed_w - GAP * (cols - 1.0)) / cols).max(0.0)
}

/// A tray `span` columns wide.
pub fn tray_w(span: usize, col_w: f32) -> f32 {
    let span = span.max(1) as f32;
    span * col_w + (span - 1.0) * GAP
}

/// The OLED window's width in a tray `w` wide: what the text leaves, and at
/// least 40% of the tray.
pub fn oled_w(w: f32) -> f32 {
    (w - INFO_W).max(w * 0.4)
}

/// The meter's width inside a tray `span` columns wide: its OLED window less
/// the module's and the window's insets (as the tray is drawn).
pub fn meter_w(span: usize, col_w: f32) -> f32 {
    oled_w(tray_w(span, col_w) - 4.0) - 16.0
}

/// The fewest columns (up to [`MAX_SPAN`] and `cols`) whose meter `fits`.
pub fn tray_span(col_w: f32, cols: usize, fits: impl Fn(f32) -> bool) -> usize {
    let most = MAX_SPAN.min(cols.max(1));
    (1..=most).find(|&s| fits(meter_w(s, col_w))).unwrap_or(most)
}

/// Places trays of `spans` row by row in `cols` columns; a tray that does not
/// fit the rest of a row, or has `breaks[i]` (a new position group), starts
/// the next. Each tray's (row, column), and the rows used.
pub fn place(spans: &[usize], breaks: &[bool], cols: usize) -> (Vec<(usize, usize)>, usize) {
    let cols = cols.max(1);
    let (mut row, mut col) = (0usize, 0usize);
    let mut at = Vec::with_capacity(spans.len());
    for (i, &s) in spans.iter().enumerate() {
        let s = s.clamp(1, cols);
        if col > 0 && (col + s > cols || breaks.get(i).copied().unwrap_or(false)) {
            row += 1;
            col = 0;
        }
        at.push((row, col));
        col += s;
    }
    let rows = if at.is_empty() { 0 } else { row + 1 };
    (at, rows)
}

/// The order with `dragged` moved to wherever its slot lands nearest
/// `pointer`; `rects_for` lays a candidate order out (one rect per key, in
/// that order).
pub fn candidate(
    order: &[String],
    dragged: &str,
    pointer: Pos2,
    rects_for: impl Fn(&[String]) -> Vec<Rect>,
) -> Vec<String> {
    let Some(from) = order.iter().position(|k| k == dragged) else { return order.to_vec() };
    let mut others = order.to_vec();
    let key = others.remove(from);
    let mut best = (f32::INFINITY, order.to_vec());
    for to in 0..=others.len() {
        let mut o = others.clone();
        o.insert(to, key.clone());
        let Some(r) = rects_for(&o).get(to).copied() else { continue };
        let d = r.center().distance_sq(pointer);
        // Ties keep the earlier place, so a tray over its own slot stays put.
        if d < best.0 || (d == best.0 && o == order) {
            best = (d, o);
        }
    }
    best.1
}

/// How big a section's trays are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SectionSize {
    #[default]
    Full,
    Compact,
    Collapsed,
}

impl SectionSize {
    /// Full, then compact, then collapsed, then full again.
    pub fn next(self) -> SectionSize {
        match self {
            SectionSize::Full => SectionSize::Compact,
            SectionSize::Compact => SectionSize::Collapsed,
            SectionSize::Collapsed => SectionSize::Full,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            SectionSize::Full => "full",
            SectionSize::Compact => "compact",
            SectionSize::Collapsed => "collapsed",
        }
    }

    /// A tray's height (`None`: the section shows no trays).
    pub fn tray_h(self) -> Option<f32> {
        match self {
            SectionSize::Full => Some(66.0),
            SectionSize::Compact => Some(34.0),
            SectionSize::Collapsed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, vec2};

    #[test]
    fn a_bed_holds_as_many_columns_as_fit_and_at_least_one() {
        assert_eq!(columns(1800.0), 9, "9 × 176 + 8 × 8 = 1648 fits, 10 do not");
        assert!(columns(1520.0) >= 8, "a group of eight fits one row of a 1600-wide window");
        assert_eq!(columns(176.0), 1);
        assert_eq!(columns(100.0), 1, "too narrow: still one");
        assert_eq!(columns(360.0), 2);
        let w = col_width(1000.0, 4);
        assert!((4.0 * w + 3.0 * GAP - 1000.0).abs() < 1e-3, "columns fill the bed");
        assert_eq!(tray_w(2, 100.0), 208.0);
        assert_eq!(tray_w(1, 100.0), 100.0);
    }

    #[test]
    fn the_meter_window_takes_what_the_text_leaves_and_at_least_two_fifths() {
        assert_eq!(oled_w(220.0), 100.0);
        assert_eq!(oled_w(150.0), 60.0, "40% of a narrow tray");
        assert_eq!(oled_w(500.0), 380.0);
    }

    #[test]
    fn a_tray_spans_the_fewest_columns_that_hold_its_meters() {
        let cw = 228.0;
        let needs = |w: f32| move |have: f32| have >= w;
        assert_eq!(meter_w(1, cw), 88.0);
        assert_eq!(tray_span(cw, 7, needs(80.0)), 1, "a stereo device");
        assert_eq!(tray_span(cw, 7, needs(200.0)), 2);
        assert_eq!(tray_span(cw, 7, needs(500.0)), 3);
        assert_eq!(tray_span(cw, 7, needs(5000.0)), MAX_SPAN, "capped");
        assert_eq!(tray_span(cw, 2, needs(500.0)), 2, "never wider than the bed");
    }

    #[test]
    fn trays_fill_rows_and_each_group_starts_its_own() {
        // WIN IN 1, 2, then WIN OUT 1 (a new group), 2 (two columns).
        let (at, rows) = place(&[1, 1, 1, 2], &[false, false, true, false], 3);
        assert_eq!(at, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(rows, 2);
        let (at, rows) = place(&[2, 2, 1], &[false, false, false], 3);
        assert_eq!(at, vec![(0, 0), (1, 0), (1, 2)], "a tray that does not fit starts the next row");
        assert_eq!(rows, 2);
        assert_eq!(place(&[4], &[false], 2), (vec![(0, 0)], 1), "wider than the bed: a row of its own");
        assert_eq!(place(&[], &[], 3), (vec![], 0));
    }

    #[test]
    fn a_dragged_tray_goes_where_its_slot_lands_nearest_the_pointer() {
        let order: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        // One row of 100-wide slots, 10 apart.
        let lay = |o: &[String]| -> Vec<Rect> {
            (0..o.len()).map(|i| Rect::from_min_size(pos2(i as f32 * 110.0, 0.0), vec2(100.0, 50.0))).collect()
        };
        let keys = |v: Vec<String>| v.join("");
        assert_eq!(keys(candidate(&order, "d", pos2(40.0, 20.0), lay)), "dabc", "over the first slot");
        assert_eq!(keys(candidate(&order, "a", pos2(370.0, 20.0), lay)), "bcda", "over the last");
        assert_eq!(keys(candidate(&order, "b", pos2(120.0, 20.0), lay)), "abcd", "over its own slot: unchanged");
        assert_eq!(keys(candidate(&order, "x", pos2(0.0, 0.0), lay)), "abcd", "an unknown tray changes nothing");
    }

    #[test]
    fn sections_cycle_full_compact_collapsed() {
        assert_eq!(SectionSize::default(), SectionSize::Full);
        assert_eq!(SectionSize::Full.next(), SectionSize::Compact);
        assert_eq!(SectionSize::Compact.next(), SectionSize::Collapsed);
        assert_eq!(SectionSize::Collapsed.next(), SectionSize::Full);
        assert_eq!(SectionSize::Compact.word(), "compact");
        assert_eq!(SectionSize::Full.tray_h(), Some(66.0));
        assert_eq!(SectionSize::Compact.tray_h(), Some(34.0));
        assert_eq!(SectionSize::Collapsed.tray_h(), None);
    }
}
