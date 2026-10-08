//! The Devices screen: every fixed position as a hardware-like card, grouped
//! in rows (spec: slot model §5). Empty cards open a picker; filled ones
//! show the device, its health on an OLED and its levels; Ctrl+click swaps.

use std::collections::{HashMap, HashSet};

use confluence_api::{
    byte_db, DeviceInfo, DeviceKind, MeterFrame, PosGroup, PosId, PositionState, PositionStatus, SlotHealth, SlotState,
};
use eframe::egui::{self, Color32, Id, Pos2, Rect, RichText, Sense, TextEdit, Vec2, WidgetInfo, WidgetType};

use crate::commands::Edit;
use crate::gear::paint;
use crate::gear::skins::GearSkin;

/// What the screen asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenAction {
    Edit(Edit),
    /// Show this slot in the inspector.
    Select(u32),
    /// Ask before emptying this position.
    AskClear(PosId),
    /// Ask before switching this virtual position off (its routes go).
    AskTurnOff(PosId),
}

pub struct ScreenState {
    /// The position whose picker is open.
    pub picker: Option<PosId>,
    /// The picker replaces the device there (Ctrl+click on a filled card).
    pub swap: bool,
    /// Peak hold per (input?, channel): level in dB and when it was set.
    pub hold: HashMap<(bool, u32), (f32, f64)>,
    /// Positions being filled (the engine is opening the device).
    pub filling: HashSet<PosId>,
    pub adding_bus: bool,
    pub bus_name: String,
    pub bus_channels: u32,
    pub app_name: String,
    pub net_stream: String,
    pub net_channels: u32,
    pub net_address: String,
}

impl Default for ScreenState {
    fn default() -> Self {
        ScreenState {
            picker: None,
            swap: false,
            hold: HashMap::new(),
            filling: HashSet::new(),
            adding_bus: false,
            bus_name: String::new(),
            bus_channels: 2,
            app_name: String::new(),
            net_stream: "Main".into(),
            net_channels: 2,
            net_address: String::new(),
        }
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

fn row_title(g: PosGroup) -> &'static str {
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
    match g {
        PosGroup::Asio => &[DeviceKind::Asio],
        PosGroup::WinIn => &[DeviceKind::WasapiCapture],
        PosGroup::WinOut => &[DeviceKind::WasapiRender],
        PosGroup::App => &[DeviceKind::AppCapture],
        PosGroup::NetIn => &[DeviceKind::NetReceive],
        PosGroup::NetOut => &[DeviceKind::NetSend],
        PosGroup::Vasio => &[DeviceKind::Vasio],
        PosGroup::Vaio => &[DeviceKind::Vaio],
    }
}

/// VASIO shapes per direction (as the engine offers them).
const SHAPES: [u32; 5] = [2, 4, 8, 16, 32];

const CARD: Vec2 = Vec2::new(240.0, 172.0);
const GAP: f32 = 14.0;

/// How many cards fit across `width`.
fn per_line(width: f32) -> usize {
    (((width + GAP) / (CARD.x + GAP)).floor() as usize).max(1)
}

/// What one card shows: the header, the OLED's two lines and its LED.
struct Face {
    header: String,
    line1: String,
    line2: String,
    led: Option<Color32>,
}

const GREEN: Color32 = Color32::from_rgb(0x4c, 0xd9, 0x64);
const AMBER: Color32 = Color32::from_rgb(0xff, 0xb0, 0x3a);
const RED: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);

fn face(p: &PositionState, view: &Views, filling: bool) -> Face {
    let label = p.pos.label();
    let device = p.device.as_ref().map(|d| d.name.clone()).unwrap_or_default();
    let health = p.slots.iter().find_map(|id| view.health.iter().find(|h| h.id == *id));
    let shape = |s: Option<(u32, u32)>| s.map(|(i, o)| format!("{o}\u{d7}{i}")).unwrap_or_default();
    if filling {
        return Face { header: label, line1: "OPENING\u{2026}".into(), line2: String::new(), led: Some(AMBER) };
    }
    match p.status {
        PositionStatus::Empty => Face {
            header: format!("{label} \u{b7} click to choose a device"),
            line1: String::new(),
            line2: String::new(),
            led: None,
        },
        PositionStatus::Off => {
            Face { header: label, line1: "\u{2014} OFF \u{2014}".into(), line2: "press to turn on".into(), led: None }
        }
        PositionStatus::On { online } if p.pos.group == PosGroup::Vasio => {
            let who = match (&p.daw, online) {
                (Some(d), _) => d.clone(),
                (None, true) => "DAW connected".into(),
                (None, false) => "No DAW".into(),
            };
            let state = if online { "ONLINE" } else { "NO DAW" };
            Face {
                header: format!("{label} \u{b7} {who}"),
                line1: format!("{state} \u{b7} {}", shape(p.shape)),
                line2: health_line(health),
                led: Some(if online { GREEN } else { AMBER }),
            }
        }
        PositionStatus::On { online } => Face {
            header: format!("{label} \u{b7} {}", if online { "app playing" } else { "no app" }),
            line1: if online { "ONLINE".into() } else { "IDLE".into() },
            line2: health_line(health),
            led: Some(if online { GREEN } else { AMBER }),
        },
        PositionStatus::Filled { online } => {
            let line1 = if !online {
                "OFFLINE".to_string()
            } else if let Some(n) = health.and_then(|h| h.net) {
                format!("PACKETS {}", n.packets)
            } else {
                view.status_line.clone()
            };
            let line2 = if p.master {
                "MASTER CLOCK".to_string()
            } else if let Some(n) = health.and_then(|h| h.net) {
                format!("LOST {} \u{b7} LATE {}", n.lost, n.late)
            } else {
                health_line(health)
            };
            Face {
                header: format!("{label} \u{b7} {device}"),
                line1,
                line2,
                led: Some(if online { GREEN } else { RED }),
            }
        }
    }
}

fn health_line(h: Option<&SlotHealth>) -> String {
    match h {
        Some(h) if h.target_frames > 0.0 => {
            format!("{:+.1} PPM \u{b7} FILL {:.0}/{:.0}", h.device_ppm, h.fill_frames, h.target_frames)
        }
        Some(h) if h.underruns + h.overruns > 0 => format!("XRUNS {}/{}", h.underruns, h.overruns),
        Some(_) => "CLOCK OK".into(),
        None => String::new(),
    }
}

/// What the cards read from the store, gathered once per frame.
struct Views<'a> {
    slots: &'a [SlotState],
    health: &'a [SlotHealth],
    meters: Option<&'a MeterFrame>,
    status_line: String,
}

/// The levels of `p`'s channels: (input?, channel, peak dB, rms dB).
fn levels(p: &PositionState, v: &Views) -> Vec<(bool, u32, f32, f32)> {
    let Some(f) = v.meters else { return Vec::new() };
    let mut out = Vec::new();
    for s in p.slots.iter().filter_map(|id| v.slots.iter().find(|s| s.id == *id)) {
        for c in s.first_input..s.first_input + s.inputs {
            if let Some(m) = c.checked_sub(f.first_input).and_then(|i| f.inputs.get(i as usize)) {
                out.push((true, c, byte_db(m[0]), byte_db(m[1])));
            }
        }
        for c in s.first_output..s.first_output + s.outputs {
            if let Some(m) = c.checked_sub(f.first_output).and_then(|i| f.outputs.get(i as usize)) {
                out.push((false, c, byte_db(m[0]), byte_db(m[1])));
            }
        }
    }
    out
}

/// Peak hold: a new higher peak, or 1.5 s without one, moves it.
fn hold(st: &mut ScreenState, key: (bool, u32), peak: f32, now: f64) -> f32 {
    let e = st.hold.entry(key).or_insert((peak, now));
    if peak >= e.0 || now - e.1 > 1.5 {
        *e = (peak, now);
    }
    e.0
}

/// The finish the screen itself (background, empty slots) is drawn in:
/// candy moulds only the devices, on a neutral silver ground.
pub fn screen_skin(base: &GearSkin) -> GearSkin {
    if base.mould {
        GearSkin::preset(crate::gear::skins::Finish::Silver)
    } else {
        *base
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
    editable: bool,
) -> Vec<ScreenAction> {
    let mut actions = Vec::new();
    let Some(state) = view.state.as_ref() else {
        ui.label("Waiting for the engine\u{2026}");
        return actions;
    };
    let status_line = view.status.as_ref().map(|s| format!("{:.0} / {}", s.sample_rate, s.block)).unwrap_or_default();
    let v = Views { slots: &state.slots, health: &view.health, meters: view.meters.as_ref(), status_line };
    if !state.notices.is_empty() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            for n in &state.notices {
                ui.label(RichText::new(n).color(AMBER));
            }
        });
    }
    let now = ui.input(|i| i.time);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            for (group, cards) in card_rows(&state.positions) {
                if cards.is_empty() {
                    continue;
                }
                ui.add_space(6.0);
                ui.label(RichText::new(row_title(group)).strong().color(screen_skin(skin).ink));
                let n = per_line(ui.available_width());
                for line in cards.chunks(n) {
                    let (row, _) =
                        ui.allocate_exact_size(Vec2::new(ui.available_width(), CARD.y + GAP), Sense::hover());
                    for (k, p) in line.iter().enumerate() {
                        let r = Rect::from_min_size(
                            row.min + Vec2::new(k as f32 * (CARD.x + GAP) + GAP / 2.0, GAP / 2.0),
                            CARD,
                        );
                        card(ui, r, p, &v, skin, palette, st, now, &mut actions);
                    }
                }
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Buses").strong().color(screen_skin(skin).ink));
            bus_card(ui, &screen_skin(skin), st, &mut actions);
        });
    });
    if let Some(pos) = st.picker {
        picker(ui.ctx(), pos, devices, &state.positions, st, &mut actions);
    }
    actions
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
    now: f64,
    actions: &mut Vec<ScreenAction>,
) {
    let colour = p.color.map(|c| Color32::from_rgb(c[0], c[1], c[2]));
    let skin = base.for_device(colour);
    let filling = st.filling.contains(&p.pos);
    let f = face(p, v, filling);
    let id = Id::new(("position-card", p.pos.to_string()));
    let resp = ui.interact(r, id, Sense::click());
    let label = [f.header.as_str(), f.line1.as_str(), f.line2.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
    let enabled = ui.is_enabled();
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &label));
    // Until the drivers are re-registered with their letters, a DAW lists the
    // old numbered name (spec: slot model §2.2).
    let resp = if p.pos.group == PosGroup::Vasio && matches!(p.status, PositionStatus::On { .. }) {
        resp.on_hover_text(format!(
            "Your DAW may list this as \"Confluence VASIO {}\" until the drivers are re-registered",
            p.pos.index + 1
        ))
    } else {
        resp
    };
    let painter = ui.painter_at(r.expand(30.0));
    if p.status == PositionStatus::Empty {
        // A recessed outline where a device would go.
        let skin = screen_skin(base);
        paint::tray(&painter, r.shrink(4.0), &skin);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            format!("{}\nclick to choose a device", p.pos.label()),
            paint::font(ui.ctx(), "label", 13.0),
            skin.ink.gamma_multiply(if resp.hovered() { 0.9 } else { 0.55 }),
        );
        if resp.clicked() {
            st.picker = Some(p.pos);
            st.swap = false;
        }
        return;
    }
    let off = p.status == PositionStatus::Off;
    paint::panel(&painter, r, &skin, None);
    if off {
        paint::dim(&painter, r, &skin);
    }
    if !skin.mould {
        if let Some(c) = colour {
            let band = Rect::from_min_size(r.min + Vec2::new(22.0, 0.0), Vec2::new(r.width() - 44.0, 4.0));
            painter.rect_filled(band, egui::CornerRadius::same(2), c);
        }
    }
    // The header: LED and label.
    let led_at = r.min + Vec2::new(18.0, 18.0);
    if let Some(c) = f.led {
        paint::led(&painter, led_at, &skin, c, true);
        let led = ui.interact(Rect::from_center_size(led_at, Vec2::splat(14.0)), id.with("led"), Sense::click());
        led.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, format!("Clear clips {}", p.pos.label())));
        if led.clicked() {
            actions.push(ScreenAction::Edit(Edit::ClearClip));
        }
    } else {
        paint::led(&painter, led_at, &skin, Color32::GRAY, false);
    }
    let header = painter
        .with_clip_rect(Rect::from_min_max(r.min + Vec2::new(30.0, 8.0), Pos2::new(r.right() - 12.0, r.top() + 28.0)));
    header.text(
        led_at + Vec2::new(14.0, 0.0),
        egui::Align2::LEFT_CENTER,
        &f.header,
        paint::font(ui.ctx(), "label", 12.0),
        skin.ink,
    );
    // The OLED.
    let oled = Rect::from_min_size(r.min + Vec2::new(14.0, 32.0), Vec2::new(r.width() - 28.0, 46.0));
    let oled_colour = if off { skin.oled.gamma_multiply(0.45) } else { skin.oled };
    paint::oled(&painter, oled, &skin, &f.line1, &f.line2, oled_colour);
    // The meters: one thin bar per channel, inputs then outputs.
    let tray = Rect::from_min_size(r.min + Vec2::new(14.0, 86.0), Vec2::new(r.width() - 28.0, 34.0));
    paint::tray(&painter, tray, &skin);
    let lv = levels(p, v);
    if !lv.is_empty() {
        let inner = tray.shrink(4.0);
        let w = (inner.width() / lv.len() as f32).min(10.0);
        for (k, (input, ch, peak, rms)) in lv.iter().enumerate() {
            let held = hold(st, (*input, *ch), *peak, now);
            let bar = Rect::from_min_size(
                inner.min + Vec2::new(k as f32 * w, 0.0),
                Vec2::new((w - 1.0).max(1.0), inner.height()),
            );
            paint::meter(&painter, bar, *peak, *rms, held);
        }
    }
    // The controls.
    let controls = Rect::from_min_size(r.min + Vec2::new(12.0, 130.0), Vec2::new(r.width() - 24.0, 30.0));
    let mut row = ui
        .new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::left_to_right(egui::Align::Center)));
    row.spacing_mut().item_spacing.x = 4.0;
    let label = p.pos.label();
    let virt = p.pos.group.is_virtual();
    if off {
        if paint::pill_labeled(&mut row, "Turn on", &format!("Turn on {label}"), &skin).clicked() {
            actions.push(ScreenAction::Edit(Edit::SetVirtual { pos: p.pos, on: true, shape: None }));
        }
    } else {
        if virt {
            if paint::pill_labeled(&mut row, "Turn off", &format!("Turn off {label}"), &skin).clicked() {
                actions.push(ScreenAction::AskTurnOff(p.pos));
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
                actions.push(ScreenAction::AskClear(p.pos));
            }
        }
        if let Some(&slot) = p.slots.first() {
            colour_menu(&mut row, slot, p.color, palette, &skin, actions);
        }
    }
    if resp.clicked() && !off {
        if ui.input(|i| i.modifiers.command) && !virt {
            st.picker = Some(p.pos);
            st.swap = true;
        } else if let Some(&slot) = p.slots.first() {
            actions.push(ScreenAction::Select(slot));
        }
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
    current: Option<confluence_api::Rgb>,
    palette: &[Color32],
    skin: &GearSkin,
    actions: &mut Vec<ScreenAction>,
) {
    let pill = paint::pill(ui, "Colour", skin);
    egui::Popup::menu(&pill).show(|ui| {
        ui.horizontal_wrapped(|ui| {
            for c in palette {
                let rgb = [c.r(), c.g(), c.b()];
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                ui.painter().rect_filled(r, egui::CornerRadius::same(4), *c);
                if current == Some(rgb) {
                    ui.painter().rect_stroke(
                        r,
                        egui::CornerRadius::same(4),
                        egui::Stroke::new(2.0, Color32::WHITE),
                        egui::StrokeKind::Outside,
                    );
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

/// Adding an insert bus.
fn bus_card(ui: &mut egui::Ui, skin: &GearSkin, st: &mut ScreenState, actions: &mut Vec<ScreenAction>) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(CARD.x * 1.4, 70.0), Sense::hover());
    let r = r.shrink2(Vec2::new(GAP / 2.0, 4.0));
    paint::panel(&ui.painter_at(r.expand(30.0)), r, skin, None);
    let mut inner = ui.new_child(
        egui::UiBuilder::new().max_rect(r.shrink(14.0)).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    inner.add(TextEdit::singleline(&mut st.bus_name).hint_text("Bus name").desired_width(110.0));
    inner.add(egui::DragValue::new(&mut st.bus_channels).range(1..=64).suffix(" ch"));
    let name = st.bus_name.trim().to_string();
    if st.adding_bus {
        inner.add(egui::Spinner::new());
    } else {
        let b = inner.add_enabled(!name.is_empty(), egui::Button::new("Add"));
        let ok = !name.is_empty();
        b.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ok, "Add insert bus"));
        if b.clicked() {
            st.adding_bus = true;
            actions.push(ScreenAction::Edit(Edit::AddBus { name, channels: st.bus_channels }));
        }
    }
}

/// Whether another position holds `d`.
fn held_elsewhere(d: &DeviceInfo, pos: PosId, positions: &[PositionState]) -> bool {
    positions.iter().any(|p| p.pos != pos && p.device.as_ref().is_some_and(|x| x.kind == d.kind && x.name == d.name))
}

fn picker(
    ctx: &egui::Context,
    pos: PosId,
    devices: &[DeviceInfo],
    positions: &[PositionState],
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let mut open = true;
    let mut chosen: Option<(DeviceKind, String)> = None;
    let title = if st.swap { format!("Swap {}", pos.label()) } else { format!("Choose a device for {}", pos.label()) };
    egui::Window::new(title).id(Id::new("device-picker")).open(&mut open).collapsible(false).show(ctx, |ui| match pos
        .group
    {
        PosGroup::App => {
            ui.add(TextEdit::singleline(&mut st.app_name).hint_text("process name or PID").desired_width(160.0));
            let name = st.app_name.trim().to_string();
            if ui.add_enabled(!name.is_empty(), egui::Button::new("Capture")).clicked() {
                chosen = Some((DeviceKind::AppCapture, name));
            }
        }
        PosGroup::NetOut => {
            ui.horizontal(|ui| {
                ui.label("Stream");
                ui.add(TextEdit::singleline(&mut st.net_stream).hint_text("stream name").desired_width(90.0));
                ui.add(egui::DragValue::new(&mut st.net_channels).range(1..=64).suffix(" ch"));
            });
            let stream = st.net_stream.trim().to_string();
            for d in devices.iter().filter(|d| d.kind == DeviceKind::NetSend) {
                let b = ui.add_enabled(!stream.is_empty(), egui::Button::new(format!("Send to {}", d.name)));
                if b.clicked() {
                    chosen = Some((DeviceKind::NetSend, format!("{}/{stream}:{}", d.name, st.net_channels)));
                }
            }
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut st.net_address).hint_text("Address (ip:port)").desired_width(140.0));
                let to = st.net_address.trim().to_string();
                if ui.add_enabled(!to.is_empty() && !stream.is_empty(), egui::Button::new("Send here")).clicked() {
                    chosen = Some((DeviceKind::NetSend, format!("{to}/{stream}:{}", st.net_channels)));
                }
            });
        }
        g => {
            let list: Vec<&DeviceInfo> = devices.iter().filter(|d| kinds(g).contains(&d.kind)).collect();
            if list.is_empty() {
                ui.label(
                    RichText::new(if g == PosGroup::NetIn {
                        "No streams arriving from other engines"
                    } else {
                        "No devices of this kind found"
                    })
                    .weak(),
                );
            }
            for d in list {
                let elsewhere = held_elsewhere(d, pos, positions);
                let text = if g == PosGroup::NetIn { format!("Receive {}", d.name) } else { d.name.clone() };
                let b = ui.add_enabled(!elsewhere, egui::Button::new(text));
                let b = if elsewhere { b.on_disabled_hover_text("in use by another position") } else { b };
                if b.clicked() {
                    chosen = Some((d.kind, d.name.clone()));
                }
            }
        }
    });
    if let Some((kind, name)) = chosen {
        st.filling.insert(pos);
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
    use confluence_api::{all_positions, PositionState, PositionStatus};
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
    fn rows_come_in_group_order_with_off_vasios_last() {
        let mut ps: Vec<PositionState> =
            all_positions().into_iter().map(|p| st(&p.to_string(), PositionStatus::Empty)).collect();
        for p in ps.iter_mut().filter(|p| p.pos.group == confluence_api::PosGroup::Vasio) {
            p.status = PositionStatus::Off;
        }
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
    fn cards_wrap_to_the_width() {
        assert_eq!(per_line(100.0), 1);
        assert_eq!(per_line(CARD.x * 3.0 + GAP * 2.0), 3);
    }
}
