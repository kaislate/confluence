//! The matrix grid's geometry and input rules, free of drawing so they can be
//! tested: rows are input channels and columns output channels, both grouped
//! into slot bands, in slot-id order unless the user arranged them. A folded
//! band takes one line instead of one per channel.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use confluence_api::{PointState, PositionState, SlotState};

use crate::commands::Edit;
use crate::theme::clamp_gain;

pub const CELL_DEFAULT: f32 = 22.0;
pub const CELL_MIN: f32 = 12.0;
pub const CELL_MAX: f32 = 40.0;
/// Pixels of vertical drag per gain step.
pub const DRAG_PX_PER_STEP: f32 = 4.0;
/// The separator strip before each device's channels (its name sits in it).
pub const SEP: f32 = 20.0;

/// One slot's channels along an axis.
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub slot: u32,
    /// What the saved layout calls it (`pos:asio:1`, see [`band_keys`]).
    pub key: String,
    /// Shown as one line.
    pub folded: bool,
    /// The device's custom name if it has one, else the slot's name.
    pub name: String,
    /// The slot's own name, whatever it is called.
    pub own_name: String,
    /// Custom names of its channels in this direction (one per channel).
    pub channel_labels: Vec<Option<String>>,
    /// The device's own names for those channels ("Front L", "DAW out 1").
    pub channel_names: Vec<String>,
    pub online: bool,
    /// An insert bus: its rows are returns and its columns sends.
    pub bus: bool,
    /// The slot's first global channel in this direction.
    pub first_channel: u32,
    pub channels: u32,
    /// The band's first row (or column) index.
    pub start: usize,
    /// Picks the default colour: the lowest id among the slots of this
    /// slot's device, so a device's input and output bands match.
    pub palette: u32,
    /// The colour chosen for its device, if any.
    pub color: Option<confluence_api::Rgb>,
}

impl Band {
    /// The lines it takes on its axis: one per channel, or one when folded.
    pub fn lines(&self) -> u32 {
        if self.folded {
            1
        } else {
            self.channels
        }
    }
}

/// What the saved layout calls each slot's band: `pos:<position>` for a slot
/// in a position, else its colour key (`bus:3` for an insert bus).
pub fn band_keys(slots: &[SlotState], positions: &[PositionState]) -> HashMap<u32, String> {
    let mut keys: HashMap<u32, String> = slots.iter().map(|s| (s.id, s.color_key())).collect();
    for p in positions {
        for id in &p.slots {
            if let Some(k) = keys.get_mut(id) {
                *k = format!("pos:{}", p.pos);
            }
        }
    }
    keys
}

/// How one axis is arranged: its saved order and folded bands.
struct Arrangement<'a> {
    keys: &'a HashMap<u32, String>,
    order: &'a [String],
    folded: &'a BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Axis {
    pub bands: Vec<Band>,
    pub len: usize,
    /// The cell size the offsets below are in.
    pub cell: f32,
}

impl Axis {
    fn inputs(slots: &[SlotState], a: &Arrangement) -> Axis {
        Self::build(slots, a, |s| (s.first_input, s.inputs, s.input_labels.clone(), s.input_names.clone()))
    }

    fn outputs(slots: &[SlotState], a: &Arrangement) -> Axis {
        Self::build(slots, a, |s| (s.first_output, s.outputs, s.output_labels.clone(), s.output_names.clone()))
    }

    #[allow(clippy::type_complexity)]
    fn build(
        slots: &[SlotState],
        a: &Arrangement,
        range: impl Fn(&SlotState) -> (u32, u32, Vec<Option<String>>, Vec<String>),
    ) -> Axis {
        let mut sorted: Vec<&SlotState> = slots.iter().collect();
        sorted.sort_by_key(|s| s.id);
        let mut axis = Axis { cell: CELL_DEFAULT, ..Axis::default() };
        for s in sorted {
            let (first_channel, channels, channel_labels, channel_names) = range(s);
            if channels == 0 {
                continue;
            }
            let key = s.color_key();
            let palette = slots.iter().filter(|o| o.color_key() == key).map(|o| o.id).min().unwrap_or(s.id);
            // Keys are unique on an axis: a repeat takes `#2`, `#3`...
            let base = a.keys.get(&s.id).cloned().unwrap_or_else(|| s.color_key());
            let mut key = base.clone();
            let mut n = 1;
            while axis.bands.iter().any(|b| b.key == key) {
                n += 1;
                key = format!("{base}#{n}");
            }
            axis.bands.push(Band {
                folded: a.folded.contains(&key),
                key,
                palette,
                color: s.color,
                slot: s.id,
                name: s.label.clone().unwrap_or_else(|| s.name.clone()),
                own_name: s.name.clone(),
                channel_labels,
                channel_names,
                online: s.online,
                bus: s.is_bus(),
                first_channel,
                channels,
                start: 0,
            });
        }
        let default: Vec<String> = axis.bands.iter().map(|b| b.key.clone()).collect();
        let order = crate::arrange::ordered(&default, a.order);
        axis.bands.sort_by_key(|b| order.iter().position(|k| *k == b.key));
        for b in &mut axis.bands {
            b.start = axis.len;
            axis.len += b.lines() as usize;
        }
        axis
    }

    /// The band at `index` and the 0-based channel within it.
    pub fn at(&self, index: usize) -> Option<(&Band, u32)> {
        self.bands.get(self.band_of(index)).map(|b| (b, (index - b.start) as u32))
    }

    /// The position of the band holding `index` (`bands.len()` past the end).
    /// Bands are contiguous and in order, so this is a binary search.
    pub fn band_of(&self, index: usize) -> usize {
        self.bands.partition_point(|b| b.start + b.lines() as usize <= index)
    }

    /// The first `k` in `0..n` for which `pred` is false (`pred` must be
    /// true then false along `0..n`).
    fn first_not(n: usize, pred: impl Fn(usize) -> bool) -> usize {
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if pred(mid) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The global channel at `index` (`None` on a folded line: it is several).
    pub fn global(&self, index: usize) -> Option<u32> {
        self.at(index).filter(|(b, _)| !b.folded).map(|(b, k)| b.first_channel + k)
    }

    /// The global channels behind line `index`: one, or a folded band's all.
    pub fn channels(&self, index: usize) -> Range<u32> {
        match self.at(index) {
            Some((b, _)) if b.folded => b.first_channel..b.first_channel + b.channels,
            Some((b, k)) => b.first_channel + k..b.first_channel + k + 1,
            None => 0..0,
        }
    }

    /// The line of global channel `channel` (a folded band's one line).
    pub fn index_of(&self, channel: u32) -> Option<usize> {
        let b = self.bands.iter().find(|b| channel >= b.first_channel && channel < b.first_channel + b.channels)?;
        Some(if b.folded { b.start } else { b.start + (channel - b.first_channel) as usize })
    }

    pub fn band(&self, slot: u32) -> Option<&Band> {
        self.bands.iter().find(|b| b.slot == slot)
    }

    /// Where band `bi`'s separator strip starts (each band is its strip, then
    /// its channels).
    pub fn strip_pos(&self, bi: usize) -> f32 {
        self.bands.get(bi).map_or(self.extent(), |b| b.start as f32 * self.cell + bi as f32 * SEP)
    }

    /// Where channel `index` starts.
    pub fn pos(&self, index: usize) -> f32 {
        let bi = self.band_of(index);
        index as f32 * self.cell + (bi + 1).min(self.bands.len().max(1)) as f32 * SEP
    }

    /// The axis's whole length: every strip and channel.
    pub fn extent(&self) -> f32 {
        self.len as f32 * self.cell + self.bands.len() as f32 * SEP
    }

    /// The channel at offset `p` (`None` on a strip or off the axis).
    pub fn index_at(&self, p: f32) -> Option<usize> {
        // The last band whose strip starts at or before `p`.
        let bi = Self::first_not(self.bands.len(), |bi| self.strip_pos(bi) <= p).checked_sub(1)?;
        let b = &self.bands[bi];
        let from = self.strip_pos(bi) + SEP;
        let k = ((p - from) / self.cell).floor();
        (p >= from && k < b.lines() as f32).then(|| b.start + k as usize)
    }

    /// The channels whose cells touch offsets `a..b`.
    pub fn range(&self, a: f32, b: f32) -> Range<usize> {
        // Channel offsets only grow along the axis.
        let first = Self::first_not(self.len, |k| self.pos(k) + self.cell <= a);
        let end = Self::first_not(self.len, |k| self.pos(k) < b);
        if first < end {
            first..end
        } else {
            0..0
        }
    }
}

/// The lines (0-based within `band`; its channels unless folded) whose
/// indices fall in `visible`.
pub fn channels_in(band: &Band, visible: &Range<usize>) -> Range<u32> {
    let from = visible.start.max(band.start);
    let to = visible.end.min(band.start + band.lines() as usize);
    if from < to {
        (from - band.start) as u32..(to - band.start) as u32
    } else {
        0..0
    }
}

pub struct GridLayout {
    pub rows: Axis,
    pub cols: Axis,
    pub cell: f32,
}

impl GridLayout {
    /// The default layout: bands in slot-id order, none folded.
    pub fn new(slots: &[SlotState], cell: f32) -> Self {
        Self::arranged(slots, &[], cell, &crate::prefs::LayoutPrefs::default())
    }

    /// The layout as the user arranged it: saved order first, folded bands as one line.
    pub fn arranged(
        slots: &[SlotState],
        positions: &[PositionState],
        cell: f32,
        layout: &crate::prefs::LayoutPrefs,
    ) -> Self {
        let cell = cell.clamp(CELL_MIN, CELL_MAX);
        let keys = band_keys(slots, positions);
        let ins = Arrangement { keys: &keys, order: &layout.matrix_inputs, folded: &layout.folded_inputs };
        let outs = Arrangement { keys: &keys, order: &layout.matrix_outputs, folded: &layout.folded_outputs };
        let (mut rows, mut cols) = (Axis::inputs(slots, &ins), Axis::outputs(slots, &outs));
        (rows.cell, cols.cell) = (cell, cell);
        GridLayout { rows, cols, cell }
    }

    /// The (inputs, outputs) global channels behind a cell: one each for a
    /// plain cell, a folded band's all on a folded line.
    pub fn block(&self, row: usize, col: usize) -> Option<(Range<u32>, Range<u32>)> {
        let (i, o) = (self.rows.channels(row), self.cols.channels(col));
        (!i.is_empty() && !o.is_empty()).then_some((i, o))
    }

    /// Whether either line of a cell is a folded band's.
    pub fn folded(&self, row: usize, col: usize) -> bool {
        self.rows.at(row).is_some_and(|(b, _)| b.folded) || self.cols.at(col).is_some_and(|(b, _)| b.folded)
    }

    /// Width and height of the cell area, separator strips included.
    pub fn size(&self) -> (f32, f32) {
        (self.cols.extent(), self.rows.extent())
    }

    /// The (row, column) under a point relative to the cell area's top left
    /// (`None` on a separator strip).
    pub fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        Some((self.rows.index_at(y)?, self.cols.index_at(x)?))
    }

    /// Row and column ranges intersecting the rectangle (cell-area coordinates).
    pub fn visible(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (Range<usize>, Range<usize>) {
        (self.rows.range(y0, y1), self.cols.range(x0, x1))
    }

    /// A cell's top-left corner (cell-area coordinates).
    pub fn cell_pos(&self, row: usize, col: usize) -> (f32, f32) {
        (self.cols.pos(col), self.rows.pos(row))
    }

    /// The (input, output) global channels of a cell.
    pub fn point(&self, row: usize, col: usize) -> Option<(u32, u32)> {
        Some((self.rows.global(row)?, self.cols.global(col)?))
    }

    /// The cell of a point, if both channels are on the grid.
    pub fn cell_of(&self, input: u32, output: u32) -> Option<(usize, usize)> {
        Some((self.rows.index_of(input)?, self.cols.index_of(output)?))
    }

    /// `Mic in 1 → Speakers out 2`; a bus says `Verb return 1`, `Verb send 1`.
    pub fn label(&self, row: usize, col: usize) -> Option<String> {
        let (ib, ik) = self.rows.at(row)?;
        let (ob, ok) = self.cols.at(col)?;
        let (iw, ow) = (if ib.bus { "return" } else { "in" }, if ob.bus { "send" } else { "out" });
        let (ia, oa) = (if ib.bus { "returns" } else { "inputs" }, if ob.bus { "sends" } else { "outputs" });
        // A channel's custom name follows its number; a folded side is all of them.
        let end = |b: &Band, k: u32, word: &str, all: &str| {
            if b.folded {
                return format!("{}, all {all}", b.name);
            }
            let named =
                b.channel_labels.get(k as usize).cloned().flatten().map(|n| format!(" ({n})")).unwrap_or_default();
            format!("{} {word} {}{named}", b.name, k + 1)
        };
        Some(format!("{} → {}", end(ib, ik, iw, ia), end(ob, ok, ow, oa)))
    }
}

/// The label of a point from the slots (`?` for channels no slot owns).
pub fn point_label(slots: &[SlotState], input: u32, output: u32) -> String {
    let l = GridLayout::new(slots, CELL_DEFAULT);
    l.cell_of(input, output)
        .and_then(|(r, c)| l.label(r, c))
        .unwrap_or_else(|| format!("in {} → out {}", input + 1, output + 1))
}

/// One end of a block of channels: `Mic, all inputs` when it is a whole
/// band, `Mic in 2` for one channel, else the channel numbers.
pub fn range_label(slots: &[SlotState], input: bool, (start, end): (u32, u32)) -> String {
    let l = GridLayout::new(slots, CELL_DEFAULT);
    let axis = if input { &l.rows } else { &l.cols };
    let band = axis.bands.iter().find(|b| start >= b.first_channel && start < b.first_channel + b.channels);
    let (word, all) = match (input, band.is_some_and(|b| b.bus)) {
        (true, false) => ("in", "inputs"),
        (true, true) => ("return", "returns"),
        (false, false) => ("out", "outputs"),
        (false, true) => ("send", "sends"),
    };
    match band {
        Some(b) if start == b.first_channel && end == b.first_channel + b.channels && b.channels > 1 => {
            format!("{}, all {all}", b.name)
        }
        Some(b) if end == start + 1 => format!("{} {word} {}", b.name, start - b.first_channel + 1),
        _ if end == start + 1 => format!("{word} {}", start + 1),
        _ => format!("{word} {}\u{2013}{}", start + 1, end),
    }
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
    /// The routes behind a folded cell: global (start, end) channel ranges.
    Block {
        inputs: (u32, u32),
        outputs: (u32, u32),
    },
}

/// False when the selected slot or channels no longer exist.
pub fn selection_valid(selection: &Selection, slots: &[SlotState]) -> bool {
    match *selection {
        Selection::None => true,
        Selection::Slot(id) => slots.iter().any(|s| s.id == id),
        Selection::Cell { input, output } => GridLayout::new(slots, CELL_DEFAULT).cell_of(input, output).is_some(),
        Selection::Block { inputs, outputs } => {
            let l = GridLayout::new(slots, CELL_DEFAULT);
            let has = |a: &Axis, (s, e): (u32, u32)| s < e && a.index_of(s).is_some() && a.index_of(e - 1).is_some();
            has(&l.rows, inputs) && has(&l.cols, outputs)
        }
    }
}

/// The (row, column) a selection sits on: a cell's, or a block's line.
pub fn selection_cell(l: &GridLayout, s: &Selection) -> Option<(usize, usize)> {
    match *s {
        Selection::Cell { input, output } => l.cell_of(input, output),
        Selection::Block { inputs, outputs } => Some((l.rows.index_of(inputs.0)?, l.cols.index_of(outputs.0)?)),
        Selection::None | Selection::Slot(_) => None,
    }
}

/// What selecting (row, column) selects: the route's cell, or a folded cell's block.
pub fn selection_at(l: &GridLayout, row: usize, col: usize) -> Option<Selection> {
    if let Some((input, output)) = l.point(row, col) {
        return Some(Selection::Cell { input, output });
    }
    let (i, o) = l.block(row, col)?;
    Some(Selection::Block { inputs: (i.start, i.end), outputs: (o.start, o.end) })
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
    // A click only selects (the caller does that); a double-click toggles.
    if input.double_clicked {
        return Some(match current {
            Some(_) => Edit::RemovePoint { input: p.0, output: p.1 },
            None => set(p, 0.0, false, false),
        });
    }
    if input.clicked {
        return None;
    }
    let cur = current?;
    let s = step(input.fine);
    let delta = -input.drag_dy / DRAG_PX_PER_STEP * s + input.wheel_notches * s;
    if delta == 0.0 {
        return None;
    }
    regain(p, cur, delta)
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

    /// Two input devices of 2 and 3 channels, one output device of 2.
    fn two_bands() -> GridLayout {
        GridLayout::new(&[slot(1, "A", 0, 2, 0, 2), slot(2, "B", 2, 3, 0, 0)], 20.0)
    }

    #[test]
    fn each_device_has_a_separator_strip_before_its_channels() {
        let l = two_bands();
        // Rows: [strip A][a1][a2][strip B][b1][b2][b3]
        assert_eq!(l.rows.strip_pos(0), 0.0);
        assert_eq!(l.rows.pos(0), SEP);
        assert_eq!(l.rows.pos(1), SEP + 20.0);
        assert_eq!(l.rows.strip_pos(1), SEP + 40.0);
        assert_eq!(l.rows.pos(2), 2.0 * SEP + 40.0);
        assert_eq!(l.rows.extent(), 2.0 * SEP + 100.0);
        assert_eq!(l.size(), (SEP + 40.0, 2.0 * SEP + 100.0));
    }

    #[test]
    fn points_map_to_cells_and_strips_take_no_cell() {
        let l = two_bands();
        for k in 0..l.rows.len {
            assert_eq!(l.rows.index_at(l.rows.pos(k) + 10.0), Some(k), "row {k}");
        }
        assert_eq!(l.rows.index_at(SEP / 2.0), None, "the first strip");
        assert_eq!(l.rows.index_at(l.rows.strip_pos(1) + 1.0), None, "the second strip");
        assert_eq!(l.rows.index_at(-1.0), None);
        assert_eq!(l.rows.index_at(l.rows.extent() + 1.0), None);
        assert_eq!(l.cell_at(SEP + 25.0, 2.0 * SEP + 45.0), Some((2, 1)));
        assert_eq!(l.cell_at(5.0, 2.0 * SEP + 45.0), None, "on the column strip");
        // Visible ranges cover every cell touching the window.
        let (rows, cols) = l.visible(0.0, SEP + 30.0, 1000.0, 2.0 * SEP + 41.0);
        assert_eq!(rows, 1..3);
        assert_eq!(cols, 0..2);
    }

    #[test]
    fn custom_names_show_on_the_bands_and_in_crosspoint_labels() {
        let mut a = slot(1, "VASIO 1", 0, 2, 0, 2);
        a.label = Some("Ableton".into());
        a.input_labels = vec![Some("Kick".into()), None];
        a.output_labels = vec![None, None];
        let l = GridLayout::new(&[a], CELL_DEFAULT);
        assert_eq!(l.rows.bands[0].name, "Ableton");
        assert_eq!(l.label(0, 1).unwrap(), "Ableton in 1 (Kick) → Ableton out 2");
        assert_eq!(l.label(1, 0).unwrap(), "Ableton in 2 → Ableton out 1");
    }
    use confluence_api::ClockRole;

    fn slot(id: u32, name: &str, first_input: u32, inputs: u32, first_output: u32, outputs: u32) -> SlotState {
        SlotState {
            input_names: Vec::new(),
            output_names: Vec::new(),
            label: None,
            input_labels: Vec::new(),
            output_labels: Vec::new(),
            id,
            name: name.into(),
            device: String::new(),
            role: ClockRole::Soft,
            online: true,
            first_input,
            inputs,
            first_output,
            outputs,
            color: None,
        }
    }

    /// Positions holding the given slots (`vasio:A` holds slots 1 and 2...).
    fn positions(of: &[(&str, &[u32])]) -> Vec<confluence_api::PositionState> {
        of.iter()
            .map(|(pos, slots)| confluence_api::PositionState {
                pos: pos.parse().unwrap(),
                status: confluence_api::PositionStatus::Filled { online: true },
                device: None,
                shape: None,
                daw: None,
                master: false,
                color: None,
                slots: slots.to_vec(),
            })
            .collect()
    }

    /// Three devices of 2 inputs and 2 outputs: slots 1, 2, 3 at vasio:A, B, C.
    fn three() -> (Vec<SlotState>, Vec<confluence_api::PositionState>) {
        let slots = vec![slot(1, "A", 0, 2, 0, 2), slot(2, "B", 2, 2, 2, 2), slot(3, "C", 4, 2, 4, 2)];
        (slots, positions(&[("vasio:A", &[1]), ("vasio:B", &[2]), ("vasio:C", &[3])]))
    }

    #[test]
    fn band_keys_name_positions_and_buses() {
        let verb = SlotState { device: confluence_api::BUS_DEVICE.into(), ..slot(9, "Verb", 6, 1, 6, 1) };
        let mut slots = three().0;
        slots.push(verb);
        let keys = band_keys(&slots, &positions(&[("vasio:A", &[1]), ("asio:1", &[2])]));
        assert_eq!(keys[&1], "pos:vasio:A");
        assert_eq!(keys[&2], "pos:asio:1");
        assert_eq!(keys[&9], "bus:6", "a bus has no position: its colour key");
        assert_eq!(keys[&3], "slot:C", "a slot nowhere else: its colour key");
    }

    #[test]
    fn bands_follow_the_saved_order_and_new_ones_go_last() {
        let (slots, ps) = three();
        let lp = crate::prefs::LayoutPrefs {
            matrix_inputs: vec!["pos:vasio:C".into(), "gone".into(), "pos:vasio:A".into()],
            matrix_outputs: vec!["pos:vasio:B".into()],
            ..Default::default()
        };
        let l = GridLayout::arranged(&slots, &ps, CELL_DEFAULT, &lp);
        let rows: Vec<(u32, usize)> = l.rows.bands.iter().map(|b| (b.slot, b.start)).collect();
        assert_eq!(rows, vec![(3, 0), (1, 2), (2, 4)], "saved first, then the rest in position order");
        let cols: Vec<u32> = l.cols.bands.iter().map(|b| b.slot).collect();
        assert_eq!(cols, vec![2, 1, 3]);
        assert_eq!(l.point(0, 0), Some((4, 2)), "C in 1 → B out 1");
        assert_eq!(l.cell_of(0, 0), Some((2, 2)));
        assert_eq!(l.rows.bands[0].key, "pos:vasio:C");
    }

    #[test]
    fn a_folded_band_is_one_line_and_its_channels_find_it() {
        let (slots, ps) = three();
        let mut lp = crate::prefs::LayoutPrefs::default();
        lp.folded_inputs.insert("pos:vasio:B".into());
        let l = GridLayout::arranged(&slots, &ps, 20.0, &lp);
        let b = l.rows.band(2).unwrap();
        assert!(b.folded && b.lines() == 1 && b.channels == 2);
        assert_eq!(l.rows.len, 5, "2 + 1 + 2 lines");
        assert_eq!(l.rows.global(2), None, "a folded line is no single channel");
        assert_eq!(l.rows.channels(2), 2..4);
        assert_eq!(l.rows.channels(3), 4..5);
        assert_eq!(l.rows.index_of(2), Some(2));
        assert_eq!(l.rows.index_of(3), Some(2), "both of B's inputs find its line");
        assert_eq!(l.rows.index_of(4), Some(3));
        assert_eq!(l.rows.extent(), 3.0 * SEP + 5.0 * 20.0);
        assert_eq!(channels_in(b, &(0..5)), 0..1, "one line on screen");
        assert_eq!(l.cols.len, 6, "outputs are folded on their own");
    }

    #[test]
    fn a_cell_on_folded_lines_covers_both_ranges() {
        let (slots, ps) = three();
        let mut lp = crate::prefs::LayoutPrefs::default();
        lp.folded_inputs.insert("pos:vasio:A".into());
        lp.folded_outputs.insert("pos:vasio:C".into());
        let l = GridLayout::arranged(&slots, &ps, CELL_DEFAULT, &lp);
        assert_eq!(l.block(0, 0), Some((0..2, 0..1)), "A's inputs → A out 1");
        assert_eq!(l.block(0, 4), Some((0..2, 4..6)), "folded × folded");
        assert_eq!(l.block(1, 1), Some((2..3, 1..2)), "a plain cell is one channel each");
        assert_eq!(l.point(0, 0), None);
        assert_eq!(l.label(0, 0).unwrap(), "A, all inputs → A out 1");
        assert_eq!(l.label(1, 4).unwrap(), "B in 1 → C, all outputs");
        assert_eq!(l.block(9, 0), None);
    }

    #[test]
    fn lookups_on_a_folded_axis_agree_with_walking_it() {
        let slots: Vec<SlotState> =
            many(48).into_iter().map(|s| SlotState { name: format!("D{}", s.id), ..s }).collect();
        let mut lp = crate::prefs::LayoutPrefs::default();
        for s in slots.iter().filter(|s| s.id % 3 == 0) {
            lp.folded_inputs.insert(s.color_key());
        }
        let l = GridLayout::arranged(&slots, &[], 17.0, &lp);
        let a = &l.rows;
        let mut walked = Vec::new();
        let mut p = 0.0;
        for b in &a.bands {
            assert_eq!(b.folded, b.slot % 3 == 0, "slot {}", b.slot);
            p += SEP;
            for k in 0..b.lines() {
                walked.push((p, b.slot, k));
                p += a.cell;
            }
        }
        assert_eq!(a.extent(), p);
        assert_eq!(a.len, walked.len());
        for (i, &(at, slot, k)) in walked.iter().enumerate() {
            assert_eq!(a.pos(i), at, "pos {i}");
            assert_eq!(a.at(i).map(|(b, k)| (b.slot, k)), Some((slot, k)), "at {i}");
            assert_eq!(a.index_at(at + 1.0), Some(i), "index_at {i}");
            for c in a.channels(i) {
                assert_eq!(a.index_of(c), Some(i), "channel {c} finds line {i}");
            }
        }
        let mut x = -30.0;
        while x < p + 30.0 {
            let on = walked.iter().position(|&(at, _, _)| x >= at && x < at + a.cell);
            assert_eq!(a.index_at(x), on, "index_at {x}");
            let touching: Vec<usize> =
                (0..walked.len()).filter(|&i| walked[i].0 + a.cell > x && walked[i].0 < x + 40.0).collect();
            let want = touching.first().map_or(0..0, |&f| f..touching.last().unwrap() + 1);
            assert_eq!(a.range(x, x + 40.0), want, "range {x}");
            x += 3.7;
        }
    }

    #[test]
    fn a_blocks_ends_name_a_whole_band_or_its_channel() {
        let s = slots(); // Mic in 0; VASIO 1 in 1-2, out 2-3; Speakers out 0-1
        assert_eq!(range_label(&s, true, (1, 3)), "VASIO 1, all inputs");
        assert_eq!(range_label(&s, true, (2, 3)), "VASIO 1 in 2");
        assert_eq!(range_label(&s, false, (0, 2)), "Speakers, all outputs");
        assert_eq!(range_label(&s, false, (9, 11)), "out 10–11");
    }

    #[test]
    fn a_block_selection_is_valid_while_its_channels_exist() {
        let s = slots();
        assert!(selection_valid(&Selection::Block { inputs: (1, 3), outputs: (0, 2) }, &s));
        assert!(!selection_valid(&Selection::Block { inputs: (1, 9), outputs: (0, 2) }, &s), "inputs gone");
        assert!(!selection_valid(&Selection::Block { inputs: (1, 3), outputs: (3, 7) }, &s), "outputs gone");
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
    fn bus_cells_say_send_and_return() {
        let mic = slot(1, "Mic", 0, 1, 0, 1);
        let verb = SlotState { device: confluence_api::BUS_DEVICE.into(), ..slot(2, "Verb", 1, 1, 1, 1) };
        let slots = [mic, verb];
        assert_eq!(point_label(&slots, 0, 1), "Mic in 1 → Verb send 1");
        assert_eq!(point_label(&slots, 1, 0), "Verb return 1 → Mic out 1");
        assert_eq!(point_label(&slots, 1, 1), "Verb return 1 → Verb send 1");
    }
    #[test]
    fn a_devices_bands_share_a_colour_and_carry_the_chosen_one() {
        let dev = |s: SlotState, d: &str| SlotState { device: d.into(), ..s };
        let slots = [
            dev(slot(6, "VASIO 1 in", 0, 2, 0, 0), "vasio:1"),
            dev(slot(7, "VASIO 1 out", 0, 0, 0, 2), "vasio:1"),
            SlotState { color: Some([1, 2, 3]), ..dev(slot(8, "Game", 0, 0, 2, 2), "wasapi-out:Game") },
        ];
        let l = GridLayout::new(&slots, CELL_DEFAULT);
        let (vin, vout) = (l.rows.band(6).unwrap(), l.cols.band(7).unwrap());
        assert_eq!((vin.palette, vout.palette), (6, 6), "in and out of one device: one default colour");
        assert_eq!(l.cols.band(8).unwrap().palette, 8);
        assert_eq!(l.cols.band(8).unwrap().color, Some([1, 2, 3]));
        assert_eq!(vin.color, None);
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
        let at = |r: usize, c: usize, dx: f32, dy: f32| l.cell_at(l.cols.pos(c) + dx, l.rows.pos(r) + dy);
        assert_eq!(at(0, 0, 0.0, 0.0), Some((0, 0)));
        assert_eq!(at(2, 1, 19.9, 19.9), Some((2, 1)));
        assert_eq!(l.cell_at(l.cols.extent(), l.rows.pos(0)), None, "right of the last column");
        assert_eq!(l.cell_at(-1.0, 5.0), None);
        let (cb, rb) = (l.cols.bands.len() as f32, l.rows.bands.len() as f32);
        assert_eq!(l.size(), (80.0 + cb * SEP, 60.0 + rb * SEP));
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

    /// `n` devices whose channel counts cycle 1..=32 in both directions.
    fn many(n: u32) -> Vec<SlotState> {
        let mut first = 0;
        (0..n)
            .map(|i| {
                let ch = i % 32 + 1;
                let s = slot(i + 1, "D", first, ch, first, ch);
                first += ch;
                s
            })
            .collect()
    }

    #[test]
    fn lookups_on_a_many_device_axis_agree_with_walking_it() {
        let l = GridLayout::new(&many(64), 17.0);
        let a = &l.rows;
        // Walk the axis: each band's strip, then its channels.
        let mut walked = Vec::new();
        let mut p = 0.0;
        for b in &a.bands {
            p += SEP;
            for k in 0..b.channels {
                walked.push((p, b.slot, k));
                p += a.cell;
            }
        }
        assert_eq!(a.extent(), p);
        for (i, &(at, slot, k)) in walked.iter().enumerate() {
            assert_eq!(a.pos(i), at, "pos {i}");
            assert_eq!(a.at(i).map(|(b, k)| (b.slot, k)), Some((slot, k)), "at {i}");
            assert_eq!(a.index_at(at + 1.0), Some(i), "index_at {i}");
        }
        assert_eq!(a.at(walked.len()), None);
        let mut x = -30.0;
        while x < p + 30.0 {
            let on = walked.iter().position(|&(at, _, _)| x >= at && x < at + a.cell);
            assert_eq!(a.index_at(x), on, "index_at {x}");
            for w in [0.5, 40.0, 333.0] {
                let touching: Vec<usize> =
                    (0..walked.len()).filter(|&i| walked[i].0 + a.cell > x && walked[i].0 < x + w).collect();
                let want = touching.first().map_or(0..0, |&f| f..touching.last().unwrap() + 1);
                assert_eq!(a.range(x, x + w), want, "range {x}+{w}");
            }
            x += 3.7;
        }
    }

    #[test]
    fn a_frames_lookups_stay_cheap_on_a_huge_matrix() {
        // 256 devices, 4,000+ channels a side: what is on screen is found
        // without walking every channel of every device.
        let l = GridLayout::new(&many(256), CELL_MIN);
        let t = std::time::Instant::now();
        for i in 0..1000 {
            let y = (i % 200) as f32 * 300.0;
            let (rows, cols) = std::hint::black_box(l.visible(y, y, y + 900.0, y + 600.0));
            std::hint::black_box((l.rows.pos(rows.start), l.cols.index_at(y + 450.0), l.rows.at(cols.end)));
        }
        assert!(t.elapsed() < std::time::Duration::from_millis(60), "{:?}", t.elapsed());
    }

    #[test]
    fn a_bands_channels_on_screen_are_those_in_the_visible_range() {
        let l = two_bands();
        let (a, b) = (&l.rows.bands[0], &l.rows.bands[1]); // rows 0..2 and 2..5
        assert_eq!(channels_in(a, &(1..4)), 1..2);
        assert_eq!(channels_in(b, &(1..4)), 0..2);
        assert_eq!(channels_in(b, &(0..2)), 0..0, "none of its channels");
        assert_eq!(channels_in(a, &(0..0)), 0..0, "nothing visible");
        assert_eq!(channels_in(b, &(0..99)), 0..3);
    }

    #[test]
    fn only_visible_cells_are_returned_for_a_large_matrix() {
        let big = vec![slot(1, "Big", 0, 1024, 0, 1024)];
        let l = GridLayout::new(&big, 20.0);
        // One device: its strip comes first.
        let (rows, cols) = l.visible(400.0 + SEP, 2000.0 + SEP, 1200.0 + SEP, 2600.0 + SEP);
        assert_eq!(rows, 100..130);
        assert_eq!(cols, 20..60);
    }

    #[test]
    fn a_click_only_selects() {
        let click = CellInput { clicked: true, ..Default::default() };
        assert_eq!(cell_edit((1, 2), None, &click), None, "an empty cell is not routed");
        assert_eq!(cell_edit((1, 2), Some(&pt(1, 2, -6.0)), &click), None, "a routed cell is not unrouted");
    }

    #[test]
    fn a_double_click_toggles_the_route() {
        let dbl = CellInput { clicked: true, double_clicked: true, ..Default::default() };
        assert_eq!(
            cell_edit((1, 2), None, &dbl),
            Some(Edit::SetPoint { input: 1, output: 2, gain_db: 0.0, mute: false, invert: false })
        );
        let muted = PointState { mute: true, ..pt(1, 2, -12.0) };
        assert_eq!(cell_edit((1, 2), Some(&muted), &dbl), Some(Edit::RemovePoint { input: 1, output: 2 }));
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
    fn a_selection_is_dropped_when_its_slot_or_channels_go() {
        let s = slots();
        assert!(selection_valid(&Selection::Slot(2), &s));
        assert!(!selection_valid(&Selection::Slot(9), &s), "slot removed elsewhere");
        assert!(selection_valid(&Selection::Cell { input: 2, output: 3 }, &s));
        assert!(!selection_valid(&Selection::Cell { input: 5, output: 0 }, &s), "input channel gone");
        assert!(selection_valid(&Selection::None, &s));
    }
}
