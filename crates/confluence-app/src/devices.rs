//! The devices panel: what the engine can open, grouped by kind; devices
//! already bound to a slot show "in use".

use std::collections::{HashMap, HashSet};

use confluence_api::{DeviceInfo, DeviceKind, SlotState};
use eframe::egui::{self, RichText, Spinner, TextEdit, WidgetInfo, WidgetType};

use crate::commands::Edit;

/// Kinds in the order the panel lists them (app capture has its own row).
const KINDS: [DeviceKind; 5] =
    [DeviceKind::Asio, DeviceKind::WasapiRender, DeviceKind::WasapiCapture, DeviceKind::Vasio, DeviceKind::Vaio];

#[derive(Default)]
pub struct DevicesState {
    /// Per VASIO instance, the channel spec typed so far (`8x2`).
    pub vasio_spec: HashMap<String, String>,
    /// The app capture field.
    pub app_name: String,
    /// Adds in flight, keyed by kind and the listed name.
    pub adding: HashSet<(DeviceKind, String)>,
}

pub fn kind_title(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Asio => "ASIO",
        DeviceKind::WasapiRender => "Windows output",
        DeviceKind::WasapiCapture => "Windows input",
        DeviceKind::AppCapture => "App capture",
        DeviceKind::Vasio => "VASIO",
        DeviceKind::Vaio => "VAIO",
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
    fn kinds_have_readable_titles() {
        assert_eq!(kind_title(DeviceKind::WasapiRender), "Windows output");
        assert_eq!(kind_title(DeviceKind::Vasio), "VASIO");
    }
}
