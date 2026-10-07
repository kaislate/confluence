//! The devices panel: what the engine can open, grouped by kind; devices
//! already bound to a slot show "in use".

use std::collections::{HashMap, HashSet};

use confluence_api::{DeviceInfo, DeviceKind, SlotState};
use eframe::egui::{self, RichText, Spinner, TextEdit, WidgetInfo, WidgetType};

use crate::commands::Edit;

/// Kinds in the order the panel lists them (app capture has its own row).
const KINDS: [DeviceKind; 5] =
    [DeviceKind::Asio, DeviceKind::WasapiRender, DeviceKind::WasapiCapture, DeviceKind::Vasio, DeviceKind::Vaio];

pub struct DevicesState {
    /// Per VASIO instance, the channel spec typed so far (`8x2`).
    pub vasio_spec: HashMap<String, String>,
    /// The app capture field.
    pub app_name: String,
    /// Adds in flight, keyed by kind and the listed name.
    pub adding: HashSet<(DeviceKind, String)>,
    /// The insert bus name field.
    pub bus_name: String,
    /// The insert bus channel count.
    pub bus_channels: u32,
    /// An insert bus is being added.
    pub adding_bus: bool,
    /// The name of a stream to send.
    pub net_stream: String,
    /// Its channel count.
    pub net_channels: u32,
    /// An address to send to (an engine not found on the network).
    pub net_address: String,
}

impl Default for DevicesState {
    fn default() -> Self {
        DevicesState {
            vasio_spec: HashMap::new(),
            app_name: String::new(),
            adding: HashSet::new(),
            bus_name: String::new(),
            bus_channels: 2,
            adding_bus: false,
            net_stream: "Main".into(),
            net_channels: 2,
            net_address: String::new(),
        }
    }
}

pub fn kind_title(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Asio => "ASIO",
        DeviceKind::WasapiRender => "Windows output",
        DeviceKind::WasapiCapture => "Windows input",
        DeviceKind::AppCapture => "App capture",
        DeviceKind::Vasio => "VASIO",
        DeviceKind::Vaio => "VAIO",
        DeviceKind::NetSend => "Network send",
        DeviceKind::NetReceive => "Network receive",
    }
}

/// True when a slot is bound to this device (`kind:name`, or `vasio:N:spec`).
pub fn in_use(device: &DeviceInfo, slots: &[SlotState]) -> bool {
    let binding = format!("{}:{}", device.kind.prefix(), device.name);
    let with_spec = format!("{binding}:");
    slots.iter().any(|s| s.device == binding || (device.kind == DeviceKind::Vasio && s.device.starts_with(&with_spec)))
}

/// The name sent for a VASIO instance: `N`, or `N:spec`.
pub fn vasio_name(instance: &str, spec: &str) -> String {
    let spec = spec.trim();
    if spec.is_empty() {
        instance.to_string()
    } else {
        format!("{instance}:{spec}")
    }
}

/// The listed name an `AddDevice` name belongs to (a VASIO spec is dropped).
pub fn base_name(kind: DeviceKind, name: &str) -> String {
    match kind {
        DeviceKind::Vasio => name.split(':').next().unwrap_or(name).to_string(),
        _ => name.to_string(),
    }
}

fn add_button(ui: &mut egui::Ui, accessible: String, enabled: bool) -> bool {
    let b = ui.add_enabled(enabled, egui::Button::new("Add"));
    b.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &accessible));
    b.clicked()
}

/// Streams from other Confluence engines (one click to play one) and sending
/// to an engine found on the network or at an address.
fn network(ui: &mut egui::Ui, devices: &[DeviceInfo], slots: &[SlotState], st: &mut DevicesState) -> Vec<Edit> {
    let mut edits = Vec::new();
    ui.label(RichText::new("Network").strong());
    let heard: Vec<&DeviceInfo> = devices.iter().filter(|d| d.kind == DeviceKind::NetReceive).collect();
    if heard.is_empty() {
        ui.label(RichText::new("No streams arriving from other engines").weak());
    }
    for d in heard {
        ui.horizontal(|ui| {
            ui.label(&d.name);
            ui.label(RichText::new(format!("{} ch", d.inputs)).weak());
            if in_use(d, slots) {
                ui.label(RichText::new("in use").weak());
            } else if st.adding.contains(&(d.kind, d.name.clone())) {
                ui.add(Spinner::new());
            } else if add_button(ui, format!("Add {} {}", kind_title(d.kind), d.name), true) {
                st.adding.insert((d.kind, d.name.clone()));
                edits.push(Edit::AddDevice { kind: d.kind, name: d.name.clone() });
            }
        });
    }
    ui.horizontal(|ui| {
        ui.label("Send");
        ui.add(TextEdit::singleline(&mut st.net_stream).hint_text("stream name").desired_width(90.0));
        ui.add(egui::DragValue::new(&mut st.net_channels).range(1..=64).suffix(" ch"));
    });
    let stream = st.net_stream.trim().to_string();
    let mut send = |ui: &mut egui::Ui, to: &str, label: String, edits: &mut Vec<Edit>| {
        let name = format!("{to}/{stream}:{}", st.net_channels);
        let ok = !to.is_empty() && !stream.is_empty();
        if st.adding.contains(&(DeviceKind::NetSend, name.clone())) {
            ui.add(Spinner::new());
            return;
        }
        let b = ui.add_enabled(ok, egui::Button::new("Send"));
        b.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ok, &label));
        if b.clicked() {
            st.adding.insert((DeviceKind::NetSend, name.clone()));
            edits.push(Edit::AddDevice { kind: DeviceKind::NetSend, name });
        }
    };
    for d in devices.iter().filter(|d| d.kind == DeviceKind::NetSend) {
        ui.horizontal(|ui| {
            ui.label(format!("to {}", d.name));
            send(ui, &d.name, format!("Send to {}", d.name), &mut edits);
        });
    }
    ui.horizontal(|ui| {
        ui.label("to");
        ui.add(TextEdit::singleline(&mut st.net_address).hint_text("IP address").desired_width(110.0));
        let address = st.net_address.trim().to_string();
        send(ui, &address, "Send to address".into(), &mut edits);
    });
    edits
}

pub fn show(
    ui: &mut egui::Ui,
    devices: &[DeviceInfo],
    slots: &[SlotState],
    st: &mut DevicesState,
    editable: bool,
) -> Vec<Edit> {
    let mut edits = Vec::new();
    ui.heading("Devices");
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_enabled_ui(editable, |ui| {
            // First: other engines' streams are the quickest thing to add.
            edits.extend(network(ui, devices, slots, st));
            for kind in KINDS {
                let list: Vec<&DeviceInfo> = devices.iter().filter(|d| d.kind == kind).collect();
                if list.is_empty() {
                    continue;
                }
                ui.add_space(6.0);
                ui.label(RichText::new(kind_title(kind)).strong());
                for d in list {
                    ui.horizontal(|ui| {
                        ui.label(&d.name);
                        if d.inputs + d.outputs > 0 {
                            ui.label(RichText::new(format!("{} in / {} out", d.inputs, d.outputs)).weak());
                        }
                        if in_use(d, slots) {
                            ui.label(RichText::new("in use").weak());
                        } else if st.adding.contains(&(kind, d.name.clone())) {
                            ui.add(Spinner::new());
                        } else {
                            let mut name = d.name.clone();
                            if kind == DeviceKind::Vasio {
                                let spec = st.vasio_spec.entry(d.name.clone()).or_default();
                                ui.add(TextEdit::singleline(spec).hint_text("2x2").desired_width(48.0));
                                name = vasio_name(&d.name, spec);
                            }
                            if add_button(ui, format!("Add {} {}", kind_title(kind), d.name), true) {
                                st.adding.insert((kind, d.name.clone()));
                                edits.push(Edit::AddDevice { kind, name });
                            }
                        }
                    });
                }
            }
            ui.add_space(6.0);
            ui.label(RichText::new(kind_title(DeviceKind::AppCapture)).strong());
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut st.app_name).hint_text("process name or PID").desired_width(140.0));
                let name = st.app_name.trim().to_string();
                if st.adding.iter().any(|(kind, _)| *kind == DeviceKind::AppCapture) {
                    ui.add(Spinner::new());
                } else if add_button(ui, "Add app capture".into(), !name.is_empty()) {
                    st.adding.insert((DeviceKind::AppCapture, name.clone()));
                    edits.push(Edit::AddDevice { kind: DeviceKind::AppCapture, name });
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new("Insert bus").strong());
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut st.bus_name).hint_text("Bus name").desired_width(100.0));
                ui.add(egui::DragValue::new(&mut st.bus_channels).range(1..=64).suffix(" ch"));
                let name = st.bus_name.trim().to_string();
                if st.adding_bus {
                    ui.add(Spinner::new());
                } else if add_button(ui, "Add insert bus".into(), !name.is_empty()) {
                    st.adding_bus = true;
                    edits.push(Edit::AddBus { name, channels: st.bus_channels });
                }
            });
        });
    });
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::ClockRole;

    fn bound(device: &str) -> SlotState {
        SlotState {
            id: 1,
            name: "x".into(),
            device: device.into(),
            role: ClockRole::Soft,
            online: true,
            first_input: 0,
            inputs: 2,
            first_output: 0,
            outputs: 2,
            color: None,
        }
    }

    fn dev(kind: DeviceKind, name: &str) -> DeviceInfo {
        DeviceInfo { kind, name: name.into(), inputs: 0, outputs: 0 }
    }

    #[test]
    fn a_device_bound_to_a_slot_is_in_use() {
        let slots = vec![bound("asio:GoXLR ASIO Driver"), bound("vasio:2:8x2")];
        assert!(in_use(&dev(DeviceKind::Asio, "GoXLR ASIO Driver"), &slots));
        assert!(in_use(&dev(DeviceKind::Vasio, "2"), &slots), "a VASIO with a channel spec");
        assert!(!in_use(&dev(DeviceKind::Vasio, "1"), &slots));
        assert!(!in_use(&dev(DeviceKind::WasapiRender, "Speakers"), &slots));
    }

    #[test]
    fn a_vasio_channel_spec_is_appended() {
        assert_eq!(vasio_name("2", ""), "2");
        assert_eq!(vasio_name("2", " 8x2 "), "2:8x2");
        assert_eq!(base_name(DeviceKind::Vasio, "2:8x2"), "2");
        assert_eq!(base_name(DeviceKind::Asio, "A:B"), "A:B");
    }

    #[test]
    fn app_capture_needs_a_name_and_shows_progress() {
        use egui_kittest::kittest::{NodeT, Queryable};
        use egui_kittest::Harness;
        let mut h = Harness::new_ui_state(
            |ui, (st, edits): &mut (DevicesState, Vec<Edit>)| {
                edits.extend(show(ui, &[], &[], st, true));
            },
            (DevicesState::default(), Vec::new()),
        );
        h.run();
        assert!(h.get_by_label("Add app capture").accesskit_node().is_disabled(), "no name: nothing to add");
        h.state_mut().0.app_name = "Discord".into();
        h.run();
        assert!(!h.get_by_label("Add app capture").accesskit_node().is_disabled());
        h.get_by_label("Add app capture").click();
        h.step(); // the spinner keeps animating: one frame, not run-until-idle
        h.step();
        assert_eq!(h.state().1, vec![Edit::AddDevice { kind: DeviceKind::AppCapture, name: "Discord".into() }]);
        assert!(h.query_by_label("Add app capture").is_none(), "a spinner while it is being added");
    }

    #[test]
    fn an_insert_bus_needs_a_name_and_shows_progress() {
        use egui_kittest::kittest::{NodeT, Queryable};
        use egui_kittest::Harness;
        let mut h = Harness::new_ui_state(
            |ui, (st, edits): &mut (DevicesState, Vec<Edit>)| {
                edits.extend(show(ui, &[], &[], st, true));
            },
            (DevicesState::default(), Vec::new()),
        );
        h.run();
        assert!(h.get_by_label("Add insert bus").accesskit_node().is_disabled(), "no name: nothing to add");
        h.state_mut().0.bus_name = " Verb ".into();
        h.run();
        h.get_by_label("Add insert bus").click();
        h.step(); // the spinner keeps animating: one frame, not run-until-idle
        h.step();
        assert_eq!(h.state().1, vec![Edit::AddBus { name: "Verb".into(), channels: 2 }], "two channels by default");
        assert!(h.query_by_label("Add insert bus").is_none(), "a spinner while it is being added");
    }

    fn net_harness(devices: Vec<DeviceInfo>) -> egui_kittest::Harness<'static, (DevicesState, Vec<Edit>)> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (st, edits): &mut (DevicesState, Vec<Edit>)| {
                edits.extend(show(ui, &devices, &[], st, true));
            },
            (DevicesState::default(), Vec::new()),
        )
    }

    #[test]
    fn a_heard_stream_is_received_with_one_click() {
        use egui_kittest::kittest::Queryable;
        let heard = DeviceInfo { kind: DeviceKind::NetReceive, name: "Lilith/Main".into(), inputs: 2, outputs: 0 };
        let mut h = net_harness(vec![heard]);
        h.run();
        assert!(h.query_by_label("Network").is_some());
        h.get_by_label("Add Network receive Lilith/Main").click();
        h.step();
        h.step();
        assert_eq!(h.state().1, vec![Edit::AddDevice { kind: DeviceKind::NetReceive, name: "Lilith/Main".into() }]);
    }

    #[test]
    fn audio_is_sent_to_an_engine_found_or_an_address() {
        use egui_kittest::kittest::{NodeT, Queryable};
        let lilith = DeviceInfo { kind: DeviceKind::NetSend, name: "Lilith".into(), inputs: 0, outputs: 0 };
        let mut h = net_harness(vec![lilith]);
        h.run();
        h.state_mut().0.net_stream = "Stream mix".into();
        h.state_mut().0.net_channels = 4;
        h.run();
        h.get_by_label("Send to Lilith").click();
        h.step();
        h.step();
        assert_eq!(
            h.state().1,
            vec![Edit::AddDevice { kind: DeviceKind::NetSend, name: "Lilith/Stream mix:4".into() }]
        );
        assert!(h.get_by_label("Send to address").accesskit_node().is_disabled(), "no address yet");
        h.state_mut().0.net_address = " 192.168.50.12 ".into();
        h.step(); // the first send's spinner keeps animating
        h.step();
        h.get_by_label("Send to address").click();
        h.step();
        h.step();
        assert_eq!(
            h.state().1[1],
            Edit::AddDevice { kind: DeviceKind::NetSend, name: "192.168.50.12/Stream mix:4".into() }
        );
    }

    #[test]
    fn kinds_have_readable_titles() {
        assert_eq!(kind_title(DeviceKind::WasapiRender), "Windows output");
        assert_eq!(kind_title(DeviceKind::Vasio), "VASIO");
    }
}
