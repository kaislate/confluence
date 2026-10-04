//! The matrix grid's geometry and input rules, free of drawing so they can be
//! tested: rows are input channels and columns output channels, both grouped
//! into slot bands in slot-id order.

use std::ops::Range;

use confluence_api::{PointState, SlotState};

use crate::commands::Edit;
use crate::theme::clamp_gain;

pub const CELL_DEFAULT: f32 = 18.0;
pub const CELL_MIN: f32 = 12.0;
pub const CELL_MAX: f32 = 40.0;
/// Pixels of vertical drag per gain step.
pub const DRAG_PX_PER_STEP: f32 = 4.0;

/// One slot's channels along an axis.
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub slot: u32,
    pub name: String,
    pub online: bool,
    /// The slot's first global channel in this direction.
    pub first_channel: u32,
    pub channels: u32,
    /// The band's first row (or column) index.
    pub start: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Axis {
    pub bands: Vec<Band>,
    pub len: usize,
}

impl Axis {
    pub fn inputs(slots: &[SlotState]) -> Axis {
        Self::build(slots, |s| (s.first_input, s.inputs))
    }

    pub fn outputs(slots: &[SlotState]) -> Axis {
        Self::build(slots, |s| (s.first_output, s.outputs))
    }

    fn build(slots: &[SlotState], range: impl Fn(&SlotState) -> (u32, u32)) -> Axis {
        let mut sorted: Vec<&SlotState> = slots.iter().collect();
        sorted.sort_by_key(|s| s.id);
        let mut axis = Axis::default();
        for s in sorted {
            let (first_channel, channels) = range(s);
            if channels == 0 {
                continue;
            }
            axis.bands.push(Band {
                slot: s.id,
                name: s.name.clone(),
                online: s.online,
                first_channel,
                channels,
                start: axis.len,
            });
            axis.len += channels as usize;
        }
        axis
    }

    /// The band at `index` and the 0-based channel within it.
    pub fn at(&self, index: usize) -> Option<(&Band, u32)> {
        self.bands
            .iter()
            .find(|b| index >= b.start && index < b.start + b.channels as usize)
            .map(|b| (b, (index - b.start) as u32))
    }

    /// The global channel at `index`.
    pub fn global(&self, index: usize) -> Option<u32> {
        self.at(index).map(|(b, k)| b.first_channel + k)
    }

    /// The index of global channel `channel`.
    pub fn index_of(&self, channel: u32) -> Option<usize> {
        self.bands
            .iter()
            .find(|b| channel >= b.first_channel && channel < b.first_channel + b.channels)
            .map(|b| b.start + (channel - b.first_channel) as usize)
    }

    pub fn band(&self, slot: u32) -> Option<&Band> {
        self.bands.iter().find(|b| b.slot == slot)
    }
}

pub struct GridLayout {
    pub rows: Axis,
    pub cols: Axis,
    pub cell: f32,
}

impl GridLayout {
    pub fn new(slots: &[SlotState], cell: f32) -> Self {
        GridLayout { rows: Axis::inputs(slots), cols: Axis::outputs(slots), cell: cell.clamp(CELL_MIN, CELL_MAX) }
    }

    /// Width and height of the cell area.
    pub fn size(&self) -> (f32, f32) {
        (self.cols.len as f32 * self.cell, self.rows.len as f32 * self.cell)
    }

    /// The (row, column) under a point relative to the cell area's top left.
    pub fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (row, col) = ((y / self.cell) as usize, (x / self.cell) as usize);
        (row < self.rows.len && col < self.cols.len).then_some((row, col))
    }

    /// Row and column ranges intersecting the rectangle (cell-area coordinates).
    pub fn visible(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (Range<usize>, Range<usize>) {
        let span = |a: f32, b: f32, len: usize| {
            let start = ((a / self.cell).floor().max(0.0) as usize).min(len);
            let end = ((b / self.cell).ceil().max(0.0) as usize).min(len);
            start..end.max(start)
        };
        (span(y0, y1, self.rows.len), span(x0, x1, self.cols.len))
    }

    /// The (input, output) global channels of a cell.
    pub fn point(&self, row: usize, col: usize) -> Option<(u32, u32)> {
        Some((self.rows.global(row)?, self.cols.global(col)?))
    }

    /// The cell of a point, if both channels are on the grid.
    pub fn cell_of(&self, input: u32, output: u32) -> Option<(usize, usize)> {
        Some((self.rows.index_of(input)?, self.cols.index_of(output)?))
    }

    /// `Mic in 1 → Speakers out 2`.
    pub fn label(&self, row: usize, col: usize) -> Option<String> {
        let (ib, ik) = self.rows.at(row)?;
        let (ob, ok) = self.cols.at(col)?;
        Some(format!("{} in {} → {} out {}", ib.name, ik + 1, ob.name, ok + 1))
    }
}

/// The label of a point from the slots (`?` for channels no slot owns).
pub fn point_label(slots: &[SlotState], input: u32, output: u32) -> String {
    let l = GridLayout::new(slots, CELL_DEFAULT);
    l.cell_of(input, output)
        .and_then(|(r, c)| l.label(r, c))
        .unwrap_or_else(|| format!("in {} → out {}", input + 1, output + 1))
}

/// What the inspector shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Selection {
    #[default]
    None,
    Cell {
        input: u32,
        output: u32,
    },
    Slot(u32),
}

/// False when the selected slot or channels no longer exist.
pub fn selection_valid(selection: &Selection, slots: &[SlotState]) -> bool {
    match *selection {
        Selection::None => true,
        Selection::Slot(id) => slots.iter().any(|s| s.id == id),
        Selection::Cell { input, output } => GridLayout::new(slots, CELL_DEFAULT).cell_of(input, output).is_some(),
    }
}

/// Pointer input on one cell during one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CellInput {
    pub clicked: bool,
    pub double_clicked: bool,
    /// Vertical drag this frame (screen pixels, down is positive).
    pub drag_dy: f32,
    pub wheel_notches: f32,
    /// Shift held: 0.1 dB steps.
    pub fine: bool,
}

/// The gain step in dB.
pub fn step(fine: bool) -> f32 {
    if fine {
        0.1
    } else {
        1.0
    }
}

fn set(p: (u32, u32), gain_db: f32, mute: bool, invert: bool) -> Edit {
    Edit::SetPoint { input: p.0, output: p.1, gain_db, mute, invert }
}

fn regain(p: (u32, u32), cur: &PointState, delta: f32) -> Option<Edit> {
    let gain = clamp_gain(cur.gain_db + delta);
    (gain != cur.gain_db).then(|| set(p, gain, cur.mute, cur.invert))
}

/// The edit for this frame's pointer input on a cell (`current`: its route, if any).
pub fn cell_edit(p: (u32, u32), current: Option<&PointState>, input: &CellInput) -> Option<Edit> {
    if input.double_clicked {
        return Some(set(p, 0.0, false, false));
    }
    if input.clicked {
        return Some(match current {
            Some(_) => Edit::RemovePoint { input: p.0, output: p.1 },
            None => set(p, 0.0, false, false),
        });
    }
    let cur = current?;
    let s = step(input.fine);
    let delta = -input.drag_dy / DRAG_PX_PER_STEP * s + input.wheel_notches * s;
    if delta == 0.0 {
        return None;
    }
    regain(p, cur, delta)
}

/// A single click on a routed cell unroutes it only once the double-click
/// window has passed, so a double-click (reset to 0 dB) never drops the route,
/// not even for a moment.
#[derive(Default)]
pub struct DeferredUnroute {
    pending: Option<((u32, u32), f64)>,
}

impl DeferredUnroute {
    /// egui's double-click window, in seconds.
    pub const WINDOW: f64 = 0.3;

    /// A single click on routed point `p` at `now` (seconds). Returns a point
    /// clicked earlier that must be unrouted now (another cell was clicked).
    pub fn click(&mut self, p: (u32, u32), now: f64) -> Option<(u32, u32)> {
        let earlier = self.pending.take().map(|(q, _)| q).filter(|q| *q != p);
        self.pending = Some((p, now));
        earlier
    }

    /// The second click of a double-click on `p`: keep the route.
    pub fn double(&mut self, p: (u32, u32)) {
        if self.pending.is_some_and(|(q, _)| q == p) {
            self.pending = None;
        }
    }

    /// The point to unroute now, once its window has passed.
    pub fn due(&mut self, now: f64) -> Option<(u32, u32)> {
        match self.pending {
            Some((p, at)) if now - at >= Self::WINDOW => {
                self.pending = None;
                Some(p)
            }
            _ => None,
        }
    }

    /// Seconds until the pending unroute is due.
    pub fn waiting(&self, now: f64) -> Option<f64> {
        self.pending.map(|(_, at)| (Self::WINDOW - (now - at)).max(0.0))
    }

    pub fn is_pending(&self, p: (u32, u32)) -> bool {
        self.pending.is_some_and(|(q, _)| q == p)
    }
}

/// Keys that act on the selected cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellKey {
    Toggle,
    Up,
    Down,
    Mute,
    Invert,
    Remove,
}

pub fn key_edit(p: (u32, u32), current: Option<&PointState>, key: CellKey, fine: bool) -> Option<Edit> {
    let s = step(fine);
    match (key, current) {
        (CellKey::Toggle, None) => Some(set(p, 0.0, false, false)),
        (CellKey::Toggle | CellKey::Remove, Some(_)) => Some(Edit::RemovePoint { input: p.0, output: p.1 }),
        (CellKey::Up, Some(c)) => regain(p, c, s),
        (CellKey::Down, Some(c)) => regain(p, c, -s),
        (CellKey::Mute, Some(c)) => Some(set(p, c.gain_db, !c.mute, c.invert)),
        (CellKey::Invert, Some(c)) => Some(set(p, c.gain_db, c.mute, !c.invert)),
        _ => None,
    }
}

/// Moves a (row, column) selection, clamped to the grid; `None` for an empty grid.
pub fn move_selection(l: &GridLayout, at: (usize, usize), dr: i32, dc: i32) -> Option<(usize, usize)> {
    if l.rows.len == 0 || l.cols.len == 0 {
        return None;
    }
    let clamp = |v: usize, d: i32, len: usize| (v as i64 + i64::from(d)).clamp(0, len as i64 - 1) as usize;
    Some((clamp(at.0, dr, l.rows.len), clamp(at.1, dc, l.cols.len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::ClockRole;

    fn slot(id: u32, name: &str, first_input: u32, inputs: u32, first_output: u32, outputs: u32) -> SlotState {
        SlotState {
            id,
            name: name.into(),
            device: String::new(),
            role: ClockRole::Soft,
            online: true,
            first_input,
            inputs,
            first_output,
            outputs,
        }
    }

    fn slots() -> Vec<SlotState> {
        vec![
            slot(3, "Speakers", 0, 0, 0, 2), // output only
            slot(1, "Mic", 0, 1, 0, 0),      // input only
            slot(2, "VASIO 1", 1, 2, 2, 2),
        ]
    }

    fn pt(input: u32, output: u32, gain_db: f32) -> PointState {
        PointState { input, output, gain_db, mute: false, invert: false }
    }

    #[test]
    fn bands_follow_slot_ids_and_skip_empty_directions() {
        let l = GridLayout::new(&slots(), CELL_DEFAULT);
        let rows: Vec<_> = l.rows.bands.iter().map(|b| (b.slot, b.start, b.channels)).collect();
        let cols: Vec<_> = l.cols.bands.iter().map(|b| (b.slot, b.start, b.channels)).collect();
        assert_eq!(rows, vec![(1, 0, 1), (2, 1, 2)]);
        assert_eq!(cols, vec![(2, 0, 2), (3, 2, 2)]);
        assert_eq!((l.rows.len, l.cols.len), (3, 4));
    }

    #[test]
    fn grid_cells_map_to_global_channels_and_back() {
        let l = GridLayout::new(&slots(), CELL_DEFAULT);
        assert_eq!(l.point(1, 2), Some((1, 0)), "VASIO 1 in 1 → Speakers out 1");
        assert_eq!(l.cell_of(1, 0), Some((1, 2)));
        assert_eq!(l.label(1, 2).unwrap(), "VASIO 1 in 1 → Speakers out 1");
        assert_eq!(l.point(3, 0), None, "past the last row");
        assert_eq!(l.cell_of(99, 0), None);
        assert_eq!(point_label(&slots(), 2, 1), "VASIO 1 in 2 → Speakers out 2");
    }

    #[test]
    fn hit_testing_finds_the_cell_and_respects_edges() {
        let l = GridLayout::new(&slots(), 20.0);
        assert_eq!(l.cell_at(0.0, 0.0), Some((0, 0)));
        assert_eq!(l.cell_at(39.9, 59.9), Some((2, 1)));
        assert_eq!(l.cell_at(80.0, 0.0), None, "right of the last column");
        assert_eq!(l.cell_at(-1.0, 5.0), None);
        assert_eq!(l.size(), (80.0, 60.0));
    }

    #[test]
    fn an_empty_engine_has_an_empty_grid() {
        let l = GridLayout::new(&[], CELL_DEFAULT);
        assert_eq!((l.rows.len, l.cols.len), (0, 0));
        assert_eq!(l.cell_at(1.0, 1.0), None);
        assert_eq!(l.visible(0.0, 0.0, 500.0, 500.0), (0..0, 0..0));
    }

    #[test]
    fn the_cell_size_is_clamped() {
        assert_eq!(GridLayout::new(&[], 2.0).cell, CELL_MIN);
        assert_eq!(GridLayout::new(&[], 99.0).cell, CELL_MAX);
    }

    #[test]
    fn only_visible_cells_are_returned_for_a_large_matrix() {
        let big = vec![slot(1, "Big", 0, 1024, 0, 1024)];
        let l = GridLayout::new(&big, 20.0);
        let (rows, cols) = l.visible(400.0, 2000.0, 1200.0, 2600.0);
        assert_eq!(rows, 100..130);
        assert_eq!(cols, 20..60);
    }

    #[test]
    fn a_click_routes_an_empty_cell_and_unroutes_a_routed_one() {
        let click = CellInput { clicked: true, ..Default::default() };
        assert_eq!(
            cell_edit((1, 2), None, &click),
            Some(Edit::SetPoint { input: 1, output: 2, gain_db: 0.0, mute: false, invert: false })
        );
        assert_eq!(cell_edit((1, 2), Some(&pt(1, 2, -6.0)), &click), Some(Edit::RemovePoint { input: 1, output: 2 }));
    }

    #[test]
    fn a_double_click_makes_a_plain_zero_db_route() {
        let dbl = CellInput { clicked: true, double_clicked: true, ..Default::default() };
        let muted = PointState { mute: true, ..pt(1, 2, -12.0) };
        assert_eq!(
            cell_edit((1, 2), Some(&muted), &dbl),
            Some(Edit::SetPoint { input: 1, output: 2, gain_db: 0.0, mute: false, invert: false })
        );
    }

    #[test]
    fn dragging_up_raises_gain_and_shift_makes_it_fine() {
        let up8 = CellInput { drag_dy: -8.0, ..Default::default() };
        let fine = CellInput { drag_dy: -8.0, fine: true, ..Default::default() };
        let gain_of = |e: Option<Edit>| match e {
            Some(Edit::SetPoint { gain_db, .. }) => gain_db,
            other => panic!("{other:?}"),
        };
        assert!((gain_of(cell_edit((0, 0), Some(&pt(0, 0, -6.0)), &up8)) - -4.0).abs() < 1e-5);
        assert!((gain_of(cell_edit((0, 0), Some(&pt(0, 0, -6.0)), &fine)) - -5.8).abs() < 1e-5);
        assert_eq!(cell_edit((0, 0), None, &up8), None, "dragging an empty cell does nothing");
    }

    #[test]
    fn gain_stops_at_the_engine_limits() {
        let way_up = CellInput { drag_dy: -1000.0, ..Default::default() };
        match cell_edit((0, 0), Some(&pt(0, 0, 20.0)), &way_up) {
            Some(Edit::SetPoint { gain_db, .. }) => assert_eq!(gain_db, 24.0),
            other => panic!("{other:?}"),
        }
        assert_eq!(cell_edit((0, 0), Some(&pt(0, 0, 24.0)), &way_up), None, "already at the limit");
    }

    #[test]
    fn the_wheel_changes_gain_only_on_routed_cells() {
        let notch = CellInput { wheel_notches: 2.0, ..Default::default() };
        match cell_edit((0, 0), Some(&pt(0, 0, -6.0)), &notch) {
            Some(Edit::SetPoint { gain_db, .. }) => assert!((gain_db - -4.0).abs() < 1e-5),
            other => panic!("{other:?}"),
        }
        assert_eq!(cell_edit((0, 0), None, &notch), None);
    }

    #[test]
    fn keys_act_on_the_selected_cell() {
        let r = pt(0, 0, -6.0);
        assert_eq!(key_edit((0, 0), Some(&r), CellKey::Toggle, false), Some(Edit::RemovePoint { input: 0, output: 0 }));
        assert!(
            matches!(key_edit((0, 0), None, CellKey::Toggle, false), Some(Edit::SetPoint { gain_db, .. }) if gain_db == 0.0)
        );
        assert!(
            matches!(key_edit((0, 0), Some(&r), CellKey::Up, false), Some(Edit::SetPoint { gain_db, .. }) if (gain_db - -5.0).abs() < 1e-5)
        );
        assert!(
            matches!(key_edit((0, 0), Some(&r), CellKey::Down, true), Some(Edit::SetPoint { gain_db, .. }) if (gain_db - -6.1).abs() < 1e-5)
        );
        assert!(matches!(key_edit((0, 0), Some(&r), CellKey::Mute, false), Some(Edit::SetPoint { mute: true, .. })));
        assert!(matches!(
            key_edit((0, 0), Some(&r), CellKey::Invert, false),
            Some(Edit::SetPoint { invert: true, .. })
        ));
        assert_eq!(key_edit((0, 0), Some(&r), CellKey::Remove, false), Some(Edit::RemovePoint { input: 0, output: 0 }));
        assert_eq!(key_edit((0, 0), None, CellKey::Mute, false), None, "nothing to mute");
    }

    #[test]
    fn the_selection_moves_within_the_grid() {
        let l = GridLayout::new(&slots(), CELL_DEFAULT);
        assert_eq!(move_selection(&l, (0, 0), 1, 1), Some((1, 1)));
        assert_eq!(move_selection(&l, (0, 0), -1, -1), Some((0, 0)), "clamped at the top left");
        assert_eq!(move_selection(&l, (2, 3), 5, 5), Some((2, 3)), "clamped at the bottom right");
        assert_eq!(move_selection(&GridLayout::new(&[], CELL_DEFAULT), (0, 0), 1, 0), None);
    }

    #[test]
    fn a_single_click_unroutes_once_the_double_click_window_has_passed() {
        let mut u = DeferredUnroute::default();
        assert_eq!(u.click((1, 2), 10.0), None);
        assert!(u.is_pending((1, 2)));
        assert_eq!(u.due(10.1), None, "a second click may still come");
        assert_eq!(u.due(10.0 + DeferredUnroute::WINDOW), Some((1, 2)));
        assert_eq!(u.due(11.0), None, "only once");
    }

    #[test]
    fn a_double_click_never_drops_the_route() {
        let mut u = DeferredUnroute::default();
        u.click((1, 2), 10.0);
        u.double((1, 2));
        assert_eq!(u.due(20.0), None);
    }

    #[test]
    fn clicking_another_routed_cell_unroutes_the_first_at_once() {
        let mut u = DeferredUnroute::default();
        u.click((1, 2), 10.0);
        assert_eq!(u.click((3, 4), 10.1), Some((1, 2)));
        assert_eq!(u.waiting(10.1).map(|w| (w * 100.0).round() / 100.0), Some(DeferredUnroute::WINDOW));
    }

    #[test]
    fn a_selection_is_dropped_when_its_slot_or_channels_go() {
        let s = slots();
        assert!(selection_valid(&Selection::Slot(2), &s));
        assert!(!selection_valid(&Selection::Slot(9), &s), "slot removed elsewhere");
        assert!(selection_valid(&Selection::Cell { input: 2, output: 3 }, &s));
        assert!(!selection_valid(&Selection::Cell { input: 5, output: 0 }, &s), "input channel gone");
        assert!(selection_valid(&Selection::None, &s));
    }
}
