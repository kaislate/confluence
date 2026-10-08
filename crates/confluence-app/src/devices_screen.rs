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

use crate::commands::Edit;
use crate::gear::motion::{Curve, Motion, ENTER, PHOSPHOR_TAU, POP, SETTLE};
use crate::gear::paint;
use crate::gear::skins::{self, GearSkin, AMBER, GREEN, RED};

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
    /// Groups showing every position, not only the next free one.
    pub expanded: HashSet<PosGroup>,
    pub confirm: Option<Confirm>,
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

/// The card rows, in screen order: Virtual (VASIO and VAIO, switched-on
/// VASIOs first), ASIO, WIN IN, WIN OUT, APP, NET IN, NET OUT.
pub fn card_rows(positions: &[PositionState]) -> Vec<(PosGroup, Vec<&PositionState>)> {
    let of = |g: PosGroup| -> Vec<&PositionState> {
        let mut v: Vec<&PositionState> = positions.iter().filter(|p| p.pos.group == g).collect();
        v.sort_by_key(|p| p.pos.index);
        v
    };
    let mut virt = of(PosGroup::Vasio);
    // Stable: on ones keep letter order, then the off ones in letter order.
    virt.sort_by_key(|p| matches!(p.status, PositionStatus::Off));
    virt.extend(of(PosGroup::Vaio));
    let mut rows = vec![(PosGroup::Vasio, virt)];
    for g in [PosGroup::Asio, PosGroup::WinIn, PosGroup::WinOut, PosGroup::App, PosGroup::NetIn, PosGroup::NetOut] {
        rows.push((g, of(g)));
    }
    rows
}

/// One group as shown: its cards, and how many positions stay folded away.
pub struct GroupView<'a> {
    pub group: PosGroup,
    pub cards: Vec<&'a PositionState>,
    pub hidden: usize,
}

/// A position that holds nothing the user chose: an empty tray or a
/// switched-off virtual position.
fn vacant(p: &PositionState) -> bool {
    matches!(p.status, PositionStatus::Empty | PositionStatus::Off)
}

/// The groups with their wall collapsed: every device, then one vacant
/// position (the next free one); `expanded` groups show every position.
pub fn group_views<'a>(positions: &'a [PositionState], expanded: &HashSet<PosGroup>) -> Vec<GroupView<'a>> {
    card_rows(positions)
        .into_iter()
        .filter(|(_, cards)| !cards.is_empty())
        .map(|(group, cards)| {
            if expanded.contains(&group) {
                return GroupView { group, cards, hidden: 0 };
            }
            let mut shown: Vec<&PositionState> = cards.iter().copied().filter(|p| !vacant(p)).collect();
            let vacant_ones = cards.iter().copied().filter(|p| vacant(p)).count();
            shown.extend(cards.iter().copied().find(|p| vacant(p)));
            GroupView { group, cards: shown, hidden: vacant_ones.saturating_sub(1) }
        })
        .collect()
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

/// Card size: the width adapts a little so rows fill the screen.
pub const CARD_H: f32 = 194.0;
pub const CARD_W_MIN: f32 = 228.0;
pub const CARD_W_MAX: f32 = 264.0;
pub const GAP: f32 = 20.0;
/// Content is centred and capped at this width on very wide windows.
pub const CONTENT_MAX: f32 = 1680.0;

/// How many cards fit across `width`, and how wide each is.
pub fn cards_across(width: f32) -> (usize, f32) {
    let n = (((width + GAP) / (CARD_W_MIN + GAP)).floor() as usize).max(1);
    let w = ((width - (n as f32 - 1.0) * GAP) / n as f32).clamp(CARD_W_MIN, CARD_W_MAX);
    (n, w)
}

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

/// The levels of `p`'s channels: (input?, channel, peak dB).
fn levels(p: &PositionState, v: &Views) -> Vec<(bool, u32, f32)> {
    let Some(f) = v.meters else { return Vec::new() };
    let mut out = Vec::new();
    for s in p.slots.iter().filter_map(|id| v.slots.iter().find(|s| s.id == *id)) {
        for c in s.first_input..s.first_input + s.inputs {
            if let Some(m) = c.checked_sub(f.first_input).and_then(|i| f.inputs.get(i as usize)) {
                out.push((true, c, byte_db(m[0])));
            }
        }
        for c in s.first_output..s.first_output + s.outputs {
            if let Some(m) = c.checked_sub(f.first_output).and_then(|i| f.outputs.get(i as usize)) {
                out.push((false, c, byte_db(m[0])));
            }
        }
    }
    out
}

/// A device's colour: the one chosen for it, else the palette entry for its
/// lowest slot id (what the matrix uses for its bands), else its position's.
pub fn device_color(p: &PositionState, palette: &[Color32]) -> Color32 {
    if let Some(c) = p.color {
        return Color32::from_rgb(c[0], c[1], c[2]);
    }
    let n = p.slots.iter().copied().min().unwrap_or(p.pos.index as u32 + 1);
    skins::palette_color(palette, n)
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
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            let full = ui.available_width();
            let width = (full - 32.0).min(CONTENT_MAX);
            let left = ui.max_rect().left() + (full - width) / 2.0;
            let (n, cw) = cards_across(width);
            for gv in group_views(&state.positions, &st.expanded) {
                group_header(ui, left, width, &gv, skin, st);
                for line in gv.cards.chunks(n) {
                    let (row, _) = ui.allocate_exact_size(Vec2::new(full, CARD_H + GAP), Sense::hover());
                    for (k, p) in line.iter().enumerate() {
                        let r = Rect::from_min_size(
                            Pos2::new(left + k as f32 * (cw + GAP), row.min.y + 4.0),
                            Vec2::new(cw, CARD_H),
                        );
                        card(ui, r, p, &v, skin, palette, st, motion, &mut actions);
                    }
                }
            }
            section_rule(ui, left, width, "Buses", skin);
            bus_tray(ui, left, cw, skin, st, &mut actions);
            ui.add_space(24.0);
        });
    });
    if let Some(pos) = st.picker {
        picker(ui.ctx(), pos, devices, &state.positions, skin, st, motion, &mut actions);
    }
    actions
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

fn group_header(ui: &mut egui::Ui, left: f32, width: f32, gv: &GroupView, skin: &GearSkin, st: &mut ScreenState) {
    let r = section_rule(ui, left, width, row_title(gv.group), skin);
    let expanded = st.expanded.contains(&gv.group);
    if gv.hidden == 0 && !expanded {
        return;
    }
    let title = row_title(gv.group);
    let (label, accessible) = if expanded {
        ("Show fewer".to_string(), format!("Show fewer {title} positions"))
    } else {
        (format!("+{} more", gv.hidden), format!("Show all {title} positions"))
    };
    let pill_w = 90.0;
    let at =
        Rect::from_min_size(Pos2::new(left + width - pill_w, r.center().y - 8.0), Vec2::new(pill_w, paint::PILL_H));
    let mut child =
        ui.new_child(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::right_to_left(egui::Align::Center)));
    if paint::pill_labeled(&mut child, &label, &accessible, skin).clicked() {
        if expanded {
            st.expanded.remove(&gv.group);
        } else {
            st.expanded.insert(gv.group);
        }
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
    actions: &mut Vec<ScreenAction>,
) {
    let colour = device_color(p, palette);
    let filling = st.filling.contains(&p.pos);
    let f = face(p, v, filling);
    let id = Id::new(("position-card", p.pos.to_string()));
    let empty = p.status == PositionStatus::Empty;
    let off = p.status == PositionStatus::Off;
    let resp = ui.interact(r, id, Sense::click());
    let enabled = ui.is_enabled();
    let label = card_label(&f, empty);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &label));
    let painter = ui.painter_at(r.expand(40.0));
    if empty {
        // A recess where a device would go.
        paint::recess(&painter, r.shrink(2.0), base, 12);
        let hover = motion.spring(id.with("hover"), if resp.hovered() { 1.0 } else { 0.0 }, SETTLE);
        let ink = base.ground_ink;
        paint::etched_text(
            &painter,
            r.center() - Vec2::new(0.0, 10.0),
            Align2::CENTER_CENTER,
            &f.tag,
            base,
            ink,
            11.0,
            true,
            0.14,
            0.72 + 0.28 * hover,
        );
        paint::etched_text(
            &painter,
            r.center() + Vec2::new(0.0, 10.0),
            Align2::CENTER_CENTER,
            "click to choose a device",
            base,
            ink,
            12.0,
            false,
            0.0,
            0.72 + 0.28 * hover,
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
    let lift = motion.spring(id.with("lift"), if resp.hovered() && !off { 1.0 } else { 0.0 }, SETTLE);
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
    // Row 1: the LED, the position tag, a master tag, the colour dot.
    let led_at = r.min + Vec2::new(18.0, 22.0);
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
    let tag_rect = paint::tag(&painter, led_at + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, &f.tag, &skin, 0.78);
    if p.master {
        paint::etched_text(
            &painter,
            Pos2::new(tag_rect.right() + 8.0, led_at.y),
            Align2::LEFT_CENTER,
            "MASTER",
            &skin,
            if skin.mould { skin.ink } else { skin.accent },
            10.5,
            true,
            0.12,
            1.0,
        );
    }
    if !skin.mould && !off {
        let dot = r.min + Vec2::new(w - 18.0, 22.0);
        painter.circle_filled(dot + Vec2::new(0.0, 1.0), 5.0, Color32::from_black_alpha(90));
        painter.circle_filled(dot, 4.5, colour);
        painter.circle_filled(dot - Vec2::new(1.3, 1.5), 1.4, Color32::from_white_alpha(120));
    }
    // Row 2: the device name and its sub-line.
    let ctx = ui.ctx().clone();
    paint::truncated(
        &painter,
        r.min + Vec2::new(16.0, 34.0),
        Align2::LEFT_TOP,
        &f.name,
        paint::font(&ctx, "label-bold", 15.0),
        skin.ink,
        w - 32.0,
    );
    if !f.sub.is_empty() {
        paint::truncated(
            &painter,
            r.min + Vec2::new(16.0, 54.0),
            Align2::LEFT_TOP,
            &f.sub,
            paint::font(&ctx, "label", 11.0),
            paint::alpha(skin.ink, 0.62),
            w - 32.0,
        );
    }
    // The OLED.
    let oled = Rect::from_min_size(r.min + Vec2::new(14.0, 72.0), Vec2::new(w - 28.0, 44.0));
    if off {
        paint::oled_well(&painter, oled, &skin);
        paint::oled_text(
            &painter,
            Pos2::new(oled.left() + 10.0, oled.center().y),
            "OFF",
            paint::OLED_L,
            paint::alpha(skin.oled, 0.25),
        );
    } else {
        let oled_colour =
            if p.device.as_ref().is_some_and(|d| matches!(d.kind, DeviceKind::NetSend | DeviceKind::NetReceive)) {
                skins::OLED_CYAN
            } else {
                skin.oled
            };
        let dim = motion.phosphor(id.with("oled"), if f.led == Some(RED) { 0.45 } else { 1.0 }, PHOSPHOR_TAU);
        paint::oled(&painter, oled, &skin, &f.line1, &f.line2, paint::alpha(oled_colour, dim));
    }
    // The meters.
    let tray = Rect::from_min_size(r.min + Vec2::new(14.0, 126.0), Vec2::new(w - 28.0, 26.0));
    paint::tray(&painter, tray, &skin);
    let lv = levels(p, v);
    if !lv.is_empty() && !off {
        let inner = tray.shrink2(Vec2::new(5.0, 4.0));
        let n = lv.len();
        if n <= 4 {
            let tag_w = 24.0;
            let h = ((inner.height() - (n as f32 - 1.0) * 2.0) / n as f32).max(2.0);
            for (k, (input, ch, peak)) in lv.iter().enumerate() {
                let (level, hold) = motion.ppm(id.with(("ppm", *input, *ch)), *peak);
                let y = inner.top() + k as f32 * (h + 2.0);
                let bar = Rect::from_min_size(Pos2::new(inner.left() + tag_w, y), Vec2::new(inner.width() - tag_w, h));
                if k == 0 || lv[k - 1].0 != *input {
                    paint::etched_text(
                        &painter,
                        Pos2::new(inner.left(), y + h / 2.0),
                        Align2::LEFT_CENTER,
                        if *input { "IN" } else { "OUT" },
                        &skin,
                        skin.ink,
                        8.5,
                        true,
                        0.1,
                        0.6,
                    );
                }
                paint::meter(&painter, bar, level, hold, false);
            }
        } else {
            let bw = inner.width() / n as f32;
            for (k, (input, ch, peak)) in lv.iter().enumerate() {
                let (level, hold) = motion.ppm(id.with(("ppm", *input, *ch)), *peak);
                let bar = Rect::from_min_size(
                    inner.min + Vec2::new(k as f32 * bw, 0.0),
                    Vec2::new((bw - 1.0).max(1.0), inner.height()),
                );
                paint::meter(&painter, bar, level, hold, true);
            }
        }
    }
    // The controls.
    let controls = Rect::from_min_size(r.min + Vec2::new(12.0, 160.0), Vec2::new(w - 24.0, paint::PILL_H));
    let mut row = ui
        .new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::left_to_right(egui::Align::Center)));
    row.spacing_mut().item_spacing.x = 6.0;
    let label = p.pos.label();
    let virt = p.pos.group.is_virtual();
    if let Some(c) = st.confirm.filter(|c| c.pos == p.pos) {
        confirm_strip(&mut row, c, &skin, st, actions);
    } else if off {
        if paint::pill_labeled(&mut row, "Turn on", &format!("Turn on {label}"), &skin).clicked() {
            st.just_filled.insert(p.pos);
            actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: None }));
        }
    } else {
        if virt {
            if paint::pill_labeled(&mut row, "Turn off", &format!("Turn off {label}"), &skin).clicked() {
                st.confirm = Some(Confirm { pos: p.pos, clear: false, at: now });
            }
            if p.pos.group == PosGroup::Vasio {
                shape_menu(&mut row, p, &skin, actions);
            }
        } else {
            if p.pos.group == PosGroup::Asio
                && !p.master
                && paint::pill_labeled(&mut row, "Master", &format!("Make {label} master"), &skin).clicked()
            {
                actions.push(ScreenAction::Edit(Edit::SetMaster { pos: Some(p.pos) }));
            }
            if paint::pill_labeled(&mut row, "Clear\u{2026}", &format!("Clear {label}"), &skin).clicked() {
                st.confirm = Some(Confirm { pos: p.pos, clear: true, at: now });
            }
        }
        if let Some(&slot) = p.slots.first() {
            colour_menu(&mut row, slot, &label, colour, palette, &skin, actions);
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

/// Where a popover of `size` anchored to `anchor` goes: under it, or above
/// when there is no room, kept inside `screen`.
pub fn popover_pos(anchor: Rect, size: Vec2, screen: Rect) -> Pos2 {
    let gap = 8.0;
    let mut x = anchor.left();
    let mut y = anchor.bottom() + gap;
    if y + size.y > screen.bottom() {
        y = (anchor.top() - gap - size.y).max(screen.top());
    }
    if x + size.x > screen.right() {
        x = (screen.right() - size.x).max(screen.left());
    }
    Pos2::new(x, y)
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
    let width = 300.0;
    let id = Id::new("device-picker");
    let screen = ctx.content_rect();
    let fade = motion.tween_from(id.with("fade"), 0.0, 1.0, Curve::Enter, ENTER * 0.65);
    // Sized on its first frame; placed from then on.
    let last_size = ctx.data(|d| d.get_temp::<Vec2>(id.with("size"))).unwrap_or(Vec2::new(width, 200.0));
    let at = popover_pos(st.picker_anchor, last_size, screen) + Vec2::new(0.0, 6.0 * (1.0 - fade));
    let area = egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(at).constrain_to(screen).show(ctx, |ui| {
        ui.set_opacity(fade);
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
                        ui.add(TextEdit::singleline(&mut st.net_stream).hint_text("stream name").desired_width(120.0));
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
                            chosen = Some((DeviceKind::NetSend, format!("{}/{stream}:{}", d.name, st.net_channels)));
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
                        let b =
                            ui.add_enabled_ui(ok, |ui| paint::pill_labeled(ui, "Send here", "Send here", skin)).inner;
                        if b.clicked() && ok {
                            chosen = Some((DeviceKind::NetSend, format!("{to}/{stream}:{}", st.net_channels)));
                        }
                    });
                }
                g => {
                    let all: Vec<&DeviceInfo> = devices.iter().filter(|d| kinds(g).contains(&d.kind)).collect();
                    if all.len() > 6 {
                        let search = ui
                            .add(TextEdit::singleline(&mut st.search).hint_text("Search").desired_width(f32::INFINITY));
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
    });
    let rect = area.inner;
    ctx.data_mut(|d| d.insert_temp(id.with("size"), rect.size()));
    // Esc, or a click outside, closes it; the click that opened it does not.
    let (esc, clicked_out) = ctx.input(|i| {
        let out = i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|q| !rect.contains(q));
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

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{all_positions, PositionDevice, PositionState, PositionStatus};
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
    fn all() -> Vec<PositionState> {
        let mut ps: Vec<PositionState> =
            all_positions().into_iter().map(|p| st(&p.to_string(), PositionStatus::Empty)).collect();
        for p in ps.iter_mut().filter(|p| p.pos.group.is_virtual()) {
            p.status = PositionStatus::Off;
        }
        ps
    }
    #[test]
    fn rows_come_in_group_order_with_off_vasios_last() {
        let mut ps = all();
        ps.iter_mut().find(|p| p.pos.to_string() == "vasio:C").unwrap().status = PositionStatus::On { online: false };
        let rows = card_rows(&ps);
        let groups: Vec<_> = rows.iter().map(|(g, _)| *g).collect();
        assert_eq!(
            groups,
            vec![
                PosGroup::Vasio,
                PosGroup::Asio,
                PosGroup::WinIn,
                PosGroup::WinOut,
                PosGroup::App,
                PosGroup::NetIn,
                PosGroup::NetOut
            ]
        );
        let virt: Vec<String> = rows[0].1.iter().map(|p| p.pos.to_string()).collect();
        assert_eq!(virt[0], "vasio:C", "on first");
        assert!(virt.contains(&"vaio:A".to_string()));
    }

    #[test]
    fn a_collapsed_group_shows_its_devices_and_the_next_free_position() {
        let mut ps = all();
        ps.iter_mut().find(|p| p.pos.to_string() == "vasio:A").unwrap().status = PositionStatus::On { online: true };
        ps.iter_mut().find(|p| p.pos.to_string() == "asio:3").unwrap().status = PositionStatus::Filled { online: true };
        let views = group_views(&ps, &HashSet::new());
        let asio = views.iter().find(|g| g.group == PosGroup::Asio).unwrap();
        let shown: Vec<String> = asio.cards.iter().map(|p| p.pos.to_string()).collect();
        assert_eq!(shown, vec!["asio:3", "asio:1"], "the device, then the first empty tray");
        assert_eq!(asio.hidden, 6);
        let virt = &views[0];
        let shown: Vec<String> = virt.cards.iter().map(|p| p.pos.to_string()).collect();
        assert_eq!(shown, vec!["vasio:A", "vasio:B"], "the on one, then the next off one");
        assert_eq!(virt.hidden, 7, "six VASIOs and the VAIO");
        let mut expanded = HashSet::new();
        expanded.insert(PosGroup::Asio);
        let views = group_views(&ps, &expanded);
        let asio = views.iter().find(|g| g.group == PosGroup::Asio).unwrap();
        assert_eq!(asio.cards.len(), 8);
        assert_eq!(asio.hidden, 0);
    }

    #[test]
    fn cards_fill_the_width() {
        let (n, w) = cards_across(100.0);
        assert_eq!((n, w), (1, CARD_W_MIN));
        let (n, w) = cards_across(CARD_W_MIN * 3.0 + GAP * 2.0 + 30.0);
        assert_eq!(n, 3);
        assert!(w > CARD_W_MIN && w <= CARD_W_MAX, "{w}");
        let (n, w) = cards_across(CARD_W_MAX * 2.0 + GAP + 100.0);
        assert_eq!((n, w), (2, CARD_W_MAX), "never wider than the cap");
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
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0));
        let tray = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(240.0, 190.0));
        let at = popover_pos(tray, Vec2::new(300.0, 200.0), screen);
        assert_eq!(at, Pos2::new(100.0, 298.0));
        let low = Rect::from_min_size(Pos2::new(800.0, 450.0), Vec2::new(240.0, 190.0));
        let at = popover_pos(low, Vec2::new(300.0, 200.0), screen);
        assert_eq!(at, Pos2::new(700.0, 242.0), "above the tray, pulled in from the right edge");
    }

    #[test]
    fn a_devices_colour_defaults_from_its_lowest_slot_like_the_matrix_band() {
        let palette = skins::DEVICE_PALETTE.to_vec();
        let mut p = st("asio:1", PositionStatus::Filled { online: true });
        p.slots = vec![9, 10];
        assert_eq!(device_color(&p, &palette), palette[9 % 8]);
        p.color = Some([1, 2, 3]);
        assert_eq!(device_color(&p, &palette), Color32::from_rgb(1, 2, 3));
    }
}
