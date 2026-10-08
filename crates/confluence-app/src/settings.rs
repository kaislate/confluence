//! The Settings window: the gear finish, with a live preview.

use eframe::egui::{self, Color32, Id, Rect, Vec2};

use crate::gear::paint;
use crate::gear::skins::{Finish, GearSkin};

/// Where the chosen finish is stored (eframe storage).
pub const FINISH_KEY: &str = "skin_finish";

/// Shows the window while `open`; the radio buttons change `finish`.
pub fn show(ctx: &egui::Context, open: &mut bool, finish: &mut Finish) {
    egui::Window::new("Settings").open(open).resizable(false).collapsible(false).show(ctx, |ui| {
        ui.label("Finish");
        ui.horizontal(|ui| {
            for f in Finish::all() {
                ui.radio_value(finish, f, f.name());
            }
        });
        ui.add_space(8.0);
        preview(ui, *finish);
    });
}

/// One card face in the chosen finish: a knob and an OLED.
fn preview(ui: &mut egui::Ui, finish: Finish) {
    let s = GearSkin::preset(finish);
    let (r, _) = ui.allocate_exact_size(Vec2::new(300.0, 150.0), egui::Sense::hover());
    let face = r.shrink(14.0);
    paint::panel(ui.painter(), face, &s, None);
    let oled = Rect::from_min_size(face.min + Vec2::new(16.0, 18.0), Vec2::new(140.0, 44.0));
    paint::oled(ui.painter(), oled, &s, "VASIO A", "8 x 8", s.oled);
    paint::led(ui.painter(), oled.left_bottom() + Vec2::new(8.0, 26.0), &s, Color32::from_rgb(0x4c, 0xd9, 0x64), true);
    paint::etched(ui.painter(), oled.left_bottom() + Vec2::new(22.0, 26.0), "ONLINE", &s);
    let id = Id::new("settings-preview-knob");
    let mut value = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(0.62);
    let knob = Rect::from_center_size(face.right_center() - Vec2::new(62.0, 0.0), Vec2::splat(120.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(knob));
    let text = format!("{:.0}", value * 100.0);
    paint::knob(&mut child, id, 120.0, &mut value, Color32::from_rgb(0xff, 0x9f, 0x3a), s.p2, s.p2, &s, &text);
    ui.ctx().data_mut(|d| d.insert_temp(id, value));
}
