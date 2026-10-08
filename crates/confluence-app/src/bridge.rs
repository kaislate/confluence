//! The meter bridge: one wide white-pixel OLED showing every channel of
//! every device, grouped by bay (spec: meter bridge §4.2). Devices and
//! single channels can be left off it; it collapses, resizes, and pops out
//! into its own window.

use confluence_api::{PositionState, PositionStatus};
use eframe::egui::{self, Align2, Id, Pos2, Rect, Sense, Vec2, WidgetInfo, WidgetType};

use crate::bays::{vacant, Bay};
use crate::devices_screen::{device_groups, split_name, Views};
use crate::gear::motion::Motion;
use crate::gear::oled_meter::{self, Geom, Group, MeterLook};
use crate::gear::skins::GearSkin;
use crate::gear::{paint, pixel_font};
use crate::prefs::BridgePrefs;

/// One device on the bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeDevice {
    pub bay: Bay,
    /// Its colour key (`pos:asio:1`): what the hidden sets hold.
    pub key: String,
    /// Its position ("ASIO 1").
    pub tag: String,
    /// Its display name.
    pub name: String,
    pub groups: Vec<Group>,
}

/// The devices on the bridge, in bay order, without the hidden ones and
/// their hidden channels.
pub fn bridge_devices(
    positions: &[PositionState],
    v: &Views,
    prefs: &BridgePrefs,
    only_custom: bool,
) -> Vec<BridgeDevice> {
    let mut out = Vec::new();
    for bay in Bay::all() {
        for &g in bay.groups() {
            let mut of: Vec<&PositionState> = positions.iter().filter(|p| p.pos.group == g && !vacant(p)).collect();
            of.sort_by_key(|p| p.pos.index);
            for p in of {
                let key = format!("pos:{}", p.pos);
                if prefs.hidden_devices.contains(&key) {
                    continue;
                }
                let base = match (&p.device, p.pos.group.is_virtual()) {
                    (_, true) => p.daw.clone().unwrap_or_else(|| p.pos.label()),
                    (Some(d), false) => split_name(d.kind, &d.name).0,
                    (None, false) => p.pos.label(),
                };
                let custom =
                    p.slots.iter().find_map(|id| v.slots.iter().find(|s| s.id == *id)).and_then(|s| s.label.clone());
                let name = crate::names::display(custom.as_deref(), &base, only_custom).0;
                let groups = device_groups(p, v)
                    .into_iter()
                    .filter_map(|mut gr| {
                        let dir = if gr.label.starts_with("IN") { "in" } else { "out" };
                        gr.channels.retain(|c| !prefs.hidden_channels.contains(&format!("{key}/{dir}/{}", c.number)));
                        if gr.channels.is_empty() {
                            return None;
                        }
                        gr.label = format!("{} {}", dir.to_uppercase(), gr.channels.len());
                        Some(gr)
                    })
                    .collect::<Vec<_>>();
                if !groups.is_empty() {
                    out.push(BridgeDevice { bay, key, tag: p.pos.label(), name, groups });
                }
            }
        }
    }
    out
}

/// The bridge's height: `frac` of the space, between 90 px and half of it.
pub fn bridge_height(frac: f32, space: f32) -> f32 {
    (frac * space).clamp(90.0, (space * 0.5).max(90.0))
}

/// What the user did on the bridge this frame.
#[derive(Default)]
pub struct BridgeResponse {
    /// Pop it out into its own window (or dock it back).
    pub toggle_popout: bool,
}

/// Space a device takes beside its meter (its name row on top).
const NAME_ROW: f32 = 14.0;
/// Points per font pixel for the device names.
const NAME_PX: f32 = 2.0;
const DEVICE_GAP: f32 = 16.0;
const CONTROLS_H: f32 = 22.0;

/// Draws the bridge in `r`. `all` lists every device (hidden ones too) for
/// the menu; `shown` the devices to meter.
#[allow(clippy::too_many_arguments)]
pub fn show_bridge(
    ui: &mut egui::Ui,
    r: Rect,
    all: &[BridgeDevice],
    shown: &[BridgeDevice],
    skin: &GearSkin,
    prefs: &mut BridgePrefs,
    look: MeterLook,
    motion: &mut Motion,
    popped: bool,
) -> BridgeResponse {
    let mut out = BridgeResponse::default();
    // The docked bridge and its window have their own ids (both draw on the frame it pops out).
    let id = Id::new(("meter-bridge", popped));
    let p = ui.painter_at(r.expand(4.0));
    paint::oled_well(&p, r, skin);
    let well = ui.interact(r, id, Sense::hover());
    well.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, "Meter bridge"));
    // The controls, top right.
    let controls = Rect::from_min_size(Pos2::new(r.right() - 330.0, r.top() + 6.0), Vec2::new(322.0, CONTROLS_H));
    let mut row = ui
        .new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::right_to_left(egui::Align::Center)));
    row.spacing_mut().item_spacing.x = 5.0;
    if !popped && paint::pill_labeled(&mut row, "Hide", "Collapse meter bridge", skin).clicked() {
        prefs.shown = false;
    }
    let (pop_text, pop_label) = if popped { ("Dock", "Dock meters") } else { ("Pop out", "Pop out meters") };
    if paint::pill_labeled(&mut row, pop_text, pop_label, skin).clicked() {
        out.toggle_popout = true;
    }
    let menu = paint::pill_labeled(&mut row, "Channels", "Bridge channels", skin);
    egui::Popup::menu(&menu).show(|ui| {
        ui.set_min_width(220.0);
        for d in all {
            let mut on = !prefs.hidden_devices.contains(&d.key);
            let label = format!("Show {} \u{b7} {}", d.tag, d.name);
            if ui.checkbox(&mut on, label).changed() {
                if on {
                    prefs.hidden_devices.remove(&d.key);
                } else {
                    prefs.hidden_devices.insert(d.key.clone());
                }
            }
            if on {
                ui.collapsing(format!("{} channels", d.tag), |ui| {
                    for g in &d.groups {
                        let dir = if g.label.starts_with("IN") { "in" } else { "out" };
                        for c in &g.channels {
                            let key = format!("{}/{dir}/{}", d.key, c.number);
                            let mut ch_on = !prefs.hidden_channels.contains(&key);
                            let text = format!("{} {} \u{b7} {}", dir.to_uppercase(), c.number, c.name);
                            if ui.checkbox(&mut ch_on, text).changed() {
                                if ch_on {
                                    prefs.hidden_channels.remove(&key);
                                } else {
                                    prefs.hidden_channels.insert(key);
                                }
                            }
                        }
                    }
                });
            }
        }
    });
    // The devices: laid out left to right, wrapping onto further lines.
    let content =
        Rect::from_min_max(Pos2::new(r.left() + 12.0, r.top() + 8.0), Pos2::new(r.right() - 12.0, r.bottom() - 8.0));
    let widths: Vec<f32> = shown
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let g = Geom { scale: false, ..Geom::bridge() };
            let w = oled_meter::meter_layout(&d.groups, Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0e6, 60.0)), &g)
                .width_used;
            w.max(pixel_font::width(&d.name.to_uppercase(), NAME_PX)) + if i == 0 { 16.0 } else { 0.0 }
        })
        .collect();
    let mut lines: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = 16.0; // the first line's scale
    for (i, w) in widths.iter().enumerate() {
        let need = if lines.last().is_some_and(|l| l.is_empty()) { *w } else { DEVICE_GAP + w };
        if used + need > content.width() && lines.last().is_some_and(|l| !l.is_empty()) {
            lines.push(Vec::new());
            used = 16.0;
        }
        used += if lines.last().is_some_and(|l| l.is_empty()) { *w } else { DEVICE_GAP + w };
        if let Some(l) = lines.last_mut() {
            l.push(i);
        }
    }
    let n = lines.len().max(1) as f32;
    let line_h = ((content.height() - (n - 1.0) * 8.0) / n).max(30.0);
    let mut names = egui::epaint::Mesh::default();
    for (li, line) in lines.iter().enumerate() {
        let top = content.top() + li as f32 * (line_h + 8.0);
        let mut x = content.left();
        for (k, &i) in line.iter().enumerate() {
            let d = &shown[i];
            let geom = Geom { scale: k == 0, ..Geom::bridge() };
            let scale_w = if k == 0 { 16.0 } else { 0.0 };
            let meter_w = widths[i] - if i == 0 { 16.0 } else { 0.0 } + scale_w;
            let dev = Rect::from_min_size(Pos2::new(x, top), Vec2::new(meter_w.max(20.0), line_h));
            pixel_font::draw(
                &mut names,
                Pos2::new(x + scale_w, top),
                &d.name.to_uppercase(),
                NAME_PX,
                d.bay.color(),
                false,
            );
            let mrect = Rect::from_min_max(Pos2::new(dev.left(), top + NAME_ROW), dev.max);
            let m = oled_meter::meter_widget(ui, id.with(&d.key), mrect, &d.groups, &geom, look, motion);
            // "Bridge: VASIO A, Ableton" (unlike the card's "VASIO A · Ableton").
            let what = format!("Bridge: {}, {}", d.tag, d.name);
            m.response.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, &what));
            x += meter_w + DEVICE_GAP;
        }
    }
    p.add(egui::Shape::mesh(names));
    if shown.is_empty() {
        paint::oled_text(
            &p,
            Pos2::new(content.left() + 6.0, content.center().y),
            "NO METERS SHOWN",
            14.0,
            paint::alpha(skin.oled, 0.5),
        );
    }
    // The bottom edge drags to resize.
    if !popped {
        let handle = Rect::from_min_max(Pos2::new(r.left(), r.bottom() - 4.0), Pos2::new(r.right(), r.bottom() + 4.0));
        let drag =
            ui.interact(handle, id.with("resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if drag.dragged() {
            let space = ui.ctx().content_rect().height().max(1.0);
            prefs.height_frac = (prefs.height_frac + drag.drag_delta().y / space).clamp(0.05, 0.5);
        }
    }
    let _ = Align2::LEFT_TOP;
    out
}

/// The collapsed bridge: a thin strip with a pill to bring it back.
pub fn collapsed_strip(ui: &mut egui::Ui, r: Rect, skin: &GearSkin, prefs: &mut BridgePrefs) {
    paint::oled_well(&ui.painter_at(r.expand(4.0)), r, skin);
    let at = Rect::from_min_size(Pos2::new(r.left() + 8.0, r.top() + 1.0), Vec2::new(200.0, r.height() - 2.0));
    let mut row =
        ui.new_child(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::left_to_right(egui::Align::Center)));
    if paint::pill_labeled(&mut row, "Show meters", "Show meter bridge", skin).clicked() {
        prefs.shown = true;
    }
}

/// The pop-out window: its title, its last place and size, and whether it
/// stays on top.
pub fn viewport_builder(prefs: &BridgePrefs) -> egui::ViewportBuilder {
    let level = if prefs.pinned { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
    let b = egui::ViewportBuilder::default()
        .with_title("Confluence meters")
        .with_min_inner_size([360.0, 140.0])
        .with_window_level(level);
    match prefs.window {
        Some([x, y, w, h]) => b.with_position([x, y]).with_inner_size([w, h]),
        None => b.with_inner_size([960.0, 280.0]),
    }
}

/// The bridge in its own window (an embedded one where the platform cannot
/// open windows): a pin to keep it on top, and Dock to put it back.
pub fn popout(
    ctx: &egui::Context,
    view: &confluence_client::StoreView,
    skin: &GearSkin,
    prefs: &mut crate::prefs::ViewPrefs,
    motion: &mut Motion,
) {
    let Some(state) = view.state.as_ref() else { return };
    let v = Views {
        slots: &state.slots,
        health: &view.health,
        meters: view.meters.as_ref(),
        sample_rate: view.status.as_ref().map(|s| s.sample_rate),
    };
    let only = prefs.only_custom_names;
    let all = bridge_devices(&state.positions, &v, &BridgePrefs::default(), only);
    let shown = bridge_devices(&state.positions, &v, &prefs.bridge, only);
    let look = prefs.meter;
    let id = egui::ViewportId::from_hash_of("confluence-meters");
    ctx.show_viewport_immediate(id, viewport_builder(&prefs.bridge), |ui, class| {
        let area = ui.max_rect();
        paint::ground(ui.painter(), area, skin);
        // The pin, top left.
        let pin_at = Rect::from_min_size(area.min + Vec2::new(10.0, 8.0), Vec2::new(140.0, paint::PILL_H));
        let mut row = ui.new_child(
            egui::UiBuilder::new().max_rect(pin_at).layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let pinned = prefs.bridge.pinned;
        if paint::pill_lit(&mut row, "Pin", "Keep meters on top", pinned, skin).clicked() {
            prefs.bridge.pinned = !pinned;
            let level = if prefs.bridge.pinned { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        }
        let r = Rect::from_min_max(area.min + Vec2::new(8.0, 12.0 + paint::PILL_H), area.max - Vec2::splat(8.0));
        let resp = show_bridge(ui, r, &all, &shown, skin, &mut prefs.bridge, look, motion, true);
        let closing =
            class != egui::ViewportClass::EmbeddedWindow && ui.ctx().input(|i| i.viewport().close_requested());
        if resp.toggle_popout || closing {
            prefs.bridge.popped = false;
        }
        if class != egui::ViewportClass::EmbeddedWindow {
            let (outer, inner) = ui.ctx().input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
            if let (Some(o), Some(i)) = (outer, inner) {
                prefs.bridge.window = Some([o.min.x, o.min.y, i.width(), i.height()]);
            }
        }
    });
}

/// True if `p` is a position the bridge can show (it holds a device).
pub fn on_bridge(p: &PositionState) -> bool {
    !matches!(p.status, PositionStatus::Empty | PositionStatus::Off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{ClockRole, DeviceKind, PositionDevice, PositionState, PositionStatus, SlotState};

    fn slot(id: u32, name: &str, inputs: u32, outputs: u32) -> SlotState {
        SlotState {
            id,
            name: name.into(),
            device: String::new(),
            role: ClockRole::Soft,
            online: true,
            first_input: id * 10,
            inputs,
            first_output: id * 10,
            outputs,
            color: None,
            input_names: Vec::new(),
            output_names: Vec::new(),
            label: None,
            input_labels: Vec::new(),
            output_labels: Vec::new(),
        }
    }

    fn pos(p: &str, kind: DeviceKind, name: &str, slots: Vec<u32>) -> PositionState {
        PositionState {
            pos: p.parse().unwrap(),
            status: PositionStatus::Filled { online: true },
            device: Some(PositionDevice { kind, name: name.into() }),
            shape: None,
            daw: None,
            master: false,
            color: None,
            slots,
        }
    }

    #[test]
    fn devices_come_in_bay_order_and_hidden_ones_drop_out() {
        let slots = vec![slot(1, "Game", 0, 2), slot(2, "GoXLR in", 4, 0), slot(3, "VASIO 1", 2, 2)];
        let mut vasio = pos("vasio:A", DeviceKind::Vasio, "1:2x2", vec![3]);
        vasio.status = PositionStatus::On { online: true };
        let positions = vec![
            pos("win-out:1", DeviceKind::WasapiRender, "Game (4- TC-HELICON GoXLR)", vec![1]),
            vasio,
            pos("asio:1", DeviceKind::Asio, "GoXLR", vec![2]),
            PositionState {
                status: PositionStatus::Empty,
                device: None,
                slots: vec![],
                ..pos("asio:2", DeviceKind::Asio, "", vec![])
            },
        ];
        let v = Views { slots: &slots, health: &[], meters: None, sample_rate: None };
        let mut prefs = BridgePrefs::default();
        let all = bridge_devices(&positions, &v, &prefs, false);
        let order: Vec<(Bay, &str)> = all.iter().map(|d| (d.bay, d.name.as_str())).collect();
        assert_eq!(order, vec![(Bay::Hardware, "GoXLR"), (Bay::Windows, "Game"), (Bay::Virtual, "VASIO A")]);
        assert_eq!(all[0].groups[0].channels.len(), 4);
        // Hide a device, and one channel of another.
        prefs.hidden_devices.insert("pos:asio:1".into());
        prefs.hidden_channels.insert("pos:win-out:1/out/2".into());
        let shown = bridge_devices(&positions, &v, &prefs, false);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].name, "Game");
        assert_eq!(shown[0].groups[0].channels.len(), 1);
        assert_eq!(shown[0].groups[0].channels[0].number, 1, "numbers stay the device's");
    }

    #[test]
    fn a_custom_name_names_the_device_on_the_bridge() {
        let mut s = slot(2, "GoXLR in", 2, 0);
        s.label = Some("Desk".into());
        let slots = vec![s];
        let positions = vec![pos("asio:1", DeviceKind::Asio, "GoXLR", vec![2])];
        let v = Views { slots: &slots, health: &[], meters: None, sample_rate: None };
        let d = bridge_devices(&positions, &v, &BridgePrefs::default(), true);
        assert_eq!(d[0].name, "Desk");
    }

    #[test]
    fn the_pop_out_window_is_titled_remembers_its_place_and_can_stay_on_top() {
        let mut prefs = BridgePrefs::default();
        let b = viewport_builder(&prefs);
        assert_eq!(b.title.as_deref(), Some("Confluence meters"));
        assert_eq!(b.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(b.inner_size, Some(Vec2::new(960.0, 280.0)));
        prefs.pinned = true;
        prefs.window = Some([100.0, 50.0, 700.0, 240.0]);
        let b = viewport_builder(&prefs);
        assert_eq!(b.window_level, Some(egui::WindowLevel::AlwaysOnTop));
        assert_eq!(b.position, Some(Pos2::new(100.0, 50.0)));
        assert_eq!(b.inner_size, Some(Vec2::new(700.0, 240.0)));
    }

    #[test]
    fn the_bridge_is_between_90_px_and_half_the_screen() {
        assert_eq!(bridge_height(0.22, 1000.0), 220.0);
        assert_eq!(bridge_height(0.01, 1000.0), 90.0);
        assert_eq!(bridge_height(0.9, 1000.0), 500.0);
    }
}
