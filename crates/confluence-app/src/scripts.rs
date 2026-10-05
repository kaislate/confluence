//! The scripts window: Luau scripts that react to MIDI, with an editor, their
//! status and their log.

use confluence_api::{ScriptStatus, State};
use eframe::egui::{self, RichText, TextEdit, WidgetInfo, WidgetType};

use crate::commands::Edit;

/// What a new script starts from.
pub const EXAMPLE: &str = r#"-- Runs for every MIDI message. m.kind is "cc", "note_on", "note_off" or
-- "other"; m.channel, m.cc / m.value, m.note / m.velocity, m.device.
function on_midi(m)
  if m.kind == "note_on" and m.note == 36 then
    -- Toggle mute on the route from input 0 to output 1.
    local r = confluence.route(0, 1)
    if r then confluence.set_route(0, 1, r.gain, not r.mute) end
  end
end
"#;

/// The window's own state: the script being edited.
#[derive(Default)]
pub struct ScriptsUi {
    /// The saved script being edited (`None`: a new one).
    pub opened: Option<String>,
    pub name: String,
    pub source: String,
    /// The editor is showing (a script opened or a new one started).
    pub editing: bool,
}

impl ScriptsUi {
    fn open(&mut self, name: &str, source: &str) {
        self.opened = Some(name.to_string());
        self.name = name.to_string();
        self.source = source.to_string();
        self.editing = true;
    }
}

fn status_word(s: &ScriptStatus) -> &'static str {
    match s {
        ScriptStatus::Running => "running",
        ScriptStatus::Stopped(_) => "stopped",
        ScriptStatus::Disabled => "disabled",
    }
}

/// Draws the window's contents; returns what the user asked for.
pub fn show(ui: &mut egui::Ui, state: &State, w: &mut ScriptsUi, editable: bool) -> Vec<Edit> {
    let mut edits = Vec::new();
    ui.add_enabled_ui(editable, |ui| {
        if state.scripts.is_empty() {
            ui.label(RichText::new("No scripts yet. A script reacts to MIDI, e.g. a pad that mutes a route.").weak());
        }
        egui::Grid::new("scripts").num_columns(3).show(ui, |ui| {
            for s in &state.scripts {
                let mut on = s.enabled;
                let verb = if s.enabled { "Disable" } else { "Enable" };
                let c = ui.checkbox(&mut on, "");
                c.widget_info(|| {
                    WidgetInfo::selected(WidgetType::Checkbox, true, s.enabled, format!("{verb} {}", s.name))
                });
                if c.changed() {
                    edits.push(Edit::SetScript { name: s.name.clone(), source: s.source.clone(), enabled: on });
                }
                let open = w.opened.as_deref() == Some(s.name.as_str());
                let r = ui.selectable_label(open, &s.name);
                r.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, open, format!("Open {}", s.name)));
                if r.clicked() {
                    w.open(&s.name, &s.source);
                }
                let word = ui.label(status_word(&s.status));
                if let ScriptStatus::Stopped(why) = &s.status {
                    word.on_hover_text(why);
                }
                ui.end_row();
            }
        });
        let new = ui.button("New");
        new.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "New script"));
        if new.clicked() {
            let n = (1..).find(|n| !state.scripts.iter().any(|s| s.name == format!("Script {n}"))).unwrap_or(1);
            *w = ScriptsUi { opened: None, name: format!("Script {n}"), source: EXAMPLE.into(), editing: true };
        }
        if !w.editing {
            return;
        }
        ui.separator();
        let saved = w.opened.as_ref().and_then(|n| state.scripts.iter().find(|s| &s.name == n));
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(TextEdit::singleline(&mut w.name).desired_width(160.0));
            let name = w.name.trim().to_string();
            let save = ui.add_enabled(!name.is_empty(), egui::Button::new("Save"));
            save.widget_info(|| WidgetInfo::labeled(WidgetType::Button, !name.is_empty(), "Save script"));
            if save.clicked() {
                let enabled = saved.is_none_or(|s| s.enabled);
                edits.push(Edit::SetScript { name: name.clone(), source: w.source.clone(), enabled });
                if let Some(old) = w.opened.as_ref().filter(|old| **old != name) {
                    edits.push(Edit::DeleteScript { name: old.clone() });
                }
                w.opened = Some(name);
            }
            if let Some(s) = saved {
                let del = ui.button("Delete");
                del.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Delete script"));
                if del.clicked() {
                    edits.push(Edit::DeleteScript { name: s.name.clone() });
                    *w = ScriptsUi::default();
                }
            }
        });
        if let Some(ScriptStatus::Stopped(why)) = saved.map(|s| &s.status) {
            ui.colored_label(ui.visuals().error_fg_color, format!("Stopped: {why}"));
        }
        egui::ScrollArea::vertical().id_salt("script-source").max_height(320.0).show(ui, |ui| {
            ui.add(TextEdit::multiline(&mut w.source).code_editor().desired_rows(14).desired_width(f32::INFINITY));
        });
        if let Some(s) = saved.filter(|s| !s.log.is_empty()) {
            ui.label(RichText::new("Log").strong());
            egui::ScrollArea::vertical().id_salt("script-log").max_height(120.0).stick_to_bottom(true).show(ui, |ui| {
                for line in &s.log {
                    ui.label(RichText::new(line).monospace());
                }
            });
        }
    });
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{EngineStatus, ScriptInfo, ScriptStatus, State};
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    fn state(scripts: Vec<ScriptInfo>) -> State {
        State {
            version: 1,
            status: EngineStatus {
                master: "internal".into(),
                sample_rate: 48_000.0,
                block: 256,
                blocks: 0,
                dsp_load: 0.0,
                xruns: 0,
            },
            slots: Vec::new(),
            points: Vec::new(),
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
            scripts,
        }
    }

    fn info(name: &str, enabled: bool, status: ScriptStatus) -> ScriptInfo {
        ScriptInfo {
            name: name.into(),
            source: format!("-- {name}"),
            enabled,
            status,
            log: vec![format!("{name} says hi")],
        }
    }

    fn harness(st: State) -> Harness<'static, (ScriptsUi, Vec<Edit>)> {
        Harness::new_ui_state(
            move |ui, (w, edits): &mut (ScriptsUi, Vec<Edit>)| edits.extend(show(ui, &st, w, true)),
            (ScriptsUi::default(), Vec::new()),
        )
    }

    #[test]
    fn a_new_script_starts_from_an_example_and_is_saved() {
        let mut h = harness(state(Vec::new()));
        h.run();
        assert!(h.query_by_label_contains("No scripts yet").is_some());
        h.get_by_label("New script").click();
        h.run();
        assert_eq!(h.state().0.name, "Script 1");
        assert_eq!(h.state().0.source, EXAMPLE);
        h.state_mut().0.source = "function on_midi(m) end".into();
        h.run();
        h.get_by_label("Save script").click();
        h.run();
        assert_eq!(
            h.state().1,
            vec![Edit::SetScript { name: "Script 1".into(), source: "function on_midi(m) end".into(), enabled: true }]
        );
    }

    #[test]
    fn a_script_is_opened_shown_and_deleted() {
        let mut h = harness(state(vec![
            info("mute", true, ScriptStatus::Stopped("boom at line 3".into())),
            info("off", false, ScriptStatus::Disabled),
        ]));
        h.run();
        assert!(h.query_by_label("stopped").is_some(), "status in the list");
        assert!(h.query_by_label("disabled").is_some());
        h.get_by_label("Open mute").click();
        h.run();
        assert_eq!(h.state().0.source, "-- mute", "its source in the editor");
        assert!(h.query_by_label_contains("boom at line 3").is_some(), "why it stopped");
        assert!(h.query_by_label_contains("mute says hi").is_some(), "its log");
        h.get_by_label("Delete script").click();
        h.run();
        assert_eq!(h.state().1, vec![Edit::DeleteScript { name: "mute".into() }]);
    }

    #[test]
    fn a_script_is_enabled_and_disabled_from_the_list() {
        let mut h = harness(state(vec![info("off", false, ScriptStatus::Disabled)]));
        h.run();
        h.get_by_label("Enable off").click();
        h.run();
        assert_eq!(h.state().1, vec![Edit::SetScript { name: "off".into(), source: "-- off".into(), enabled: true }]);
    }

    #[test]
    fn a_renamed_script_replaces_the_old_one() {
        let mut h = harness(state(vec![info("old", true, ScriptStatus::Running)]));
        h.run();
        h.get_by_label("Open old").click();
        h.run();
        h.state_mut().0.name = "new".into();
        h.run();
        h.get_by_label("Save script").click();
        h.run();
        assert_eq!(
            h.state().1,
            vec![
                Edit::SetScript { name: "new".into(), source: "-- old".into(), enabled: true },
                Edit::DeleteScript { name: "old".into() },
            ]
        );
    }
}
