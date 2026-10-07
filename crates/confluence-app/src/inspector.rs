//! The inspector: the selected route, the selected slot (with clock health),
//! or a summary when nothing is selected.

use confluence_api::{ClockRole, PluginStatus, PointState, SlotState, State};
use std::collections::VecDeque;

use confluence_client::{HealthSample, StoreView, HISTORY_LEN};
use eframe::egui::{self, Button, Color32, DragValue, RichText, Slider};

use crate::commands::Edit;
use crate::graph::{plot, Series};
use crate::matrix::{point_label, Selection};
use crate::skin::Look;
use crate::theme::{fader_db, fader_pos, GAIN_MAX_DB, GAIN_MIN_DB};

pub enum Action {
    Edit(Edit),
    /// Ask before removing this slot.
    RemoveSlot(u32),
    /// Open the plugin picker for this bus.
    PickPlugin(u32),
}

/// What the inspector needs to show plugins between engine updates.
pub struct PluginUi<'a> {
    /// Parameter values sent but not yet confirmed: (bus, param) → value.
    pub pending: &'a std::collections::HashMap<(u32, u32), f64>,
    /// A bus whose plugin is being loaded.
    pub loading: Option<u32>,
}

/// Routes with an end on one of the slot's channels.
pub fn routes_of(state: &State, slot: &SlotState) -> usize {
    let ins = slot.first_input..slot.first_input + slot.inputs;
    let outs = slot.first_output..slot.first_output + slot.outputs;
    state.points.iter().filter(|p| ins.contains(&p.input) || outs.contains(&p.output)).count()
}

fn range_text(first: u32, n: u32) -> String {
    match n {
        0 => "none".into(),
        1 => format!("{}", first + 1),
        _ => format!("{}–{}", first + 1, first + n),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    view: &StoreView,
    look: &Look,
    selection: &Selection,
    point: Option<PointState>,
    history: Option<&VecDeque<Option<HealthSample>>>,
    editable: bool,
    plugin_ui: &PluginUi,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(state) = &view.state else {
        ui.label("Waiting for the engine…");
        return actions;
    };
    ui.add_enabled_ui(editable, |ui| match *selection {
        Selection::None => summary(ui, look, state),
        Selection::Cell { input, output } => point_panel(ui, state, input, output, point, &mut actions),
        Selection::Slot(id) => slot_panel(ui, look, view, history, state, id, plugin_ui, &mut actions),
    });
    actions
}

fn summary(ui: &mut egui::Ui, look: &Look, state: &State) {
    ui.heading("Engine");
    ui.label(format!("{} slots · {} routes", state.slots.len(), state.points.len()));
    for n in &state.notices {
        ui.label(RichText::new(n).color(look.skin.colors.warn));
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Click a cell or a slot header to inspect it.").weak());
}

fn point_panel(
    ui: &mut egui::Ui,
    state: &State,
    input: u32,
    output: u32,
    point: Option<PointState>,
    actions: &mut Vec<Action>,
) {
    ui.heading(point_label(&state.slots, input, output));
    let set =
        |gain_db: f32, mute: bool, invert: bool| Action::Edit(Edit::SetPoint { input, output, gain_db, mute, invert });
    match point {
        Some(p) => {
            // The slider moves a fader position (fine steps near 0 dB, see
            // `fader_db`); the number field shows and edits the real gain, so
            // the slider's range never overwrites a gain outside it.
            let mut pos = fader_pos(p.gain_db);
            let mut typed = p.gain_db;
            let slider = ui.add(Slider::new(&mut pos, 0.0..=1.0).text("Gain (dB)").show_value(false));
            let field = ui.add(DragValue::new(&mut typed).range(GAIN_MIN_DB..=GAIN_MAX_DB).speed(0.1).suffix(" dB"));
            let gain = if field.changed() {
                typed
            } else if slider.changed() {
                (fader_db(pos) * 10.0).round() / 10.0 // 0.1 dB steps
            } else {
                p.gain_db
            };
            if gain != p.gain_db {
                actions.push(set(gain, p.mute, p.invert));
            }
            let (mut mute, mut invert) = (p.mute, p.invert);
            if ui.checkbox(&mut mute, "Mute").changed() {
                actions.push(set(p.gain_db, mute, p.invert));
            }
            if ui.checkbox(&mut invert, "Invert").changed() {
                actions.push(set(p.gain_db, p.mute, invert));
            }
            if ui.button("Remove route").clicked() {
                actions.push(Action::Edit(Edit::RemovePoint { input, output }));
            }
            midi_section(ui, state, input, output, actions);
        }
        None => {
            ui.label("No route");
            if ui.button("Route at 0 dB").clicked() {
                actions.push(set(0.0, false, false));
            }
        }
    }
}

/// The route's MIDI controls: bindings (with Forget) and MIDI Learn.
fn midi_section(ui: &mut egui::Ui, state: &State, input: u32, output: u32, actions: &mut Vec<Action>) {
    ui.separator();
    ui.label(RichText::new("MIDI").strong());
    for b in state.midi_bindings.iter().filter(|b| (b.input, b.output) == (input, output)) {
        ui.horizontal(|ui| {
            ui.label(format!("CC {} · ch {} · {}", b.cc, b.channel, b.device));
            if ui.small_button("Forget").clicked() {
                actions.push(Action::Edit(Edit::RemoveMidiBinding {
                    device: b.device.clone(),
                    channel: b.channel,
                    cc: b.cc,
                }));
            }
        });
    }
    if state.midi_learning == Some((input, output)) {
        ui.label(RichText::new("Move a control on your MIDI device…").italics());
        if ui.button("Cancel").clicked() {
            actions.push(Action::Edit(Edit::CancelMidiLearn));
        }
    } else if ui.button("MIDI Learn").clicked() {
        actions.push(Action::Edit(Edit::LearnMidi { input, output }));
    }
    if state.midi_inputs.is_empty() {
        ui.label(RichText::new("No MIDI inputs found").weak());
    }
}

#[allow(clippy::too_many_arguments)]
fn slot_panel(
    ui: &mut egui::Ui,
    look: &Look,
    view: &StoreView,
    history: Option<&VecDeque<Option<HealthSample>>>,
    state: &State,
    id: u32,
    plugin_ui: &PluginUi,
    actions: &mut Vec<Action>,
) {
    let c = &look.skin.colors;
    let (accent, warn, error) = (c.accent, c.warn, c.error);
    let Some(slot) = state.slots.iter().find(|s| s.id == id) else {
        ui.label("Slot removed");
        return;
    };
    ui.heading(&slot.name);
    if slot.is_bus() {
        bus_panel(ui, view, state, slot, look, plugin_ui, actions);
        return;
    }
    ui.label(format!("Device: {}", if slot.device.is_empty() { "—" } else { slot.device.as_str() }));
    ui.label(format!("Role: {:?}", slot.role));
    if slot.online {
        ui.label("Online");
    } else {
        ui.label(RichText::new("OFFLINE").color(warn).strong());
    }
    ui.label(format!(
        "in {} · out {}",
        range_text(slot.first_input, slot.inputs),
        range_text(slot.first_output, slot.outputs)
    ));
    if let Some(h) = view.health.iter().find(|h| h.id == id) {
        if h.device_lost {
            ui.label(RichText::new("Device lost: its channels are silent until it returns").color(error).strong());
        }
        if h.attached == Some(false) {
            ui.label(h.idle_note.as_deref().unwrap_or("nothing attached"));
        }
        if let Some(n) = h.net {
            ui.separator();
            ui.label(RichText::new("Network").strong());
            ui.label(format!("Packets {} · lost {} · late {} · reordered {}", n.packets, n.lost, n.late, n.reordered));
            if n.malformed > 0 {
                ui.label(format!("{} unreadable packets dropped", n.malformed));
            }
            if n.mismatched > 0 {
                let s = format!(
                    "{} packets at another sample rate or block size dropped: remove the stream and add it again",
                    n.mismatched
                );
                ui.label(RichText::new(s).color(warn));
            }
            if n.silent_ms > 1000 {
                let s = if n.silent_ms == u64::MAX {
                    "No packets yet".to_string()
                } else {
                    format!("No packets for {:.1} s", n.silent_ms as f64 / 1000.0)
                };
                ui.label(RichText::new(s).color(warn));
            }
        }
        ui.separator();
        ui.label(RichText::new("Clock health").strong());
        let samples = history;
        let bridged = samples.is_some_and(|r| r.iter().flatten().any(|s| s.target > 0.0));
        match (slot.role, bridged, samples) {
            (ClockRole::Master, false, _) => {
                ui.label("Master clock (no bridge)");
            }
            (_, true, Some(ring)) => {
                let col = |f: fn(&confluence_client::HealthSample) -> f64| {
                    ring.iter().map(|s| s.as_ref().map(f)).collect::<Vec<_>>()
                };
                plot(
                    ui,
                    "Fill and target graph",
                    80.0,
                    HISTORY_LEN,
                    &[
                        Series { name: "fill", color: accent, values: col(|s| s.fill) },
                        Series { name: "target", color: Color32::GRAY, values: col(|s| s.target) },
                    ],
                );
                plot(
                    ui,
                    "Drift and correction graph",
                    80.0,
                    HISTORY_LEN,
                    &[
                        Series { name: "device ppm", color: warn, values: col(|s| s.ppm) },
                        Series { name: "correction ppm", color: accent, values: col(|s| s.correction) },
                    ],
                );
            }
            _ => {
                ui.label("No clock bridge (runs on the engine clock)");
            }
        }
        ui.label(format!("Underruns {} · Overruns {}", h.underruns, h.overruns));
        if h.device_faults > 0 {
            ui.label(RichText::new(format!("Device faults {}", h.device_faults)).color(error));
        }
        if h.driver_requests > 0 {
            ui.label(RichText::new(format!("{} driver requests (re-add the device)", h.driver_requests)).color(warn));
        }
    }
    ui.separator();
    colour_row(ui, look, slot, actions);
    ui.separator();
    if ui.add(Button::new("Remove slot…")).clicked() {
        actions.push(Action::RemoveSlot(id));
    }
}

/// The colour of `slot`'s device (all its slots): a palette swatch, any
/// colour, or back to the default.
fn colour_row(ui: &mut egui::Ui, look: &Look, slot: &SlotState, actions: &mut Vec<Action>) {
    let id = slot.id;
    ui.label(RichText::new("Colour").strong());
    ui.horizontal_wrapped(|ui| {
        for c in &look.skin.slot_colors {
            let rgb = [c.r(), c.g(), c.b()];
            let name = format!("Colour #{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
            let chosen = slot.color == Some(rgb);
            let swatch = Button::new("").fill(*c).min_size(egui::vec2(18.0, 18.0)).selected(chosen);
            let r = ui.add(swatch).on_hover_text(&name);
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
            if r.clicked() {
                actions.push(Action::Edit(Edit::SetSlotColor { id, color: Some(rgb) }));
            }
        }
        let mut any = slot.color.unwrap_or([128, 128, 128]);
        if egui::widgets::color_picker::color_edit_button_srgb(ui, &mut any).on_hover_text("Any colour").changed() {
            actions.push(Action::Edit(Edit::SetSlotColor { id, color: Some(any) }));
        }
        if ui.add_enabled(slot.color.is_some(), Button::new("Default colour")).clicked() {
            actions.push(Action::Edit(Edit::SetSlotColor { id, color: None }));
        }
    });
}

/// An insert bus: its channels, its plugin and the plugin's parameters.
fn bus_panel(
    ui: &mut egui::Ui,
    view: &StoreView,
    state: &State,
    slot: &SlotState,
    look: &Look,
    plugin_ui: &PluginUi,
    actions: &mut Vec<Action>,
) {
    let (warn, error) = (look.skin.colors.warn, look.skin.colors.error);
    ui.label(format!("Insert bus · {} channels", slot.inputs));
    ui.label(format!(
        "sends {} · returns {}",
        range_text(slot.first_output, slot.outputs),
        range_text(slot.first_input, slot.inputs)
    ));
    ui.separator();
    let bus = slot.id;
    match state.bus_plugins.iter().find(|p| p.bus == bus) {
        _ if plugin_ui.loading == Some(bus) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label("Loading the plugin…");
            });
        }
        None => {
            ui.label("Plugin: none");
            ui.label(RichText::new("Passes audio through").weak());
            if ui.button("Load plugin…").clicked() {
                actions.push(Action::PickPlugin(bus));
            }
        }
        Some(p) => {
            ui.label(RichText::new(&p.info.name).strong());
            if !p.info.vendor.is_empty() {
                ui.label(RichText::new(&p.info.vendor).weak());
            }
            match &p.status {
                PluginStatus::Running => {}
                PluginStatus::Faulted => {
                    ui.label(RichText::new("The plugin stopped after an error — load it again").color(error));
                }
                PluginStatus::Failed(why) => {
                    ui.label(RichText::new(format!("Missing: {why}")).color(warn));
                }
            }
            if p.latency > 0 {
                ui.label(RichText::new(format!("Latency {} samples (not compensated)", p.latency)).weak());
            }
            if p.has_editor {
                ui.horizontal(|ui| {
                    if p.editor_open {
                        if ui.button("Bring editor to front").clicked() {
                            actions.push(Action::Edit(Edit::ShowEditor { bus }));
                        }
                        if ui.button("Close editor").clicked() {
                            actions.push(Action::Edit(Edit::HideEditor { bus }));
                        }
                    } else if ui.button("Show editor").clicked() {
                        actions.push(Action::Edit(Edit::ShowEditor { bus }));
                    }
                });
            } else if p.status == PluginStatus::Running {
                ui.label(RichText::new("This plugin has no editor of its own").weak());
            }
            ui.horizontal(|ui| {
                if ui.button("Replace…").clicked() {
                    actions.push(Action::PickPlugin(bus));
                }
                if ui.button("Unload").clicked() {
                    actions.push(Action::Edit(Edit::UnloadPlugin { bus }));
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("plugin-params").show(ui, |ui| {
                for q in &p.params {
                    let pending = plugin_ui.pending.get(&(bus, q.id)).copied();
                    let mut v = pending.unwrap_or(q.value);
                    ui.horizontal(|ui| {
                        let mut slider = Slider::new(&mut v, q.min..=q.max).show_value(false).text(&q.name);
                        if q.stepped {
                            slider = slider.step_by(1.0);
                        }
                        let r = ui.add_enabled(!q.read_only, slider);
                        let shown = match pending {
                            Some(x) if x != q.value => format!("{x:.2}"),
                            _ => q.text.clone(),
                        };
                        ui.label(shown);
                        if r.double_clicked() {
                            actions.push(Action::Edit(Edit::SetParam { bus, param: q.id, value: q.default }));
                        } else if r.changed() {
                            actions.push(Action::Edit(Edit::SetParam { bus, param: q.id, value: v }));
                        }
                    });
                }
            });
        }
    }
    if let Some(h) = view.health.iter().find(|h| h.id == bus) {
        if h.device_faults > 0 {
            ui.label(RichText::new(format!("Processing faults {}", h.device_faults)).color(error));
        }
    }
    ui.separator();
    colour_row(ui, look, slot, actions);
    ui.separator();
    if ui.add(Button::new("Remove slot…")).clicked() {
        actions.push(Action::RemoveSlot(bus));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{ClockRole, EngineStatus};

    fn slot(id: u32, first_input: u32, inputs: u32, first_output: u32, outputs: u32) -> SlotState {
        SlotState {
            id,
            name: format!("S{id}"),
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

    #[test]
    fn a_slots_routes_are_those_on_its_channels() {
        let a = slot(1, 0, 2, 0, 2);
        let b = slot(2, 2, 2, 2, 2);
        let p = |input, output| PointState { input, output, gain_db: 0.0, mute: false, invert: false };
        let state = State {
            version: 0,
            status: EngineStatus {
                master: "internal".into(),
                sample_rate: 48_000.0,
                block: 256,
                blocks: 0,
                dsp_load: 0.0,
                xruns: 0,
            },
            slots: vec![a.clone(), b.clone()],
            points: vec![p(0, 2), p(1, 1), p(3, 3)],
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
            scenes: Vec::new(),
            current_scene: None,
            morphing: false,
            midi_inputs: Vec::new(),
            midi_bindings: Vec::new(),
            midi_learning: None,
            scripts: Vec::new(),
            peers: Vec::new(),
            positions: Vec::new(),
        };
        assert_eq!(routes_of(&state, &a), 2, "0→2 (its input) and 1→1 (both)");
        assert_eq!(routes_of(&state, &b), 2, "0→2 (its output) and 3→3");
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use confluence_api::{ClockRole, EngineStatus};
    use confluence_client::ConnState;
    use egui_kittest::kittest::{NodeT, Queryable};
    use egui_kittest::Harness;

    fn view_with(point: PointState) -> StoreView {
        let slot = SlotState {
            id: 1,
            name: "S".into(),
            device: String::new(),
            role: ClockRole::Soft,
            online: true,
            first_input: 0,
            inputs: 2,
            first_output: 0,
            outputs: 2,
            color: None,
        };
        StoreView {
            state: Some(State {
                version: 1,
                status: EngineStatus {
                    master: "internal".into(),
                    sample_rate: 48_000.0,
                    block: 256,
                    blocks: 0,
                    dsp_load: 0.0,
                    xruns: 0,
                },
                slots: vec![slot],
                points: vec![point],
                devices: Vec::new(),
                notices: Vec::new(),
                plugins: Vec::new(),
                bad_plugins: Vec::new(),
                bus_plugins: Vec::new(),
                scenes: Vec::new(),
                current_scene: None,
                morphing: false,
                midi_inputs: Vec::new(),
                midi_bindings: Vec::new(),
                midi_learning: None,
                scripts: Vec::new(),
                peers: Vec::new(),
                positions: Vec::new(),
            }),
            conn: ConnState::Live,
            status: None,
            health: Vec::new(),
            last_event: None,
            snapshots: 1,
        }
    }

    fn bus_view(plugin: Option<confluence_api::LoadedPlugin>) -> StoreView {
        let mut view = view_with(PointState { input: 0, output: 0, gain_db: 0.0, mute: false, invert: false });
        let state = view.state.as_mut().unwrap();
        state.slots.push(SlotState {
            id: 2,
            name: "FX".into(),
            device: confluence_api::BUS_DEVICE.into(),
            role: ClockRole::Strict,
            online: true,
            first_input: 2,
            inputs: 2,
            first_output: 2,
            outputs: 2,
            color: None,
        });
        state.bus_plugins = plugin.into_iter().collect();
        view
    }

    fn gain_plugin(status: confluence_api::PluginStatus) -> confluence_api::LoadedPlugin {
        confluence_api::LoadedPlugin {
            bus: 2,
            info: confluence_api::PluginInfo {
                path: "t.clap".into(),
                id: "t".into(),
                name: "Test Gain".into(),
                vendor: "Confluence".into(),
                version: "1".into(),
            },
            status,
            latency: 0,
            has_editor: false,
            editor_open: false,
            params: vec![confluence_api::ParamState {
                id: 1,
                name: "Gain".into(),
                module: String::new(),
                min: -60.0,
                max: 12.0,
                default: 0.0,
                value: 0.0,
                text: "0.0 dB".into(),
                stepped: false,
                read_only: false,
            }],
        }
    }

    /// Runs the inspector with bus 2 selected; returns what it asked for.
    fn bus_panel_actions(view: &StoreView, setup: impl Fn(&mut Harness<'_>)) -> Vec<Action> {
        let look = Look::builtin();
        let pending = std::collections::HashMap::new();
        let sel = Selection::Slot(2);
        let mut out = Vec::new();
        {
            let mut h = Harness::new_ui(|ui| {
                let ui_state = PluginUi { pending: &pending, loading: None };
                out.extend(show(ui, view, &look, &sel, None, None, true, &ui_state));
            });
            h.run();
            setup(&mut h);
            h.run();
        }
        out
    }

    #[test]
    fn a_network_streams_packets_and_losses_are_shown() {
        let mut view = view_with(PointState { input: 0, output: 0, gain_db: 0.0, mute: false, invert: false });
        view.health.push(confluence_api::SlotHealth {
            id: 1,
            underruns: 0,
            overruns: 0,
            fill_frames: 0.0,
            target_frames: 0.0,
            device_ppm: 0.0,
            correction_ppm: 0.0,
            device_lost: true,
            device_faults: 0,
            driver_requests: 0,
            attached: None,
            idle_note: None,
            net: Some(confluence_api::NetStats {
                packets: 1200,
                lost: 3,
                late: 1,
                reordered: 7,
                malformed: 0,
                silent_ms: 2500,
                mismatched: 40,
            }),
        });
        let look = Look::builtin();
        let pending = std::collections::HashMap::new();
        let sel = Selection::Slot(1);
        let mut h = Harness::new_ui(|ui| {
            let ui_state = PluginUi { pending: &pending, loading: None };
            show(ui, &view, &look, &sel, None, None, true, &ui_state);
        });
        h.run();
        assert!(h.query_by_label("Packets 1200 · lost 3 · late 1 · reordered 7").is_some());
        assert!(h.query_by_label_contains("No packets for 2.5 s").is_some());
        assert!(h.query_by_label_contains("40 packets at another sample rate").is_some());
    }

    /// Runs the inspector with slot 1 of `view` selected, clicking `label`.
    fn slot_panel_click(view: &StoreView, label: &str) -> Vec<Action> {
        let look = Look::builtin();
        let pending = std::collections::HashMap::new();
        let sel = Selection::Slot(1);
        let mut out = Vec::new();
        {
            let mut h = Harness::new_ui(|ui| {
                let ui_state = PluginUi { pending: &pending, loading: None };
                out.extend(show(ui, view, &look, &sel, None, None, true, &ui_state));
            });
            h.run();
            h.get_by_label(label).click();
            h.run();
        }
        out
    }

    #[test]
    fn a_slots_device_is_coloured_from_the_palette_or_put_back_to_the_default() {
        let view = view_with(PointState { input: 0, output: 0, gain_db: 0.0, mute: false, invert: false });
        let first = Look::builtin().skin.slot_colors[0];
        let label = format!("Colour #{:02x}{:02x}{:02x}", first.r(), first.g(), first.b());
        let picked = slot_panel_click(&view, &label);
        let want = Some([first.r(), first.g(), first.b()]);
        assert!(picked
            .iter()
            .any(|a| matches!(a, Action::Edit(Edit::SetSlotColor { id: 1, color }) if *color == want)));

        let mut coloured = view.clone();
        coloured.state.as_mut().unwrap().slots[0].color = Some([1, 2, 3]);
        let reset = slot_panel_click(&coloured, "Default colour");
        assert!(reset.iter().any(|a| matches!(a, Action::Edit(Edit::SetSlotColor { id: 1, color: None }))));
    }

    #[test]
    fn a_bus_without_a_plugin_offers_to_load_one() {
        let view = bus_view(None);
        let actions = bus_panel_actions(&view, |h| {
            assert!(h.query_by_label("Plugin: none").is_some());
            h.get_by_label("Load plugin…").click();
        });
        assert!(actions.iter().any(|a| matches!(a, Action::PickPlugin(2))));
    }

    #[test]
    fn a_plugins_parameter_is_a_slider_that_sends_its_value() {
        use eframe::egui::accesskit::{Action as AkAction, ActionData, ActionRequest};
        let view = bus_view(Some(gain_plugin(confluence_api::PluginStatus::Running)));
        let actions = bus_panel_actions(&view, |h| {
            assert!(h.query_by_label("Test Gain").is_some());
            assert!(h.query_by_label("0.0 dB").is_some(), "the plugin's own text");
            let (target_node, target_tree) = h.get_by_label("Gain").accesskit_node().locate();
            h.event(eframe::egui::Event::AccessKitActionRequest(ActionRequest {
                action: AkAction::SetValue,
                target_node,
                target_tree,
                data: Some(ActionData::NumericValue(-6.0)),
            }));
        });
        let sent: Vec<&Edit> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Edit(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [&Edit::SetParam { bus: 2, param: 1, value: -6.0 }]);
    }

    fn edits(actions: &[Action]) -> Vec<&Edit> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Edit(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_plugin_with_an_editor_can_show_and_close_it() {
        let mut p = gain_plugin(confluence_api::PluginStatus::Running);
        p.has_editor = true;
        let closed = bus_view(Some(p.clone()));
        let actions = bus_panel_actions(&closed, |h| h.get_by_label("Show editor").click());
        assert_eq!(edits(&actions), [&Edit::ShowEditor { bus: 2 }]);
        p.editor_open = true;
        let open = bus_view(Some(p));
        let actions = bus_panel_actions(&open, |h| {
            assert!(h.query_by_label("Bring editor to front").is_some());
            h.get_by_label("Close editor").click();
        });
        assert_eq!(edits(&actions), [&Edit::HideEditor { bus: 2 }]);
    }

    #[test]
    fn a_plugin_without_an_editor_says_so() {
        let view = bus_view(Some(gain_plugin(confluence_api::PluginStatus::Running)));
        bus_panel_actions(&view, |h| {
            assert!(h.query_by_label("Show editor").is_none());
            assert!(h.query_by_label("This plugin has no editor of its own").is_some());
        });
    }

    #[test]
    fn a_plugins_trouble_is_spelled_out() {
        let faulted = bus_view(Some(gain_plugin(confluence_api::PluginStatus::Faulted)));
        bus_panel_actions(&faulted, |h| {
            assert!(h.query_by_label_contains("stopped after an error").is_some());
        });
        let failed = bus_view(Some(gain_plugin(confluence_api::PluginStatus::Failed("t.clap was not found".into()))));
        bus_panel_actions(&failed, |h| {
            assert!(h.query_by_label_contains("Missing: t.clap was not found").is_some());
        });
    }

    /// The route panel's actions with route 0→0, after `setup`.
    fn route_panel_actions(view: &StoreView, setup: impl Fn(&mut Harness<'_>)) -> Vec<Action> {
        let look = Look::builtin();
        let pending = std::collections::HashMap::new();
        let sel = Selection::Cell { input: 0, output: 0 };
        let pt = view.state.as_ref().and_then(|s| s.points.first().cloned());
        let mut out = Vec::new();
        {
            let mut h = Harness::new_ui(|ui| {
                let ui_state = PluginUi { pending: &pending, loading: None };
                out.extend(show(ui, view, &look, &sel, pt.clone(), None, true, &ui_state));
            });
            h.run();
            setup(&mut h);
            h.run();
        }
        out
    }

    #[test]
    fn a_routes_midi_controls_are_shown_learned_and_forgotten() {
        let pt = PointState { input: 0, output: 0, gain_db: 0.0, mute: false, invert: false };
        let mut view = view_with(pt.clone());
        let none = route_panel_actions(&view, |h| h.get_by_label("MIDI Learn").click());
        assert_eq!(edits(&none), [&Edit::LearnMidi { input: 0, output: 0 }]);
        view.state.as_mut().unwrap().midi_bindings =
            vec![confluence_api::MidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7, input: 0, output: 0 }];
        let bound = route_panel_actions(&view, |h| {
            assert!(h.query_by_label("CC 7 · ch 1 · nanoKONTROL2").is_some());
            h.get_by_label("Forget").click();
        });
        assert_eq!(edits(&bound), [&Edit::RemoveMidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7 }]);
        view.state.as_mut().unwrap().midi_learning = Some((0, 0));
        let learning = route_panel_actions(&view, |h| {
            assert!(h.query_by_label_contains("Move a control").is_some());
            h.get_by_label("Cancel").click();
        });
        assert_eq!(edits(&learning), [&Edit::CancelMidiLearn]);
    }

    /// A route quieter than the slider's range must still show its real gain
    /// in the number field (and nudging it must start from that value).
    #[test]
    fn a_gain_outside_the_slider_range_is_shown_as_it_is() {
        let pt = PointState { input: 0, output: 0, gain_db: -80.0, mute: false, invert: false };
        let view = view_with(pt.clone());
        let look = Look::builtin();
        let sel = Selection::Cell { input: 0, output: 0 };
        let mut sent = Vec::new();
        let mut h = Harness::new_ui(|ui| {
            let pending = std::collections::HashMap::new();
            let plugin_ui = PluginUi { pending: &pending, loading: None };
            for a in show(ui, &view, &look, &sel, Some(pt.clone()), None, true, &plugin_ui) {
                if let Action::Edit(e) = a {
                    sent.push(e);
                }
            }
        });
        h.run();
        let field = h.get_by_role(eframe::egui::accesskit::Role::SpinButton).value();
        assert_eq!(field.as_deref(), Some("-80.0 dB"), "the number field shows the real gain");
        drop(h);
        assert!(sent.is_empty(), "showing the panel sends nothing: {sent:?}");
    }
}
