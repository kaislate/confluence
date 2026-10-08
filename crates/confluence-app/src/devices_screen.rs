//! The Devices screen: the fixed positions as hardware-like cards, grouped
//! in rows (spec: slot model §5). Each group shows its devices and one tray
//! for the next free position; "Show all" opens every position. Empty
//! trays open a picker that grows out of the tray; filled cards show the
//! device, its health on an OLED and its levels; Ctrl+click swaps.

use std::collections::{HashMap, HashSet};

use confluence_api::{
    byte_db, DeviceInfo, DeviceKind, MeterFrame, PosGroup, PosId, PositionState, PositionStatus, SlotHealth, SlotState,
};
use eframe::egui::{self, Align2, Color32, Id, Key, Pos2, Rect, Sense, TextEdit, Vec2, WidgetInfo, WidgetType};

use crate::bays::{Bay, BayView};
use crate::commands::Edit;
use crate::gear::motion::{Curve, Motion, ENTER, PHOSPHOR_TAU, POP, SETTLE};
use crate::gear::oled_meter::{Chan, Geom, Group};
use crate::gear::paint;
use crate::gear::skins::{self, GearSkin, AMBER, GREEN, RED};
use crate::prefs::ViewPrefs;

/// What the screen asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenAction {
    Edit(Edit),
    /// Show this slot in the inspector.
    Select(u32),
}

/// An in-place confirmation on a card: clear it (true) or turn it off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Confirm {
    pub pos: PosId,
    pub clear: bool,
    pub at: f64,
}

/// Two clicks closer than this are a double-click.
pub const DOUBLE_CLICK_SECS: f64 = 0.45;

/// The confirmation strip gives up after this long.
pub const CONFIRM_SECS: f64 = 5.0;

pub struct ScreenState {
    /// The position whose picker is open.
    pub picker: Option<PosId>,
    /// The picker replaces the device there (Ctrl+click on a filled card).
    pub swap: bool,
    /// Where the picker grows from, and the frame it opened on.
    pub picker_anchor: Rect,
    pub picker_opened: u64,
    pub search: String,
    /// Positions being filled (the engine is opening the device).
    pub filling: HashSet<PosId>,
    /// Positions that just appeared (their card pops in).
    pub just_filled: HashSet<PosId>,
    /// Bays showing every position, not only the next free ones.
    pub expanded: HashSet<Bay>,
    pub confirm: Option<Confirm>,
    /// The device being renamed on its card, and the text so far.
    pub renaming: Option<(PosId, String)>,
    /// The last click on a card's name (a second one soon after renames it).
    pub name_click: Option<(PosId, f64)>,
    /// App icons for app-capture cards.
    pub icons: crate::app_icon::IconCache,
    /// The position whose channel list is open.
    pub channels_of: Option<PosId>,
    /// Channel names being typed, by (input?, channel index).
    /// Per channel: the text in its field, and whether it was typed in.
    pub channel_drafts: HashMap<(bool, u32), (String, bool)>,
    /// Each card's LED colour and when it last changed (for the bloom).
    pub led_seen: HashMap<PosId, (Option<Color32>, f64)>,
    pub adding_bus: bool,
    pub bus_name: String,
    pub bus_channels: u32,
    pub app_name: String,
    pub net_stream: String,
    pub net_channels: u32,
    pub net_address: String,
    frame: u64,
}

impl Default for ScreenState {
    fn default() -> Self {
        ScreenState {
            picker: None,
            swap: false,
            picker_anchor: Rect::NOTHING,
            picker_opened: 0,
            search: String::new(),
            filling: HashSet::new(),
            just_filled: HashSet::new(),
            expanded: HashSet::new(),
            confirm: None,
            renaming: None,
            name_click: None,
            channels_of: None,
            icons: crate::app_icon::IconCache::default(),
            channel_drafts: HashMap::new(),
            led_seen: HashMap::new(),
            adding_bus: false,
            bus_name: String::new(),
            bus_channels: 2,
            app_name: String::new(),
            net_stream: "Main".into(),
            net_channels: 2,
            net_address: String::new(),
            frame: 0,
        }
    }
}

impl ScreenState {
    fn open_picker(&mut self, pos: PosId, swap: bool, anchor: Rect) {
        self.picker = Some(pos);
        self.swap = swap;
        self.picker_anchor = anchor;
        self.picker_opened = self.frame;
        self.search.clear();
    }
}

pub fn row_title(g: PosGroup) -> &'static str {
    match g {
        PosGroup::Vasio | PosGroup::Vaio => "Virtual",
        PosGroup::Asio => "ASIO",
        PosGroup::WinIn => "Windows inputs",
        PosGroup::WinOut => "Windows outputs",
        PosGroup::App => "App capture",
        PosGroup::NetIn => "Network in",
        PosGroup::NetOut => "Network out",
    }
}

/// The device kinds a picker for `g` lists.
fn kinds(g: PosGroup) -> &'static [DeviceKind] {
    g.kinds()
}

/// VASIO shapes per direction (as the engine offers them).
const SHAPES: [u32; 5] = [2, 4, 8, 16, 32];

/// A device name split for the card: the name, and the vendor noise or
/// the kind as a sub-line. "Game (4- TC-HELICON GoXLR)" is "Game" over
/// "TC-HELICON GoXLR"; a network stream "peer/Main:2" is "Main" over "to peer".
pub fn split_name(kind: DeviceKind, raw: &str) -> (String, String) {
    let raw = raw.trim();
    match kind {
        DeviceKind::WasapiCapture | DeviceKind::WasapiRender => {
            if let Some((name, rest)) = raw.split_once(" (") {
                let inner = rest.trim_end_matches(')').trim();
                let digits = inner.chars().take_while(|c| c.is_ascii_digit()).count();
                let vendor = if digits > 0 && inner[digits..].starts_with("- ") { &inner[digits + 2..] } else { inner };
                (name.trim().to_string(), vendor.trim().to_string())
            } else {
                (raw.to_string(), crate::devices::kind_title(kind).to_string())
            }
        }
        DeviceKind::NetSend | DeviceKind::NetReceive => {
            let (peer, stream) = raw.split_once('/').unwrap_or((raw, "Main"));
            let stream = stream.split(':').next().unwrap_or(stream);
            let dir = if kind == DeviceKind::NetSend { "to" } else { "from" };
            (stream.to_string(), format!("{dir} {peer}"))
        }
        DeviceKind::Vasio => (raw.to_string(), "Virtual ASIO".into()),
        DeviceKind::Vaio => ("Windows playback".into(), "Virtual device".into()),
        _ => (raw.to_string(), crate::devices::kind_title(kind).to_string()),
    }
}

/// What one card shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Face {
    /// The position: "ASIO 1".
    pub tag: String,
    pub name: String,
    pub sub: String,
    pub line1: String,
    pub line2: String,
    pub led: Option<Color32>,
    /// The LED blinks (the device is opening).
    pub blink: bool,
}

/// One word for the slot's clock: the fault if there is one, else its state.
pub fn health_word(h: Option<&SlotHealth>, master: bool) -> String {
    match h {
        Some(h) if h.device_lost => "DEVICE LOST".into(),
        Some(h) if h.underruns + h.overruns > 0 => format!("XRUNS {}", h.underruns + h.overruns),
        _ if master => "MASTER CLOCK".into(),
        Some(h) if h.device_ppm.abs() >= 50.0 => format!("DRIFT {:+.0} PPM", h.device_ppm),
        Some(h) if h.target_frames > 0.0 => "IN SYNC".into(),
        Some(_) => "CLOCK OK".into(),
        None => String::new(),
    }
}

fn khz(rate: f64) -> String {
    let k = rate / 1000.0;
    if (k - k.round()).abs() < 0.05 {
        format!("{k:.0} kHz")
    } else {
        format!("{k:.1} kHz")
    }
}

/// `ins IN · outs OUT · word`, leaving out what is zero or empty.
fn join(parts: &[String]) -> String {
    parts.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" \u{b7} ")
}

pub fn face(p: &PositionState, view: &Views, filling: bool) -> Face {
    let tag = p.pos.label();
    let health = p.slots.iter().find_map(|id| view.health.iter().find(|h| h.id == *id));
    let (ins, outs) = p
        .slots
        .iter()
        .filter_map(|id| view.slots.iter().find(|s| s.id == *id))
        .fold((0, 0), |(i, o), s| (i + s.inputs, o + s.outputs));
    let ch = |n: u32, what: &str| if n > 0 { format!("{n} {what}") } else { String::new() };
    let shape = |s: Option<(u32, u32)>| s.map(|(i, o)| format!("{o}\u{d7}{i}")).unwrap_or_default();
    let (name, sub) = match (&p.device, p.pos.group) {
        (_, PosGroup::Vasio) => (p.daw.clone().unwrap_or_else(|| "No DAW".into()), "Virtual ASIO".into()),
        (Some(d), _) => split_name(d.kind, &d.name),
        (None, g) => split_name(g.kinds()[0], ""),
    };
    if filling {
        return Face {
            tag,
            name,
            sub,
            line1: "OPENING\u{2026}".into(),
            line2: String::new(),
            led: Some(AMBER),
            blink: true,
        };
    }
    match p.status {
        PositionStatus::Empty => Face { tag, ..Default::default() },
        PositionStatus::Off => Face { tag, name: "Switched off".into(), line1: "OFF".into(), ..Default::default() },
        PositionStatus::On { online } if p.pos.group == PosGroup::Vasio => Face {
            tag,
            name,
            sub,
            line1: format!("{} \u{b7} {}", if online { "ONLINE" } else { "NO DAW" }, shape(p.shape)),
            line2: health_word(health, p.master),
            led: Some(if online { GREEN } else { AMBER }),
            blink: false,
        },
        PositionStatus::On { online } => Face {
            tag,
            name,
            sub,
            line1: if online { "CAPTURING".into() } else { "IDLE".into() },
            line2: if online { join(&[ch(ins, "IN"), health_word(health, false)]) } else { "NO APP PLAYING".into() },
            led: Some(if online { GREEN } else { AMBER }),
            blink: false,
        },
        PositionStatus::Filled { online } => {
            let kind = p.device.as_ref().map(|d| d.kind);
            let net = health.and_then(|h| h.net);
            let (line1, line2) = if !online {
                ("OFFLINE".to_string(), "DEVICE MISSING".to_string())
            } else if let Some(n) = net {
                let l2 = if n.lost + n.late > 0 {
                    format!("LOST {} \u{b7} LATE {}", n.lost, n.late)
                } else {
                    format!("{} PACKETS", n.packets)
                };
                (format!("RECEIVING \u{b7} {}", ch(ins, "CH")), l2)
            } else if kind == Some(DeviceKind::NetSend) {
                (format!("SENDING \u{b7} {}", ch(outs, "CH")), health_word(health, p.master))
            } else {
                let rate = view.sample_rate.map(khz).unwrap_or_default();
                (join(&["ONLINE".into(), rate]), join(&[ch(ins, "IN"), ch(outs, "OUT"), health_word(health, p.master)]))
            };
            Face { tag, name, sub, line1, line2, led: Some(if online { GREEN } else { RED }), blink: false }
        }
    }
}

/// What the cards read from the store, gathered once per frame.
pub struct Views<'a> {
    pub slots: &'a [SlotState],
    pub health: &'a [SlotHealth],
    pub meters: Option<&'a MeterFrame>,
    pub sample_rate: Option<f64>,
}

/// The palette index a position takes by default: fixed per position, so a
/// device keeps its colour across sessions, and spread so a usual setup
/// (VASIO A, ASIO 1, two Windows inputs, three outputs, an app) gets eight
/// different colours.
pub fn position_palette(pos: PosId) -> u32 {
    let offset = match pos.group {
        PosGroup::Vasio | PosGroup::Vaio => 0,
        PosGroup::Asio => 1,
        PosGroup::WinIn => 2,
        PosGroup::NetIn => 3,
        PosGroup::WinOut => 4,
        PosGroup::NetOut => 5,
        PosGroup::App => 7,
    };
    (offset + pos.index as u32) % 8
}

/// A device's colour: the one chosen for it, else its position's default.
pub fn device_color(p: &PositionState, palette: &[Color32]) -> Color32 {
    if let Some(c) = p.color {
        return Color32::from_rgb(c[0], c[1], c[2]);
    }
    skins::palette_color(palette, position_palette(p.pos))
}

/// Card size, gaps and the bays' chrome.
pub const CARD_W: f32 = 220.0;
pub const CARD_H: f32 = 150.0;
pub const GAP: f32 = 12.0;
/// Between bays.
pub const BAY_GAP: f32 = 14.0;
/// The OLED well's inset from the card's sides, and the meter's from the well's.
const WELL_INSET: f32 = 8.0;
const METER_INSET: f32 = 5.0;
/// The width a single card's meter has.
pub const CARD_METER_W: f32 = CARD_W - 2.0 * (WELL_INSET + METER_INSET);
/// A bay's padding and its header strip.
pub const BAY_PAD: f32 = 12.0;
pub const BAY_HEADER: f32 = 30.0;
/// Content is centred and capped at this width on very wide windows.
pub const CONTENT_MAX: f32 = 1760.0;

/// How many columns a card takes: two when its meter, at full card bars,
/// is wider than a single card's meter. Worked out from the segment
/// geometry whatever the style, so switching style never moves cards.
pub fn card_span(groups: &[Group]) -> usize {
    let full = Geom::card().resolve(crate::gear::oled_meter::MeterStyle::Segments, 1.0, 50.0);
    let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(CARD_METER_W, 50.0));
    if crate::gear::oled_meter::meter_layout(groups, r, &full).overflow {
        2
    } else {
        1
    }
}

/// The span of position `p`'s card (empty and switched-off cards are single).
fn span_of(p: &PositionState, v: &Views) -> usize {
    if crate::bays::vacant(p) || matches!(p.status, PositionStatus::Off) {
        1
    } else {
        card_span(&device_groups(p, v))
    }
}

/// The width of a card `span` columns wide.
fn card_width(span: usize) -> f32 {
    span as f32 * CARD_W + span.saturating_sub(1) as f32 * GAP
}

/// A bay's size for `cols` columns and `rows` rows of cards.
pub fn bay_size(cols: usize, rows: usize) -> Vec2 {
    let (cols, rows) = (cols.max(1), rows.max(1));
    Vec2::new(
        cols as f32 * CARD_W + (cols - 1) as f32 * GAP + 2.0 * BAY_PAD,
        BAY_HEADER + rows as f32 * CARD_H + (rows - 1) as f32 * GAP + BAY_PAD,
    )
}

/// The channels of position `p`'s slots as meter groups: one IN and one OUT
/// group, each channel with its number, name (custom or the device's) and
/// the latest levels from `v.meters`.
pub fn device_groups(p: &PositionState, v: &Views) -> Vec<Group> {
    let slots: Vec<&SlotState> = p.slots.iter().filter_map(|id| v.slots.iter().find(|s| s.id == *id)).collect();
    let mut ins = Vec::new();
    let mut outs = Vec::new();
    for s in &slots {
        for i in 0..s.inputs {
            let c = s.first_input + i;
            ins.push(chan(v, true, c, ins.len() as u32 + 1, crate::names::channel(s, true, i as usize)));
        }
        for i in 0..s.outputs {
            let c = s.first_output + i;
            outs.push(chan(v, false, c, outs.len() as u32 + 1, crate::names::channel(s, false, i as usize)));
        }
    }
    let mut groups = Vec::new();
    if !ins.is_empty() {
        groups.push(Group { label: format!("IN {}", ins.len()), channels: ins });
    }
    if !outs.is_empty() {
        groups.push(Group { label: format!("OUT {}", outs.len()), channels: outs });
    }
    groups
}

/// Global channel `c` (input or output) as a meter channel.
fn chan(v: &Views, input: bool, c: u32, number: u32, name: String) -> Chan {
    let silent = Chan { number, name, ..Chan::silent() };
    let Some(f) = v.meters else { return silent };
    let (first, list, clipped) =
        if input { (f.first_input, &f.inputs, &f.clipped_in) } else { (f.first_output, &f.outputs, &f.clipped_out) };
    match c.checked_sub(first).and_then(|i| list.get(i as usize)) {
        Some(m) => Chan { peak_db: byte_db(m[0]), rms_db: byte_db(m[1]), clipped: clipped.contains(&c), ..silent },
        None => silent,
    }
}

/// Shows the screen; `palette` is the colours offered for a device.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    view: &confluence_client::StoreView,
    devices: &[DeviceInfo],
    skin: &GearSkin,
    palette: &[Color32],
    st: &mut ScreenState,
    motion: &mut Motion,
    prefs: &mut ViewPrefs,
    editable: bool,
) -> Vec<ScreenAction> {
    let mut actions = Vec::new();
    st.frame += 1;
    let Some(state) = view.state.as_ref() else {
        paint::etched_text(
            ui.painter(),
            ui.max_rect().center(),
            Align2::CENTER_CENTER,
            "Waiting for the engine\u{2026}",
            skin,
            skin.ground_ink,
            13.0,
            false,
            0.0,
            0.7,
        );
        return actions;
    };
    let v = Views {
        slots: &state.slots,
        health: &view.health,
        meters: view.meters.as_ref(),
        sample_rate: view.status.as_ref().map(|s| s.sample_rate),
    };
    if let Some(c) = st.confirm {
        if motion.now() - c.at > CONFIRM_SECS {
            st.confirm = None;
        }
    }
    // The screen is revealed, not slid in.
    let reveal = motion.tween_from(Id::new("devices-reveal"), 0.0, 1.0, Curve::Enter, 0.15);
    ui.set_opacity(reveal);
    // The meter bridge, above the bays (unless it is in its own window).
    if !prefs.bridge.popped {
        let full = ui.available_width();
        let width = (full - 32.0).min(CONTENT_MAX);
        let left = ui.max_rect().left() + (full - width) / 2.0;
        let h = if prefs.bridge.shown {
            crate::bridge::bridge_height(prefs.bridge.height_frac, ui.available_height())
        } else {
            26.0
        };
        let (band, _) = ui.allocate_exact_size(Vec2::new(full, h + 18.0), Sense::hover());
        let r = Rect::from_min_size(Pos2::new(left, band.top() + 10.0), Vec2::new(width, h));
        if prefs.bridge.shown {
            let only = prefs.only_custom_names;
            let all = crate::bridge::bridge_devices(&state.positions, &v, &crate::prefs::BridgePrefs::default(), only);
            let shown = crate::bridge::bridge_devices(&state.positions, &v, &prefs.bridge, only);
            let look = prefs.meter;
            let resp = crate::bridge::show_bridge(ui, r, &all, &shown, skin, &mut prefs.bridge, look, motion, false);
            if resp.toggle_popout {
                prefs.bridge.popped = true;
            }
        } else {
            crate::bridge::collapsed_strip(ui, r, skin, &mut prefs.bridge);
        }
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            let full = ui.available_width();
            let width = (full - 32.0).min(CONTENT_MAX);
            let left = ui.max_rect().left() + (full - width) / 2.0;
            let max_cols = (((width - 2.0 * BAY_PAD + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
            let views = crate::bays::bay_views(&state.positions, &st.expanded);
            let spans: Vec<Vec<usize>> =
                views.iter().map(|b| b.cards.iter().map(|p| span_of(p, &v)).collect()).collect();
            let sizes: Vec<Vec2> = spans
                .iter()
                .map(|s| {
                    let (_, cols, rows) = crate::bays::place_cards(s, max_cols);
                    bay_size(cols, rows)
                })
                .collect();
            let widths: Vec<f32> = sizes.iter().map(|s| s.x).collect();
            ui.add_space(8.0);
            for row in crate::bays::pack_bays(&widths, width, BAY_GAP) {
                let h = row.iter().map(|&i| sizes[i].y).fold(0.0, f32::max);
                let (band, _) = ui.allocate_exact_size(Vec2::new(full, h + BAY_GAP), Sense::hover());
                let mut x = left;
                for i in row {
                    let r = Rect::from_min_size(Pos2::new(x, band.top()), sizes[i]);
                    bay(ui, r, &views[i], &spans[i], max_cols, &v, skin, palette, st, motion, prefs, &mut actions);
                    x += sizes[i].x + BAY_GAP;
                }
            }
            section_rule(ui, left, width, "Buses", skin);
            bus_tray(ui, left, CARD_W, skin, st, &mut actions);
            ui.add_space(24.0);
        });
    });
    if let Some(pos) = st.picker {
        picker(ui.ctx(), pos, devices, &state.positions, skin, st, motion, &mut actions);
    }
    if let Some(pos) = st.channels_of {
        channel_editor(ui.ctx(), pos, &state.positions, &v, st, &mut actions);
    }
    actions
}

/// Every channel of a device with a field for its custom name (spec: meter
/// bridge 4.5). A name is committed with Enter or when the field loses focus.
fn channel_editor(
    ctx: &egui::Context,
    pos: PosId,
    positions: &[PositionState],
    v: &Views,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let Some(p) = positions.iter().find(|p| p.pos == pos) else {
        st.channels_of = None;
        return;
    };
    let slots: Vec<&SlotState> = p.slots.iter().filter_map(|id| v.slots.iter().find(|s| s.id == *id)).collect();
    let mut open = true;
    egui::Window::new(format!("Channels of {}", pos.label()))
        .id(Id::new("channel-editor"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(320.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for s in &slots {
                    for input in [true, false] {
                        let n = if input { s.inputs } else { s.outputs };
                        if n == 0 {
                            continue;
                        }
                        ui.label(if input { "Inputs" } else { "Outputs" });
                        egui::Grid::new(("channels", s.id, input)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                            for i in 0..n {
                                let names = if input { &s.input_names } else { &s.output_names };
                                let device_name =
                                    names.get(i as usize).cloned().unwrap_or_else(|| format!("Ch {}", i + 1));
                                let labels = if input { &s.input_labels } else { &s.output_labels };
                                let current = labels.get(i as usize).cloned().flatten().unwrap_or_default();
                                ui.label(format!("{}", i + 1));
                                let (draft, edited) =
                                    st.channel_drafts.entry((input, i)).or_insert((current.clone(), false));
                                if !*edited {
                                    // Untouched fields follow the engine (a name set elsewhere).
                                    draft.clone_from(&current);
                                }
                                let field = ui.add(
                                    TextEdit::singleline(draft)
                                        .hint_text(device_name)
                                        .char_limit(confluence_api::MAX_LABEL)
                                        .desired_width(220.0),
                                );
                                *edited |= field.changed();
                                let enter = ui.input(|k| k.key_pressed(Key::Enter));
                                if field.lost_focus() || (enter && field.has_focus()) {
                                    if let Some(name) = name_to_send(draft, &current, *edited) {
                                        actions.push(ScreenAction::Edit(Edit::SetSlotLabel {
                                            id: s.id,
                                            channel: Some(confluence_api::ChannelRef { input, index: i }),
                                            name: Some(name),
                                        }));
                                    }
                                    *edited = false;
                                }
                                ui.end_row();
                            }
                        });
                    }
                }
            });
        });
    if !open {
        st.channels_of = None;
    }
}

/// An etched group title with a hairline rule.
fn section_rule(ui: &mut egui::Ui, left: f32, width: f32, title: &str, skin: &GearSkin) -> Rect {
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter();
    let text = paint::etched_text(
        p,
        Pos2::new(left + 2.0, r.center().y + 4.0),
        Align2::LEFT_CENTER,
        &title.to_uppercase(),
        skin,
        skin.ground_ink,
        11.0,
        true,
        0.14,
        0.75,
    );
    let y = r.center().y + 4.5;
    paint::seam(p, Pos2::new(text.right() + 12.0, y), Pos2::new(left + width, y), skin);
    r
}

/// One bay: a recessed zone with a coloured strip and its etched title, an
/// expander when positions are folded away, and its cards.
#[allow(clippy::too_many_arguments)]
fn bay(
    ui: &mut egui::Ui,
    r: Rect,
    bv: &BayView,
    spans: &[usize],
    max_cols: usize,
    v: &Views,
    skin: &GearSkin,
    palette: &[Color32],
    st: &mut ScreenState,
    motion: &mut Motion,
    prefs: &ViewPrefs,
    actions: &mut Vec<ScreenAction>,
) {
    let p = ui.painter_at(r.expand(4.0));
    paint::recess(&p, r, skin, 16);
    let colour = bv.bay.color();
    let strip = Rect::from_min_size(r.min + Vec2::new(BAY_PAD + 2.0, 15.0), Vec2::new(20.0, 3.0));
    p.rect_filled(strip, egui::CornerRadius::same(2), colour);
    p.rect_filled(strip.expand(2.0), egui::CornerRadius::same(3), paint::alpha(colour, 0.18));
    let title = bv.bay.title().to_uppercase();
    let text = paint::etched_text(
        &p,
        Pos2::new(strip.right() + 8.0, strip.center().y),
        Align2::LEFT_CENTER,
        &title,
        skin,
        skin.ground_ink,
        10.5,
        true,
        0.16,
        0.82,
    );
    let (_, label) = ui.allocate_exact_size(Vec2::ZERO, Sense::hover());
    label.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &title));
    let _ = text;
    // The expander: every position of the bay, or back to the devices.
    let expanded = st.expanded.contains(&bv.bay);
    if bv.hidden > 0 || expanded {
        let name = match bv.bay {
            Bay::Hardware => "ASIO",
            other => other.title(),
        };
        let (shown, accessible) = if expanded {
            ("Show fewer".to_string(), format!("Show fewer {name} positions"))
        } else {
            (format!("+{} more", bv.hidden), format!("Show all {name} positions"))
        };
        let at =
            Rect::from_min_size(Pos2::new(r.right() - BAY_PAD - 96.0, r.top() + 4.0), Vec2::new(96.0, paint::PILL_H));
        let mut child =
            ui.new_child(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if paint::pill_labeled(&mut child, &shown, &accessible, skin).clicked() {
            if expanded {
                st.expanded.remove(&bv.bay);
            } else {
                st.expanded.insert(bv.bay);
            }
        }
    }
    let (places, _, _) = crate::bays::place_cards(spans, max_cols);
    for ((pos, &(row, col)), &span) in bv.cards.iter().zip(&places).zip(spans) {
        let at = r.min + Vec2::new(BAY_PAD + col as f32 * (CARD_W + GAP), BAY_HEADER + row as f32 * (CARD_H + GAP));
        let w = card_width(span.clamp(1, max_cols.max(1)));
        card(ui, Rect::from_min_size(at, Vec2::new(w, CARD_H)), pos, v, skin, palette, st, motion, prefs, actions);
    }
}

/// The accessible label of a card: its tag, name and OLED lines.
fn card_label(f: &Face, empty: bool) -> String {
    if empty {
        return format!("{} \u{b7} click to choose a device", f.tag);
    }
    [f.tag.as_str(), f.name.as_str(), f.line1.as_str(), f.line2.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut egui::Ui,
    r: Rect,
    p: &PositionState,
    v: &Views,
    base: &GearSkin,
    palette: &[Color32],
    st: &mut ScreenState,
    motion: &mut Motion,
    prefs: &ViewPrefs,
    actions: &mut Vec<ScreenAction>,
) {
    let colour = device_color(p, palette);
    let filling = st.filling.contains(&p.pos);
    let mut f = face(p, v, filling);
    // The display name: a custom name first (spec: meter bridge §4.4).
    let custom = p.slots.iter().find_map(|id| v.slots.iter().find(|s| s.id == *id)).and_then(|s| s.label.clone());
    let (name, sub) = crate::names::display(custom.as_deref(), &f.name, prefs.only_custom_names);
    if custom.is_some() {
        f.sub = sub.unwrap_or_default();
        f.name = name;
    }
    let id = Id::new(("position-card", p.pos.to_string()));
    let empty = p.status == PositionStatus::Empty;
    let off = p.status == PositionStatus::Off;
    let resp = ui.interact(r, id, Sense::click());
    if st.picker == Some(p.pos) {
        st.picker_anchor = r; // the popover follows its tray while the screen scrolls
    }
    let enabled = ui.is_enabled();
    let label = card_label(&f, empty);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &label));
    let painter = ui.painter_at(r.expand(40.0));
    if empty {
        // A recess where a device would go, with a "+".
        let hover = motion.spring(id.with("hover"), if resp.hovered() { 1.0 } else { 0.0 }, SETTLE);
        paint::recess(&painter, r.shrink(1.0), base, 12);
        let ink = base.ground_ink;
        paint::etched_text(
            &painter,
            r.center() - Vec2::new(0.0, 12.0),
            Align2::CENTER_CENTER,
            "+",
            base,
            ink,
            26.0,
            false,
            0.0,
            0.55 + 0.45 * hover,
        );
        paint::etched_text(
            &painter,
            r.center() + Vec2::new(0.0, 18.0),
            Align2::CENTER_CENTER,
            &f.tag,
            base,
            ink,
            10.5,
            true,
            0.14,
            0.62 + 0.38 * hover,
        );
        if resp.clicked() {
            st.open_picker(p.pos, false, r);
        }
        return;
    }
    let resp = if p.pos.group == PosGroup::Vasio && matches!(p.status, PositionStatus::On { .. }) {
        resp.on_hover_text(format!(
            "Your DAW may list this as \"Confluence VASIO {}\" until the drivers are re-registered",
            p.pos.index + 1
        ))
    } else if !p.pos.group.is_virtual() && !off {
        resp.on_hover_text("Ctrl+click to swap the device")
    } else {
        resp
    };
    let lit = base.for_device(Some(colour));
    let skin = if off { lit.powered_off() } else { lit };
    // Motion: a hover lift, a pop when the card appears, a crossfade between on and off.
    let hovered = ui.rect_contains_pointer(r);
    let lift = motion.spring(id.with("lift"), if hovered && !off { 1.0 } else { 0.0 }, SETTLE);
    let scale = if st.just_filled.contains(&p.pos) {
        motion.spring_from(id.with("pop"), 0.94, 1.0, POP)
    } else {
        motion.spring(id.with("pop"), 1.0, POP)
    };
    if scale >= 0.999 {
        st.just_filled.remove(&p.pos);
    }
    let r = Rect::from_center_size(r.center(), r.size() * scale);
    let power = motion.decay(id.with("power"), if off { 0.0 } else { 1.0 }, 0.12);
    paint::panel_lifted(&painter, r, &skin, None, lift, paint::PANEL_RADIUS);
    if power > 0.0 && power < 1.0 {
        // The lit face fades over the dark one (or away from it).
        let mut lit_painter = painter.clone();
        lit_painter.set_opacity(power);
        paint::panel_lifted(&lit_painter, r, &lit, None, lift, paint::PANEL_RADIUS);
    }
    let w = r.width();
    // An app's icon, painted on the card's corner like a badge.
    let badge = p.pos.group == PosGroup::App && !off;
    if let (true, Some(d)) = (badge, p.device.as_ref()) {
        let tex = st.icons.get(ui.ctx(), &d.name);
        let (c, side, angle) = crate::app_icon::badge_rect(r);
        paint::textured_rounded(
            &painter,
            r,
            paint::PANEL_RADIUS as f32,
            tex.id(),
            |q| crate::app_icon::badge_uv(q, c, side, angle),
            crate::app_icon::BADGE_TINT,
        );
    }
    // Row 1: the LED, the position tag, a master tag; the controls on the right.
    let led_at = r.min + Vec2::new(17.0, 19.0);
    let now = motion.now();
    let seen = st.led_seen.entry(p.pos).or_insert((f.led, now));
    if seen.0 != f.led {
        *seen = (f.led, now);
    }
    let since = seen.1;
    let bloom = 1.0 + 0.6 * (1.0 - motion.since(since, 0.4));
    let want = match f.led {
        Some(_) if f.blink => f32::from(u8::from(motion.blink(2.0))),
        Some(_) => 1.0,
        None => 0.0,
    };
    let glow = motion.phosphor(id.with("led"), want, PHOSPHOR_TAU) * bloom;
    let led_colour = f.led.unwrap_or(Color32::GRAY);
    paint::led_glow(&painter, led_at, &skin, led_colour, glow);
    if f.led.is_some() {
        let led = ui.interact(Rect::from_center_size(led_at, Vec2::splat(14.0)), id.with("led"), Sense::click());
        led.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, format!("Clear clips {}", p.pos.label())));
        if led.clicked() {
            actions.push(ScreenAction::Edit(Edit::ClearClip));
        }
    }
    let tag_rect = paint::tag(&painter, led_at + Vec2::new(13.0, 0.0), Align2::LEFT_CENTER, &f.tag, &skin, 0.78);
    if p.master {
        paint::etched_text(
            &painter,
            Pos2::new(tag_rect.right() + 7.0, led_at.y),
            Align2::LEFT_CENTER,
            "MASTER",
            &skin,
            if skin.mould { skin.ink } else { skin.accent },
            10.0,
            true,
            0.12,
            1.0,
        );
    }
    // Row 2: the name (double-click to rename it) and its sub-line.
    let ctx = ui.ctx().clone();
    let name_rect = Rect::from_min_size(r.min + Vec2::new(12.0, 31.0), Vec2::new(w - 24.0, 20.0));
    let first_slot = p.slots.first().copied();
    let renaming = st.renaming.as_ref().is_some_and(|(pos, _)| *pos == p.pos);
    if renaming {
        let mut text = st.renaming.as_ref().map(|r| r.1.clone()).unwrap_or_default();
        let edit = ui.put(
            name_rect,
            TextEdit::singleline(&mut text).hint_text("Custom name").font(paint::font(&ctx, "label-bold", 14.0)),
        );
        if !edit.has_focus() && !edit.lost_focus() {
            edit.request_focus();
        }
        let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
        if escape {
            st.renaming = None;
        } else if edit.lost_focus() || enter {
            st.renaming = None;
            if let Some(id) = first_slot {
                actions.push(ScreenAction::Edit(Edit::SetSlotLabel { id, channel: None, name: Some(text) }));
            }
        } else if let Some(r) = st.renaming.as_mut() {
            r.1 = text;
        }
    } else {
        paint::truncated(
            &painter,
            r.min + Vec2::new(15.0, 33.0),
            Align2::LEFT_TOP,
            &f.name,
            paint::font(&ctx, "label-bold", 15.0),
            skin.ink,
            w - 30.0,
        );
        if !off && first_slot.is_some() {
            let name_resp = ui.interact(name_rect, id.with("name"), Sense::click());
            let what = format!("Rename {}", p.pos.label());
            name_resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &what));
            // A double-click, timed here so screen readers (whose clicks
            // never pair up as double-clicks) can rename too.
            if name_resp.clicked() {
                let t = ui.input(|i| i.time);
                match st.name_click {
                    Some((pos, at)) if pos == p.pos && t - at < DOUBLE_CLICK_SECS => {
                        st.name_click = None;
                        st.renaming = Some((p.pos, custom.clone().unwrap_or_default()));
                    }
                    _ => st.name_click = Some((p.pos, t)),
                }
            }
        }
    }
    if !f.sub.is_empty() {
        paint::truncated(
            &painter,
            r.min + Vec2::new(15.0, 51.0),
            Align2::LEFT_TOP,
            &f.sub,
            paint::font(&ctx, "label", 10.5),
            paint::alpha(skin.ink, 0.62),
            w - 30.0,
        );
    }
    // The OLED: the state line on top, the meters below.
    let well = Rect::from_min_size(r.min + Vec2::new(WELL_INSET, 66.0), Vec2::new(w - 2.0 * WELL_INSET, 74.0));
    paint::oled_well(&painter, well, &skin);
    let inner = well.shrink2(Vec2::new(METER_INSET, 5.0));
    if prefs.meter.style == crate::gear::oled_meter::MeterStyle::DotMatrix && !off {
        // The display's own pixel grid, behind its text and meters.
        crate::gear::oled_meter::dot_grid(&painter.with_clip_rect(well.shrink(2.0)), well.shrink(2.0), inner.height());
    }
    let state_text = if f.line2.is_empty() { f.line1.clone() } else { format!("{} {}", f.line1, f.line2) };
    let state_text: String =
        state_text.replace('\u{b7}', " ").replace('\u{2026}', "...").replace('\u{d7}', "X").to_uppercase();
    let mut text_mesh = egui::epaint::Mesh::default();
    let oled_c = paint::alpha(skin.oled, if off { 0.3 } else { 0.9 });
    if off {
        crate::gear::pixel_font::draw(&mut text_mesh, inner.left_top(), "OFF", 2.0, oled_c, false);
    } else {
        crate::gear::pixel_font::draw(&mut text_mesh, inner.left_top(), &state_text, 1.0, oled_c, false);
    }
    painter.with_clip_rect(inner).add(egui::Shape::mesh(text_mesh));
    if !off {
        let groups = device_groups(p, v);
        if !groups.is_empty() {
            // The state line has its own row; the meter fills the rest.
            let mrect = Rect::from_min_max(Pos2::new(inner.left(), inner.top() + 9.0), inner.max);
            let ppp = ui.ctx().pixels_per_point();
            let full = Geom::card().resolve(prefs.meter.style, ppp, mrect.height());
            let geom = crate::gear::oled_meter::fit_geom(&groups, mrect.width(), full);
            let m =
                crate::gear::oled_meter::meter_widget(ui, id.with("meter"), mrect, &groups, &geom, prefs.meter, motion);
            let what = format!("Channels of {}", p.pos.label());
            m.response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &what));
            if m.response.clicked() {
                st.channels_of = Some(p.pos);
                st.channel_drafts.clear();
            }
        }
    }
    // The controls, top right: faint until the card is hovered.
    let quiet = motion.spring(id.with("controls"), if hovered || off { 1.0 } else { 0.38 }, SETTLE);
    // (Left of an app's badge.)
    let right = r.right() - if badge { 44.0 } else { 10.0 };
    let controls =
        Rect::from_min_max(Pos2::new(r.left() + 70.0, r.top() + 7.0), Pos2::new(right, r.top() + 7.0 + paint::PILL_H));
    let mut row = ui
        .new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::right_to_left(egui::Align::Center)));
    row.set_opacity(quiet);
    row.spacing_mut().item_spacing.x = 5.0;
    let label = p.pos.label();
    let virt = p.pos.group.is_virtual();
    if let Some(c) = st.confirm.filter(|c| c.pos == p.pos) {
        row.set_opacity(1.0);
        confirm_strip(&mut row, c, &skin, st, actions);
    } else if off {
        if paint::pill_labeled(&mut row, "Turn on", &format!("Turn on {label}"), &skin).clicked() {
            st.just_filled.insert(p.pos);
            actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: None }));
        }
    } else {
        if let Some(&slot) = p.slots.first() {
            colour_menu(&mut row, slot, &label, colour, palette, &skin, actions);
        }
        if virt {
            if p.pos.group == PosGroup::Vasio {
                shape_menu(&mut row, p, &skin, actions);
            }
            if paint::pill_labeled(&mut row, "Off", &format!("Turn off {label}"), &skin).clicked() {
                st.confirm = Some(Confirm { pos: p.pos, clear: false, at: now });
            }
        } else {
            if paint::pill_labeled(&mut row, "Clear\u{2026}", &format!("Clear {label}"), &skin).clicked() {
                st.confirm = Some(Confirm { pos: p.pos, clear: true, at: now });
            }
            if p.pos.group == PosGroup::Asio
                && !p.master
                && paint::pill_labeled(&mut row, "Master", &format!("Make {label} master"), &skin).clicked()
            {
                actions.push(ScreenAction::Edit(Edit::SetMaster { pos: Some(p.pos) }));
            }
        }
    }
    if resp.clicked() && !off {
        if ui.input(|i| i.modifiers.command) && !virt {
            st.open_picker(p.pos, true, r);
        } else if let Some(&slot) = p.slots.first() {
            actions.push(ScreenAction::Select(slot));
        }
    }
}

/// "Its routes are removed. [Yes] [No]" in the card's control row.
fn confirm_strip(
    ui: &mut egui::Ui,
    c: Confirm,
    skin: &GearSkin,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let verb = if c.clear { "Clear" } else { "Turn off" };
    let question = format!("{verb} {}? Its routes are removed.", c.pos.label());
    let (r, text) = ui.allocate_exact_size(Vec2::new(96.0, paint::PILL_H), Sense::hover());
    text.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &question));
    paint::truncated(
        ui.painter(),
        r.left_center(),
        Align2::LEFT_CENTER,
        "Routes go too?",
        paint::font(ui.ctx(), "label-bold", 11.5),
        skin.ink,
        r.width(),
    );
    if paint::pill_tinted(ui, "Yes", verb, RED, skin).clicked() {
        st.confirm = None;
        actions.push(ScreenAction::Edit(if c.clear {
            Edit::ClearPosition { pos: c.pos }
        } else {
            Edit::SetVirtual { pos: c.pos, on: false, shape: None }
        }));
    }
    if paint::pill_labeled(ui, "No", "Cancel", skin).clicked() {
        st.confirm = None;
    }
}

/// VASIO's shape, as the DAW sees it: both directions together, or each.
fn shape_menu(ui: &mut egui::Ui, p: &PositionState, skin: &GearSkin, actions: &mut Vec<ScreenAction>) {
    let (ins, outs) = p.shape.unwrap_or((8, 8)); // engine side: (DAW outputs, DAW inputs)
    let pill = paint::pill_labeled(ui, "Shape\u{2026}", &format!("Shape of {}", p.pos.label()), skin);
    egui::Popup::menu(&pill).show(|ui| {
        for n in SHAPES {
            if ui.button(format!("{n}\u{d7}{n}")).clicked() {
                actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: Some((n, n)) }));
            }
        }
        ui.separator();
        ui.label("DAW inputs");
        ui.horizontal(|ui| {
            for n in SHAPES {
                if ui.selectable_label(outs == n, n.to_string()).clicked() {
                    actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: Some((ins, n)) }));
                }
            }
        });
        ui.label("DAW outputs");
        ui.horizontal(|ui| {
            for n in SHAPES {
                if ui.selectable_label(ins == n, n.to_string()).clicked() {
                    actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: Some((n, outs)) }));
                }
            }
        });
    });
}

fn colour_menu(
    ui: &mut egui::Ui,
    slot: u32,
    label: &str,
    current: Color32,
    palette: &[Color32],
    skin: &GearSkin,
    actions: &mut Vec<ScreenAction>,
) {
    let swatch = paint::swatch(ui, current, &format!("Colour of {label}"), skin);
    egui::Popup::menu(&swatch).show(|ui| {
        ui.horizontal_wrapped(|ui| {
            for c in palette {
                let rgb = [c.r(), c.g(), c.b()];
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                ui.painter().circle_filled(r.center(), 8.0, *c);
                if current == *c {
                    ui.painter().circle_stroke(r.center(), 9.5, egui::Stroke::new(2.0, skin.ground_ink));
                }
                if resp.clicked() {
                    actions.push(ScreenAction::Edit(Edit::SetSlotColor { id: slot, color: Some(rgb) }));
                }
            }
        });
        if ui.button("Default").clicked() {
            actions.push(ScreenAction::Edit(Edit::SetSlotColor { id: slot, color: None }));
        }
    });
}

/// Adding an insert bus: a tray with an OLED name field and a channel stepper.
fn bus_tray(
    ui: &mut egui::Ui,
    left: f32,
    card_w: f32,
    skin: &GearSkin,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
    let r = Rect::from_min_size(Pos2::new(left, row.min.y + 4.0), Vec2::new(card_w * 2.0 + GAP, 60.0));
    let p = ui.painter_at(r.expand(10.0));
    paint::recess(&p, r, skin, 12);
    let oled = Rect::from_min_size(r.min + Vec2::new(14.0, 14.0), Vec2::new(card_w - 60.0, 32.0));
    paint::oled_well(&p, oled, skin);
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(r.shrink2(Vec2::new(14.0, 14.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    inner.spacing_mut().item_spacing.x = 10.0;
    let field = TextEdit::singleline(&mut st.bus_name)
        .hint_text("Bus name")
        .font(paint::font(inner.ctx(), "oled", 20.0))
        .text_color(skin.oled)
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::symmetric(8, 4))
        .desired_width(oled.width() - 16.0);
    inner.add(field);
    inner.add(egui::DragValue::new(&mut st.bus_channels).range(1..=64).suffix(" ch"));
    let name = st.bus_name.trim().to_string();
    if st.adding_bus {
        inner.add(egui::Spinner::new());
    } else {
        let ok = !name.is_empty();
        let b = inner.add_enabled_ui(ok, |ui| paint::pill_labeled(ui, "Add bus", "Add insert bus", skin)).inner;
        if b.clicked() && ok {
            st.adding_bus = true;
            actions.push(ScreenAction::Edit(Edit::AddBus { name, channels: st.bus_channels }));
        }
    }
}

/// Whether another position holds `d`, and which.
fn held_elsewhere(d: &DeviceInfo, pos: PosId, positions: &[PositionState]) -> Option<PosId> {
    positions
        .iter()
        .find(|p| p.pos != pos && p.device.as_ref().is_some_and(|x| x.kind == d.kind && x.name == d.name))
        .map(|p| p.pos)
}

/// The picker popover's width, and the most it can grow to.
pub const POPOVER_W: f32 = 300.0;
pub const POPOVER_MAX_H: f32 = 440.0;

/// Where a popover anchored to `anchor` goes, kept inside `screen`: its
/// top-left corner under the tray, or (true) its bottom-left corner above
/// it when the room below is short. Decided from the largest size the
/// popover can take, so it never jumps once it has measured itself.
pub fn popover_pos(anchor: Rect, screen: Rect) -> (Pos2, bool) {
    let gap = 8.0;
    let x = anchor.left().min(screen.right() - POPOVER_W).max(screen.left());
    if anchor.bottom() + gap + POPOVER_MAX_H <= screen.bottom() {
        (Pos2::new(x, anchor.bottom() + gap), false)
    } else if anchor.top() - gap - POPOVER_MAX_H >= screen.top() {
        (Pos2::new(x, anchor.top() - gap), true)
    } else {
        (Pos2::new(x, screen.top() + gap), false)
    }
}

/// A device row in the picker; `disabled` rows say why.
fn device_row(
    ui: &mut egui::Ui,
    name: &str,
    sub: &str,
    accessible: &str,
    disabled: Option<String>,
    skin: &GearSkin,
) -> bool {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
    let enabled = disabled.is_none();
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, accessible));
    let p = ui.painter();
    if resp.hovered() && enabled {
        p.rect_filled(
            r,
            egui::CornerRadius::same(8),
            if skin.light() { Color32::from_black_alpha(14) } else { Color32::from_white_alpha(14) },
        );
    }
    let ink = if enabled { skin.ink } else { paint::alpha(skin.ink, 0.4) };
    let ctx = ui.ctx().clone();
    paint::truncated(
        p,
        r.min + Vec2::new(10.0, 5.0),
        Align2::LEFT_TOP,
        name,
        paint::font(&ctx, "label-bold", 13.0),
        ink,
        w - 20.0,
    );
    let sub = disabled.unwrap_or_else(|| sub.to_string());
    paint::truncated(
        p,
        r.min + Vec2::new(10.0, 22.0),
        Align2::LEFT_TOP,
        &sub,
        paint::font(&ctx, "label", 10.5),
        paint::alpha(skin.ink, 0.6),
        w - 20.0,
    );
    let resp = if enabled { resp } else { resp.on_hover_text("in use by another position") };
    resp.clicked() && enabled
}

fn channels_text(d: &DeviceInfo) -> String {
    match (d.inputs, d.outputs) {
        (0, 0) => String::new(),
        (0, o) => format!("{o} out"),
        (i, 0) => format!("{i} in"),
        (i, o) => format!("{i} in \u{b7} {o} out"),
    }
}

#[allow(clippy::too_many_arguments)]
fn picker(
    ctx: &egui::Context,
    pos: PosId,
    devices: &[DeviceInfo],
    positions: &[PositionState],
    skin: &GearSkin,
    st: &mut ScreenState,
    motion: &mut Motion,
    actions: &mut Vec<ScreenAction>,
) {
    let mut open = true;
    let mut chosen: Option<(DeviceKind, String)> = None;
    let title = if st.swap { format!("Swap {}", pos.label()) } else { format!("Choose a device for {}", pos.label()) };
    let width = POPOVER_W;
    let id = Id::new("device-picker");
    let screen = ctx.content_rect();
    let fade = motion.tween_from(id.with("fade"), 0.0, 1.0, Curve::Enter, ENTER * 0.65);
    // The panel under the content is painted at last frame's size.
    let last_size = ctx.data(|d| d.get_temp::<Vec2>(id.with("size"))).unwrap_or(Vec2::new(width, 200.0));
    let (at, above) = popover_pos(st.picker_anchor, screen);
    let at = at + Vec2::new(0.0, 4.0 * (1.0 - fade));
    let pivot = if above { Align2::LEFT_BOTTOM } else { Align2::LEFT_TOP };
    let area = egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(at).pivot(pivot).constrain_to(screen).show(
        ctx,
        |ui| {
            ui.set_opacity(fade);
            // A new area measures itself in an invisible pass; the frame is
            // run again at once so nothing (nor anyone) sees it misplaced.
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("picker sizing");
            }
            // The floating panel, under the content, at last frame's size.
            paint::floating(ui.painter(), Rect::from_min_size(ui.cursor().min, last_size), skin, 12);
            let outer = egui::Frame::NONE.inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                ui.set_width(width - 28.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                paint::etched_text(
                    ui.painter(),
                    ui.cursor().min + Vec2::new(0.0, 6.0),
                    Align2::LEFT_CENTER,
                    &title.to_uppercase(),
                    skin,
                    skin.ink,
                    10.5,
                    true,
                    0.12,
                    0.7,
                );
                ui.add_space(16.0);
                match pos.group {
                    PosGroup::App => {
                        ui.add(
                            TextEdit::singleline(&mut st.app_name)
                                .hint_text("process name or PID")
                                .desired_width(f32::INFINITY),
                        );
                        let name = st.app_name.trim().to_string();
                        let b = ui
                            .add_enabled_ui(!name.is_empty(), |ui| paint::pill_labeled(ui, "Capture", "Capture", skin))
                            .inner;
                        if b.clicked() && !name.is_empty() {
                            chosen = Some((DeviceKind::AppCapture, name));
                        }
                    }
                    PosGroup::NetOut => {
                        ui.horizontal(|ui| {
                            ui.add(
                                TextEdit::singleline(&mut st.net_stream).hint_text("stream name").desired_width(120.0),
                            );
                            ui.add(egui::DragValue::new(&mut st.net_channels).range(1..=64).suffix(" ch"));
                        });
                        let stream = st.net_stream.trim().to_string();
                        for d in devices.iter().filter(|d| d.kind == DeviceKind::NetSend) {
                            if device_row(
                                ui,
                                &format!("Send to {}", d.name),
                                "another Confluence engine",
                                &format!("Send to {}", d.name),
                                stream.is_empty().then(|| "name the stream first".into()),
                                skin,
                            ) {
                                chosen =
                                    Some((DeviceKind::NetSend, format!("{}/{stream}:{}", d.name, st.net_channels)));
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.add(
                                TextEdit::singleline(&mut st.net_address)
                                    .hint_text("Address (ip:port)")
                                    .desired_width(150.0),
                            );
                            let to = st.net_address.trim().to_string();
                            let ok = !to.is_empty() && !stream.is_empty();
                            let b = ui
                                .add_enabled_ui(ok, |ui| paint::pill_labeled(ui, "Send here", "Send here", skin))
                                .inner;
                            if b.clicked() && ok {
                                chosen = Some((DeviceKind::NetSend, format!("{to}/{stream}:{}", st.net_channels)));
                            }
                        });
                    }
                    g => {
                        let all: Vec<&DeviceInfo> = devices.iter().filter(|d| kinds(g).contains(&d.kind)).collect();
                        if all.len() > 6 {
                            let search = ui.add(
                                TextEdit::singleline(&mut st.search).hint_text("Search").desired_width(f32::INFINITY),
                            );
                            if st.picker_opened == st.frame {
                                search.request_focus();
                            }
                        }
                        let needle = st.search.trim().to_lowercase();
                        let list: Vec<&DeviceInfo> = all
                            .into_iter()
                            .filter(|d| needle.is_empty() || d.name.to_lowercase().contains(&needle))
                            .collect();
                        if list.is_empty() {
                            let text = if g == PosGroup::NetIn {
                                "No streams arriving from other engines"
                            } else if needle.is_empty() {
                                "No devices of this kind found"
                            } else {
                                "Nothing matches"
                            };
                            ui.add_space(4.0);
                            paint::etched_text(
                                ui.painter(),
                                ui.cursor().min + Vec2::new(10.0, 8.0),
                                Align2::LEFT_CENTER,
                                text,
                                skin,
                                skin.ink,
                                12.0,
                                false,
                                0.0,
                                0.6,
                            );
                            ui.add_space(20.0);
                        }
                        let enter = ui.input(|i| i.key_pressed(Key::Enter));
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            for (k, d) in list.iter().enumerate() {
                                let elsewhere = held_elsewhere(d, pos, positions);
                                let accessible =
                                    if g == PosGroup::NetIn { format!("Receive {}", d.name) } else { d.name.clone() };
                                let (name, sub) = split_name(d.kind, &d.name);
                                let sub = join(&[sub, channels_text(d)]);
                                let why = elsewhere.map(|p| format!("in use by {}", p.label()));
                                let hit = device_row(ui, &name, &sub, &accessible, why, skin);
                                if hit || (enter && k == 0 && elsewhere.is_none() && !needle.is_empty()) {
                                    chosen = Some((d.kind, d.name.clone()));
                                }
                            }
                        });
                    }
                }
            });
            outer.response.rect
        },
    );
    let rect = area.inner;
    ctx.data_mut(|d| d.insert_temp(id.with("size"), rect.size()));
    // Esc, or a click outside, closes it; the click that opened it does not.
    let anchor = st.picker_anchor;
    let (esc, clicked_out) = ctx.input(|i| {
        let out = i.pointer.any_pressed()
            && i.pointer.interact_pos().is_some_and(|q| !rect.contains(q) && !anchor.contains(q));
        (i.key_pressed(Key::Escape), out)
    });
    if esc || (clicked_out && st.picker_opened < st.frame.saturating_sub(1)) {
        open = false;
    }
    if let Some((kind, name)) = chosen {
        st.filling.insert(pos);
        st.just_filled.insert(pos);
        actions.push(ScreenAction::Edit(Edit::FillPosition { pos, kind, name }));
        open = false;
    }
    if !open {
        st.picker = None;
        st.swap = false;
    }
}

/// The name a channel field sends when it is committed: nothing unless the
/// user typed in it and the cleaned text differs from the current name ("" clears).
fn name_to_send(draft: &str, current: &str, edited: bool) -> Option<String> {
    if !edited {
        return None;
    }
    let name = confluence_api::clean_label(Some(draft)).unwrap_or_default();
    (name != current).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(ins: usize, outs: usize) -> Vec<Group> {
        let chans = |n: usize| (0..n).map(|i| Chan { number: i as u32 + 1, ..Chan::silent() }).collect::<Vec<_>>();
        [("IN", ins), ("OUT", outs)]
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(l, n)| Group { label: format!("{l} {n}"), channels: chans(n) })
            .collect()
    }

    #[test]
    fn many_channel_devices_take_double_cards() {
        for (ins, outs) in [(2, 0), (8, 8), (0, 2), (0, 8)] {
            assert_eq!(card_span(&io(ins, outs)), 1, "{ins}x{outs}");
        }
        for (ins, outs) in [(16, 16), (23, 10)] {
            assert_eq!(card_span(&io(ins, outs)), 2, "{ins}x{outs}");
        }
        // Whatever the style or scale, a single card's meter holds what card_span gave it.
        use crate::gear::oled_meter::{fit_geom, meter_layout, MeterStyle};
        for style in MeterStyle::all() {
            let g = io(8, 8);
            let full = Geom::card().resolve(style, 1.0, 50.0);
            assert_eq!(fit_geom(&g, CARD_METER_W, full), full, "{style:?}: an 8x8 card keeps full bars at 100 %");
            let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(CARD_METER_W, 50.0));
            assert!(!meter_layout(&g, r, &full).overflow);
        }
    }

    #[test]
    fn a_channel_name_is_sent_only_when_edited_and_changed() {
        assert_eq!(name_to_send("Kick", "Snare", false), None, "an untouched draft never overwrites");
        assert_eq!(name_to_send(" Snare ", "Snare", true), None, "unchanged after trimming");
        assert_eq!(name_to_send("Kick", "Snare", true), Some("Kick".to_string()));
        assert_eq!(name_to_send("  ", "Snare", true), Some(String::new()), "cleared");
        let long = "x".repeat(100);
        let cut = "x".repeat(confluence_api::MAX_LABEL);
        assert_eq!(name_to_send(&long, &cut, true), None, "the engine's cut is the same name");
    }
    use confluence_api::{PositionDevice, PositionState, PositionStatus};
    fn st(pos: &str, status: PositionStatus) -> PositionState {
        PositionState {
            pos: pos.parse().unwrap(),
            status,
            device: None,
            shape: None,
            daw: None,
            master: false,
            color: None,
            slots: vec![],
        }
    }
    #[test]
    fn device_names_are_split_from_their_vendor_noise() {
        assert_eq!(
            split_name(DeviceKind::WasapiRender, "Game (4- TC-HELICON GoXLR)"),
            ("Game".into(), "TC-HELICON GoXLR".into())
        );
        assert_eq!(split_name(DeviceKind::WasapiCapture, "Microphone (Yeti)"), ("Microphone".into(), "Yeti".into()));
        assert_eq!(split_name(DeviceKind::Asio, "GoXLR ASIO Driver"), ("GoXLR ASIO Driver".into(), "ASIO".into()));
        assert_eq!(split_name(DeviceKind::NetSend, "127.0.0.1:9/Main:2"), ("Main".into(), "to 127.0.0.1:9".into()));
        assert_eq!(split_name(DeviceKind::NetReceive, "studio/Guest"), ("Guest".into(), "from studio".into()));
    }

    #[test]
    fn the_oled_speaks_in_plain_words() {
        let views = Views { slots: &[], health: &[], meters: None, sample_rate: Some(48_000.0) };
        let mut p = st("asio:1", PositionStatus::Filled { online: true });
        p.device = Some(PositionDevice { kind: DeviceKind::Asio, name: "GoXLR ASIO Driver".into() });
        p.master = true;
        let f = face(&p, &views, false);
        assert_eq!(f.line1, "ONLINE \u{b7} 48 kHz");
        assert_eq!(f.line2, "MASTER CLOCK");
        assert_eq!(f.led, Some(GREEN));
        let f = face(&st("asio:1", PositionStatus::Filled { online: false }), &views, false);
        assert_eq!((f.line1.as_str(), f.line2.as_str()), ("OFFLINE", "DEVICE MISSING"));
        assert_eq!(f.led, Some(RED));
        let f = face(&st("vasio:B", PositionStatus::Off), &views, false);
        assert_eq!(f.line1, "OFF");
        assert_eq!(f.led, None);
        let f = face(&st("asio:1", PositionStatus::Empty), &views, true);
        assert!(f.blink && f.led == Some(AMBER));
        assert_eq!(khz(44_100.0), "44.1 kHz");
    }

    #[test]
    fn the_health_word_names_the_fault_first() {
        let h = |ppm: f64, xruns: u64, lost: bool| SlotHealth {
            id: 1,
            underruns: xruns,
            overruns: 0,
            fill_frames: 100.0,
            target_frames: 128.0,
            device_ppm: ppm,
            correction_ppm: 0.0,
            device_lost: lost,
            device_faults: 0,
            driver_requests: 0,
            attached: None,
            idle_note: None,
            net: None,
        };
        assert_eq!(health_word(Some(&h(3.0, 0, false)), false), "IN SYNC");
        assert_eq!(health_word(Some(&h(120.0, 0, false)), false), "DRIFT +120 PPM");
        assert_eq!(health_word(Some(&h(0.0, 3, false)), true), "XRUNS 3", "a fault beats the master word");
        assert_eq!(health_word(Some(&h(0.0, 0, true)), false), "DEVICE LOST");
        assert_eq!(health_word(None, true), "MASTER CLOCK");
    }

    #[test]
    fn a_popover_opens_under_its_tray_or_above_when_there_is_no_room() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0));
        let tray = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(240.0, 190.0));
        assert_eq!(popover_pos(tray, screen), (Pos2::new(100.0, 298.0), false));
        let low = Rect::from_min_size(Pos2::new(800.0, 700.0), Vec2::new(240.0, 190.0));
        let (at, above) = popover_pos(low, screen);
        assert!(above, "no room below: its bottom sits over the tray");
        assert_eq!(at, Pos2::new(700.0, 692.0), "pulled in from the right edge");
        let short = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        assert_eq!(popover_pos(tray, short), (Pos2::new(100.0, 8.0), false), "neither fits: pinned to the top");
    }

    #[test]
    fn a_devices_colour_defaults_from_its_position_and_a_usual_setup_gets_eight() {
        let palette = skins::DEVICE_PALETTE.to_vec();
        let mut p = st("asio:1", PositionStatus::Filled { online: true });
        p.slots = vec![9, 10];
        assert_eq!(device_color(&p, &palette), palette[1]);
        p.color = Some([1, 2, 3]);
        assert_eq!(device_color(&p, &palette), Color32::from_rgb(1, 2, 3));
        let usual = ["vasio:A", "asio:1", "win-in:1", "win-in:2", "win-out:1", "win-out:2", "win-out:3", "app:1"];
        let mut seen: Vec<u32> = usual.iter().map(|s| position_palette(s.parse().unwrap())).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), usual.len(), "all different");
    }
}
