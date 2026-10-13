//! The Devices screen: the fixed positions as hardware-like cards, grouped
//! in rows (spec: slot model §5). Each group shows its devices and one tray
//! for the next free position; "Show all" opens every position. Empty
//! trays open a picker that grows out of the tray; filled cards show the
//! device, its health on an OLED and its levels; Ctrl+click swaps.

use std::collections::{HashMap, HashSet};

use confluence_api::{
    byte_db, DeviceInfo, DeviceKind, MeterFrame, PosGroup, PosId, PositionState, PositionStatus, SlotHealth, SlotState,
};
use eframe::egui::{self, Align, Align2, Color32, Id, Key, Pos2, Rect, Sense, TextEdit, Vec2, WidgetInfo, WidgetType};

use crate::commands::Edit;
use crate::gear::motion::{Curve, Motion, ENTER};
use crate::gear::oled_meter::{Chan, Group};
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
    pub confirm: Option<Confirm>,
    /// The device being renamed on its card, and the text so far.
    pub renaming: Option<(PosId, String)>,
    /// The last click on a card's name (a second one soon after renames it).
    pub name_click: Option<(PosId, f64)>,
    /// App icons for app-capture cards and the app list.
    pub icons: crate::app_icon::IconCache,
    /// The running apps, read while an Apps picker is open.
    pub apps: Option<crate::running_apps::AppLister>,
    /// The app list's filter.
    pub app_filter: String,
    /// The app picker's "capture by process name or PID" section is open.
    pub pid_entry_open: bool,
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
            confirm: None,
            renaming: None,
            name_click: None,
            channels_of: None,
            icons: crate::app_icon::IconCache::default(),
            apps: None,
            app_filter: String::new(),
            pid_entry_open: false,
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
    pub(crate) fn open_picker(&mut self, pos: PosId, swap: bool, anchor: Rect) {
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
pub(crate) const SHAPES: [u32; 5] = [2, 4, 8, 16, 32];

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
        PositionStatus::Disabled => {
            Face { tag, name, sub, line1: "OFF".into(), line2: "ROUTES KEPT".into(), ..Default::default() }
        }
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

/// Content is centred and capped at this width on very wide windows.
pub const CONTENT_MAX: f32 = 1760.0;

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

/// The bus tray's unit width.
const BUS_TRAY_W: f32 = 220.0;
/// Between the bus tray's halves.
const GAP: f32 = 12.0;

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
            ui.add_space(8.0);
            crate::rack_view::show_sections(
                ui,
                left,
                width,
                &state.positions,
                &v,
                skin,
                palette,
                st,
                motion,
                prefs,
                &mut actions,
            );
            section_rule(ui, left, width, "Buses", skin);
            bus_tray(ui, left, BUS_TRAY_W, skin, st, &mut actions);
            ui.add_space(24.0);
        });
    });
    if let Some(pos) = st.picker {
        picker(ui.ctx(), pos, devices, &state.positions, skin, st, motion, prefs.advanced, &mut actions);
    }
    // The app list is read only while an Apps picker is open.
    if st.picker.is_none_or(|p| p.group != PosGroup::App) {
        st.apps = None;
        st.app_filter.clear();
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

/// The accessible label of a card: its tag, name and OLED lines.
pub(crate) fn card_label(f: &Face, empty: bool) -> String {
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
pub const POPOVER_W: f32 = 340.0;
pub const POPOVER_MAX_H: f32 = 520.0;

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

/// The app list's height in the picker.
const APP_LIST_H: f32 = 330.0;

/// The running apps, playing audio first, with a filter. Returns the
/// executable of the app clicked.
fn app_list(ui: &mut egui::Ui, st: &mut ScreenState, advanced: bool, skin: &GearSkin) -> Option<String> {
    let procs = st.apps.get_or_insert_with(crate::running_apps::AppLister::open).latest();
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    ui.add(TextEdit::singleline(&mut st.app_filter).hint_text("Filter apps\u{2026}").desired_width(f32::INFINITY));
    // Confluence itself is never a capture target.
    let own: Vec<u32> = procs
        .iter()
        .filter(|p| p.exe.to_ascii_lowercase().starts_with("confluence"))
        .map(|p| p.pid)
        .chain([std::process::id()])
        .collect();
    let (playing, other) = crate::running_apps::rows(&procs, &st.app_filter, advanced, &own);
    let mut chosen = None;
    let heading = |ui: &mut egui::Ui, text: &str| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(text).size(10.0).strong().color(paint::alpha(skin.ink, 0.55)));
    };
    // A fixed height: the popover keeps its size (and place) while the list loads.
    let (list, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), APP_LIST_H), Sense::hover());
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list).layout(egui::Layout::top_down(Align::Min)));
    let ui = &mut list_ui;
    egui::ScrollArea::vertical().id_salt("app-list").max_height(APP_LIST_H).auto_shrink([false, false]).show(
        ui,
        |ui| {
            if procs.is_empty() {
                ui.label(egui::RichText::new("Looking for apps\u{2026}").color(paint::alpha(skin.ink, 0.6)));
            }
            if !playing.is_empty() {
                heading(ui, "PLAYING AUDIO");
            }
            for r in &playing {
                if app_row(ui, r, advanced, &mut st.icons, skin) {
                    chosen = Some(r.exe.clone());
                }
            }
            if !other.is_empty() {
                heading(ui, if advanced { "OTHER APPS AND BACKGROUND PROCESSES" } else { "OTHER APPS" });
            }
            for r in &other {
                if app_row(ui, r, advanced, &mut st.icons, skin) {
                    chosen = Some(r.exe.clone());
                }
            }
            if !procs.is_empty() && playing.is_empty() && other.is_empty() {
                ui.label(egui::RichText::new("No apps match").color(paint::alpha(skin.ink, 0.6)));
            }
        },
    );
    chosen
}

/// One app in the list: its icon, name, executable (and PID with advanced
/// options) and, while it plays, a small level. True when clicked.
fn app_row(
    ui: &mut egui::Ui,
    r: &crate::running_apps::AppRow,
    advanced: bool,
    icons: &mut crate::app_icon::IconCache,
    skin: &GearSkin,
) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 38.0), Sense::click());
    let label = format!("Capture {}", r.name);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
    if !ui.is_rect_visible(rect) {
        return resp.clicked();
    }
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(
            rect,
            egui::CornerRadius::same(8),
            if skin.light() { Color32::from_black_alpha(14) } else { Color32::from_white_alpha(14) },
        );
    }
    let icon = Rect::from_min_size(rect.min + Vec2::new(8.0, 7.0), Vec2::splat(24.0));
    let tex = icons.get(ui.ctx(), &r.exe);
    p.image(tex.id(), icon, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    let ctx = ui.ctx().clone();
    let text_w = w - 44.0 - if r.audio { 44.0 } else { 8.0 };
    paint::truncated(
        p,
        rect.min + Vec2::new(40.0, 4.0),
        Align2::LEFT_TOP,
        &r.name,
        paint::font(&ctx, "label-bold", 13.0),
        skin.ink,
        text_w,
    );
    let mut sub = r.exe.clone();
    if advanced {
        if let Some(pid) = r.pids.first() {
            sub.push_str(&format!(" \u{b7} PID {pid}"));
            if r.pids.len() > 1 {
                sub.push_str(&format!(" +{}", r.pids.len() - 1));
            }
        }
    }
    paint::truncated(
        p,
        rect.min + Vec2::new(40.0, 21.0),
        Align2::LEFT_TOP,
        &sub,
        paint::font(&ctx, "label", 10.5),
        paint::alpha(skin.ink, 0.6),
        text_w,
    );
    if r.audio {
        // Six steps, lit by the session's peak.
        let lit = (r.level.clamp(0.0, 1.0).sqrt() * 6.0).round() as usize;
        for k in 0..6 {
            let x = rect.right() - 44.0 + k as f32 * 6.0;
            let bar = Rect::from_min_size(Pos2::new(x, rect.center().y - 5.0), Vec2::new(4.0, 10.0));
            let c = if k < lit.max(1) { skin.ink } else { paint::alpha(skin.ink, 0.15) };
            p.rect_filled(bar, egui::CornerRadius::same(1), c);
        }
    }
    resp.clicked()
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
    advanced: bool,
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
                        let pid_entry = advanced || cfg!(not(windows));
                        if pid_entry {
                            // A disclosure row (open by default where there is no list).
                            let label = "Advanced: capture by process name or PID";
                            let open = st.pid_entry_open || cfg!(not(windows));
                            let arrow = if open { "\u{25be}" } else { "\u{25b8}" };
                            let row = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{arrow} {label}"))
                                        .size(11.5)
                                        .color(paint::alpha(skin.ink, 0.7)),
                                )
                                .sense(Sense::click()),
                            );
                            row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
                            if row.clicked() {
                                st.pid_entry_open = !st.pid_entry_open;
                            }
                            if open {
                                ui.add(
                                    TextEdit::singleline(&mut st.app_name)
                                        .hint_text("process name or PID")
                                        .desired_width(f32::INFINITY),
                                );
                                let name = st.app_name.trim().to_string();
                                let b = ui
                                    .add_enabled_ui(!name.is_empty(), |ui| {
                                        paint::pill_labeled(ui, "Capture", "Capture", skin)
                                    })
                                    .inner;
                                if b.clicked() && !name.is_empty() {
                                    chosen = Some((DeviceKind::AppCapture, name));
                                }
                            }
                        }
                        if let Some(exe) = app_list(ui, st, advanced, skin) {
                            chosen = Some((DeviceKind::AppCapture, exe));
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
        assert_eq!(at, Pos2::new(1000.0 - POPOVER_W, 692.0), "pulled in from the right edge");
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
