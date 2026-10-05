//! The plugin picker: the CLAP plugins the engine found, filtered as you type.

use confluence_api::{PluginInfo, State};
use eframe::egui::{self, RichText, TextEdit};

use crate::commands::Edit;

/// The picker while it is open: for which bus, and the filter typed so far.
pub struct Picker {
    pub bus: u32,
    pub filter: String,
}

/// The plugins whose name or vendor contains `filter` (any case).
pub fn matching<'a>(all: &'a [PluginInfo], filter: &str) -> Vec<&'a PluginInfo> {
    let f = filter.trim().to_lowercase();
    all.iter()
        .filter(|p| f.is_empty() || p.name.to_lowercase().contains(&f) || p.vendor.to_lowercase().contains(&f))
        .collect()
}

/// What the user did in the picker this frame.
pub enum Choice {
    Load(Edit),
    Close,
}

/// Draws the picker; `None` while it stays open.
pub fn show(ctx: &egui::Context, state: &State, picker: &mut Picker) -> Option<Choice> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("plugin-picker")).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.heading("Load a plugin");
        ui.add(TextEdit::singleline(&mut picker.filter).hint_text("Filter").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let found = matching(&state.plugins, &picker.filter);
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            if state.plugins.is_empty() {
                ui.label("No CLAP plugins found.");
                ui.label(
                    RichText::new(
                        "Plugins are looked for in Common Files\\CLAP, in %LOCALAPPDATA%\\Programs\\Common\\CLAP \
                         and in the folders listed in CLAP_PATH.",
                    )
                    .weak(),
                );
            }
            for p in found {
                ui.horizontal(|ui| {
                    let load = ui.button("Load");
                    load.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Load {}", p.name))
                    });
                    if load.clicked() {
                        choice = Some(Choice::Load(Edit::LoadPlugin {
                            bus: picker.bus,
                            path: p.path.clone(),
                            plugin_id: p.id.clone(),
                        }));
                    }
                    ui.label(RichText::new(&p.name).strong());
                    ui.label(RichText::new(&p.vendor).weak());
                });
            }
            for (path, why) in &state.bad_plugins {
                ui.label(RichText::new(format!("{path}: {why}")).weak());
            }
        });
        ui.add_space(4.0);
        if ui.button("Cancel").clicked() {
            choice = Some(Choice::Close);
        }
    });
    if modal.should_close() && choice.is_none() {
        choice = Some(Choice::Close);
    }
    choice
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::PluginInfo;

    fn info(name: &str, vendor: &str) -> PluginInfo {
        PluginInfo {
            path: format!("{name}.clap"),
            id: name.to_lowercase(),
            name: name.into(),
            vendor: vendor.into(),
            version: "1".into(),
        }
    }

    #[test]
    fn the_filter_matches_name_or_vendor_in_any_case() {
        let all = [info("Valhalla Room", "Valhalla DSP"), info("Pro-Q", "FabFilter"), info("Surge XT", "Surge")];
        let names = |f: &str| matching(&all, f).iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["Valhalla Room", "Pro-Q", "Surge XT"]);
        assert_eq!(names("fab"), ["Pro-Q"]);
        assert_eq!(names("  ROOM "), ["Valhalla Room"]);
        assert!(names("nothing").is_empty());
    }
}
