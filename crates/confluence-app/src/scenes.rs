//! The scene rail: one glass pill per scene (click to recall, the current
//! one lit), and a form to save the current mix as a scene.

use confluence_api::{State, MAX_MORPH_MS};
use eframe::egui::{self, Align2, DragValue, TextEdit, Vec2, WidgetInfo, WidgetType};

use crate::commands::Edit;
use crate::gear::motion::Motion;
use crate::gear::paint;
use crate::gear::skins::GearSkin;

/// The bar's own state: the "+ Scene" form.
pub struct SceneBar {
    pub adding: bool,
    pub name: String,
    /// Morph time for the new scene, seconds.
    pub morph_s: f32,
}

impl Default for SceneBar {
    fn default() -> Self {
        SceneBar { adding: false, name: String::new(), morph_s: 1.0 }
    }
}

fn ms(seconds: f32) -> u32 {
    ((seconds.clamp(0.0, MAX_MORPH_MS as f32 / 1000.0)) * 1000.0).round() as u32
}

/// Draws the rail; returns what the user asked for.
pub fn show(
    ui: &mut egui::Ui,
    state: &State,
    bar: &mut SceneBar,
    skin: &GearSkin,
    motion: &mut Motion,
    editable: bool,
) -> Vec<Edit> {
    let mut edits = Vec::new();
    ui.add_enabled_ui(editable, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (r, _) = ui.allocate_exact_size(Vec2::new(58.0, paint::PILL_H), egui::Sense::hover());
            paint::etched_text(
                ui.painter(),
                r.left_center() + Vec2::new(4.0, 0.0),
                Align2::LEFT_CENTER,
                "SCENES",
                skin,
                skin.ground_ink,
                10.5,
                true,
                0.16,
                0.7,
            );
            if state.scenes.is_empty() && !bar.adding {
                let (r, _) = ui.allocate_exact_size(Vec2::new(300.0, paint::PILL_H), egui::Sense::hover());
                paint::etched_text(
                    ui.painter(),
                    r.left_center(),
                    Align2::LEFT_CENTER,
                    "Save the current mix as a scene to recall it later",
                    skin,
                    skin.ground_ink,
                    11.5,
                    false,
                    0.0,
                    0.55,
                );
            }
            for s in &state.scenes {
                let current = state.current_scene.as_deref() == Some(s.name.as_str());
                let label = format!("Scene {}", s.name);
                let r = paint::pill_lit(ui, &s.name, &label, current, skin);
                r.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, current, &label));
                let r = r.on_hover_text(format!(
                    "Recall (glides over {:.1} s) \u{b7} {} routes, {} plugin settings \u{b7} right-click for more",
                    s.morph_ms as f32 / 1000.0,
                    s.routes,
                    s.params
                ));
                if r.clicked() {
                    edits.push(Edit::RecallScene { name: s.name.clone() });
                }
                r.context_menu(|ui| {
                    if ui.button("Update with the current mix").clicked() {
                        edits.push(Edit::SaveScene { name: s.name.clone(), morph_ms: s.morph_ms });
                        ui.close();
                    }
                    let mut secs = s.morph_ms as f32 / 1000.0;
                    ui.horizontal(|ui| {
                        ui.label("Morph");
                        let d = ui.add(DragValue::new(&mut secs).range(0.0..=10.0).speed(0.05).suffix(" s"));
                        if d.changed() {
                            edits.push(Edit::SetSceneMorph { name: s.name.clone(), morph_ms: ms(secs) });
                        }
                    });
                    if ui.button("Delete").clicked() {
                        edits.push(Edit::DeleteScene { name: s.name.clone() });
                        ui.close();
                    }
                });
            }
            if state.morphing {
                let k = 0.5 + 0.5 * motion.pulse(1.0);
                let (r, l) = ui.allocate_exact_size(Vec2::new(80.0, paint::PILL_H), egui::Sense::hover());
                l.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, "morphing\u{2026}"));
                paint::etched_text(
                    ui.painter(),
                    r.left_center(),
                    Align2::LEFT_CENTER,
                    "MORPHING",
                    skin,
                    skin.accent,
                    10.5,
                    true,
                    0.14,
                    k,
                );
            }
            ui.add_space(6.0);
            if bar.adding {
                ui.add(TextEdit::singleline(&mut bar.name).hint_text("Scene name").desired_width(120.0));
                ui.add(DragValue::new(&mut bar.morph_s).range(0.0..=10.0).speed(0.05).prefix("morph ").suffix(" s"));
                let name = bar.name.trim().to_string();
                let ok = !name.is_empty();
                let save = ui.add_enabled_ui(ok, |ui| paint::pill_labeled(ui, "Save", "Save scene", skin)).inner;
                if save.clicked() && ok {
                    edits.push(Edit::SaveScene { name, morph_ms: ms(bar.morph_s) });
                    bar.adding = false;
                    bar.name.clear();
                }
                if paint::pill_labeled(ui, "Cancel", "Cancel", skin).clicked() {
                    bar.adding = false;
                }
            } else if paint::pill_labeled(ui, "+ Scene", "+ Scene", skin).clicked() {
                bar.adding = true;
            }
        });
    });
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear::skins::Finish;
    use confluence_api::{EngineStatus, SceneInfo, State};
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    fn state(current: Option<&str>, morphing: bool) -> State {
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
            scenes: ["Verse", "Chorus"]
                .iter()
                .map(|n| SceneInfo { name: (*n).into(), morph_ms: 1500, routes: 2, params: 0 })
                .collect(),
            current_scene: current.map(String::from),
            morphing,
            midi_inputs: Vec::new(),
            midi_bindings: Vec::new(),
            midi_learning: None,
            scripts: Vec::new(),
            peers: Vec::new(),
            positions: Vec::new(),
        }
    }

    fn harness(st: State) -> Harness<'static, (SceneBar, Vec<Edit>, Motion)> {
        let skin = GearSkin::preset(Finish::Graphite);
        Harness::new_ui_state(
            move |ui, (bar, edits, motion): &mut (SceneBar, Vec<Edit>, Motion)| {
                motion.begin_frame(ui.ctx());
                edits.extend(show(ui, &st, bar, &skin, motion, true));
                motion.end_frame(ui.ctx());
            },
            (SceneBar::default(), Vec::new(), Motion::default()),
        )
    }

    #[test]
    fn clicking_a_scene_recalls_it() {
        let mut h = harness(state(Some("Verse"), false));
        h.run();
        h.get_by_label("Scene Chorus").click();
        h.run();
        assert_eq!(h.state().1, vec![Edit::RecallScene { name: "Chorus".into() }]);
    }

    #[test]
    fn the_current_mix_is_saved_as_a_new_scene() {
        let mut h = harness(state(None, false));
        h.run();
        h.get_by_label("+ Scene").click();
        h.run();
        {
            let bar = &mut h.state_mut().0;
            bar.name = "Bridge".into();
            bar.morph_s = 2.5;
        }
        h.run();
        h.get_by_label("Save scene").click();
        h.run();
        assert_eq!(h.state().1, vec![Edit::SaveScene { name: "Bridge".into(), morph_ms: 2500 }]);
        assert!(h.query_by_label("Save scene").is_none(), "the form closes");
    }

    #[test]
    fn a_scene_is_updated_or_deleted_from_its_menu() {
        let mut h = harness(state(None, false));
        h.run();
        h.get_by_label("Scene Verse").click_secondary();
        h.run();
        h.get_by_label("Delete").click();
        h.run();
        assert_eq!(h.state().1, vec![Edit::DeleteScene { name: "Verse".into() }]);
        h.get_by_label("Scene Chorus").click_secondary();
        h.run();
        h.get_by_label("Update with the current mix").click();
        h.run();
        assert_eq!(h.state().1[1], Edit::SaveScene { name: "Chorus".into(), morph_ms: 1500 });
    }

    #[test]
    fn a_morph_in_progress_is_shown() {
        let mut h = harness(state(Some("Chorus"), true));
        h.run_steps(3); // the pulse keeps asking for frames
        assert!(h.query_by_label_contains("morphing").is_some());
    }
}
