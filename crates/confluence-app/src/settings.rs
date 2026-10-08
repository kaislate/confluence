//! The Settings screen (spec: round 3 §5): every choice shown as what it
//! does. Finish tiles painted in each finish, live meters for each meter
//! style, and gear switches for names, motion and advanced options.

use std::time::Duration;

use eframe::egui::{self, Align, Align2, Color32, Id, Pos2, Rect, Sense, Vec2, WidgetInfo, WidgetType};

use crate::gear::motion::Motion;
use crate::gear::oled_meter::{self, Chan, Geom, Group, MeterLook, MeterStyle};
use crate::gear::paint;
use crate::gear::skins::{Finish, GearSkin};
use crate::prefs::ViewPrefs;

/// Where the chosen finish is stored (eframe storage).
pub const FINISH_KEY: &str = "skin_finish";
/// Where "Reduce motion" is stored.
pub const REDUCE_MOTION_KEY: &str = "reduce_motion";

/// The screen's sections, in order.
const SECTIONS: [&str; 5] = ["Appearance", "Meters", "Names", "Motion", "Advanced"];
/// The section list's width.
const NAV_W: f32 = 180.0;
/// Panes sit side by side from this content width.
const TWO_COLUMNS: f32 = 1100.0;
const PANE_PAD: f32 = 16.0;
/// The Names pane's height (two switches and a preview).
const NAMES_H: f32 = 222.0;
/// The chosen tile's outline.
const GOLD: Color32 = Color32::from_rgb(0xf3, 0xc2, 0x4f);

fn section_id() -> Id {
    Id::new("settings-section")
}

/// Draws the Settings screen in `ui`.
pub fn screen(
    ui: &mut egui::Ui,
    skin: &GearSkin,
    finish: &mut Finish,
    reduce: &mut bool,
    prefs: &mut ViewPrefs,
    _motion: &mut Motion,
) {
    let area = ui.max_rect();
    paint::ground(ui.painter(), area, skin);
    let (current, mut scroll_to) = ui.ctx().data(|d| d.get_temp::<(usize, bool)>(section_id())).unwrap_or((0, false));
    // The section list.
    let nav = Rect::from_min_size(area.min + Vec2::new(16.0, 18.0), Vec2::new(NAV_W, area.height() - 36.0));
    for (i, name) in SECTIONS.iter().enumerate() {
        let r = Rect::from_min_size(nav.min + Vec2::new(0.0, i as f32 * 38.0), Vec2::new(NAV_W, 32.0));
        let resp = ui.interact(r, Id::new(("settings-nav", i)), Sense::click());
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, i == current, format!("{name} section")));
        let p = ui.painter();
        if i == current {
            p.rect_filled(r, egui::CornerRadius::same(9), paint::alpha(skin.ground_ink, 0.08));
            p.rect_filled(
                Rect::from_min_size(r.min + Vec2::new(0.0, 6.0), Vec2::new(3.0, r.height() - 12.0)),
                egui::CornerRadius::same(2),
                GOLD,
            );
        } else if resp.hovered() {
            p.rect_filled(r, egui::CornerRadius::same(9), paint::alpha(skin.ground_ink, 0.04));
        }
        let ink = paint::alpha(skin.ground_ink, if i == current { 1.0 } else { 0.7 });
        p.text(
            r.left_center() + Vec2::new(14.0, 0.0),
            Align2::LEFT_CENTER,
            *name,
            paint::font(ui.ctx(), "label-bold", 13.0),
            ink,
        );
        if resp.clicked() {
            ui.ctx().data_mut(|d| d.insert_temp(section_id(), (i, true)));
            scroll_to = false;
        }
    }
    // The panes.
    let content = Rect::from_min_max(Pos2::new(nav.right() + 20.0, area.top()), area.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content));
    egui::ScrollArea::vertical().id_salt("settings-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        let width = (ui.available_width() - 20.0).min(1240.0);
        ui.add_space(18.0);
        let section = |ui: &mut egui::Ui, index: usize, r: Rect| {
            if scroll_to && index == current {
                ui.scroll_to_rect(r, Some(Align::TOP));
            }
        };
        let two = width >= TWO_COLUMNS;
        // Appearance: the finishes.
        let r = pane(ui, width, 196.0, "FINISH", "The look of the whole app.", skin);
        section(ui, 0, r);
        finish_tiles(ui, r, finish, skin);
        // Meters: the styles, peak line and clip.
        let r = pane(ui, width, 262.0, "METERS", "Click a style. The peak line and clip show on the meters.", skin);
        section(ui, 1, r);
        meter_tiles(ui, r, &mut prefs.meter, skin);
        // Names and Motion: side by side on wide screens.
        let half = if two { (width - 16.0) / 2.0 } else { width };
        let (names, motion_r) = if two {
            let (row, _) = ui.allocate_exact_size(Vec2::new(width, NAMES_H), Sense::hover());
            let a = Rect::from_min_size(row.min, Vec2::new(half, NAMES_H));
            let b = Rect::from_min_size(row.min + Vec2::new(half + 16.0, 0.0), Vec2::new(half, NAMES_H));
            pane_at(ui, a, "NAMES", "What cards and the bridge call a device.", skin);
            pane_at(ui, b, "MOTION", "Springs, fades and sliding panels.", skin);
            ui.add_space(16.0);
            (a, b)
        } else {
            let a = pane(ui, width, NAMES_H, "NAMES", "What cards and the bridge call a device.", skin);
            let b = pane(ui, width, 170.0, "MOTION", "Springs, fades and sliding panels.", skin);
            (a, b)
        };
        section(ui, 2, names);
        section(ui, 3, motion_r);
        names_pane(ui, names, prefs, skin);
        motion_pane(ui, motion_r, reduce, skin);
        // Advanced.
        let r = pane(ui, width, 128.0, "ADVANCED", "Options most people never need.", skin);
        section(ui, 4, r);
        let sw = Rect::from_min_size(r.min + Vec2::new(PANE_PAD, 58.0), Vec2::new(r.width() - 2.0 * PANE_PAD, 44.0));
        paint::switch(
            ui,
            sw,
            &mut prefs.advanced,
            "Enable advanced options",
            "The app picker also lists background processes and can capture by process name or PID.",
            skin,
        );
        ui.add_space(24.0);
    });
    if scroll_to {
        ui.ctx().data_mut(|d| d.insert_temp(section_id(), (current, false)));
    }
    // The live meters and the motion demo move.
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

/// Allocates a pane `height` tall across `width` and draws its frame and
/// title; returns its rect.
fn pane(ui: &mut egui::Ui, width: f32, height: f32, title: &str, sub: &str, skin: &GearSkin) -> Rect {
    let (r, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    pane_at(ui, r, title, sub, skin);
    ui.add_space(16.0);
    r
}

fn pane_at(ui: &egui::Ui, r: Rect, title: &str, sub: &str, skin: &GearSkin) {
    let p = ui.painter();
    paint::recess(p, r, skin, 14);
    paint::etched_text(
        p,
        r.min + Vec2::new(PANE_PAD, 20.0),
        Align2::LEFT_CENTER,
        title,
        skin,
        skin.ground_ink,
        11.0,
        true,
        0.16,
        0.9,
    );
    p.text(
        r.min + Vec2::new(PANE_PAD, 38.0),
        Align2::LEFT_CENTER,
        sub,
        paint::font(ui.ctx(), "label", 12.0),
        paint::alpha(skin.ground_ink, 0.6),
    );
}

/// A clickable tile: an accessible button, `selected` outlined in gold.
fn tile(ui: &mut egui::Ui, r: Rect, id: Id, label: &str, selected: bool, skin: &GearSkin) -> egui::Response {
    let resp = ui.interact(r, id, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, label));
    let p = ui.painter();
    p.rect_filled(
        r,
        egui::CornerRadius::same(12),
        paint::alpha(Color32::BLACK, if resp.hovered() { 0.22 } else { 0.16 }),
    );
    if selected {
        p.rect_stroke(r, egui::CornerRadius::same(12), egui::Stroke::new(2.0, GOLD), egui::StrokeKind::Inside);
    }
    let ink = paint::alpha(skin.ground_ink, if selected { 1.0 } else { 0.78 });
    let f = paint::font(ui.ctx(), "label-bold", 12.5);
    p.text(Pos2::new(r.left() + 10.0, r.bottom() - 14.0), Align2::LEFT_CENTER, label, f, ink);
    if selected {
        // A check mark, drawn (the UI font has no glyph for it).
        let c = Pos2::new(r.right() - 18.0, r.bottom() - 14.0);
        let stroke = egui::Stroke::new(2.0, GOLD);
        p.line_segment([c + Vec2::new(-5.0, 0.0), c + Vec2::new(-1.5, 3.5)], stroke);
        p.line_segment([c + Vec2::new(-1.5, 3.5), c + Vec2::new(5.0, -4.0)], stroke);
    }
    resp
}

fn finish_tiles(ui: &mut egui::Ui, pane: Rect, finish: &mut Finish, skin: &GearSkin) {
    let size = Vec2::new(200.0, 130.0);
    for (i, f) in Finish::all().into_iter().enumerate() {
        let r = Rect::from_min_size(pane.min + Vec2::new(PANE_PAD + i as f32 * (size.x + 14.0), 52.0), size);
        if tile(ui, r, Id::new(("finish-tile", i)), f.name(), *finish == f, skin).clicked() {
            *finish = f;
        }
        // A miniature card in that finish.
        let s = GearSkin::preset(f);
        let mini = Rect::from_min_max(r.min + Vec2::new(8.0, 8.0), Pos2::new(r.right() - 8.0, r.bottom() - 30.0));
        let p = ui.painter_at(mini.expand(2.0));
        paint::ground(&p, mini, &s);
        let face = mini.shrink(7.0);
        paint::panel(&p, face, &s, None);
        paint::led(&p, face.min + Vec2::new(10.0, 10.0), &s, Color32::from_rgb(0x4c, 0xd9, 0x64), true);
        let bar = |y: f32, w: f32, c: Color32| {
            p.rect_filled(
                Rect::from_min_size(face.min + Vec2::new(20.0, y), Vec2::new(face.width() * w, 5.0)),
                egui::CornerRadius::same(3),
                c,
            );
        };
        bar(7.0, 0.45, Color32::from_rgb(0x5b, 0x8d, 0xef));
        bar(16.0, 0.3, paint::alpha(s.ink, 0.35));
        let well = Rect::from_min_max(face.min + Vec2::new(8.0, 30.0), face.max - Vec2::new(8.0, 7.0));
        paint::oled_well(&p, well, &s);
    }
}

/// Eight bars of a demo signal, as (level, hold, rms) in dB.
fn demo_levels(t: f64) -> Vec<(f32, f32, f32)> {
    (0..8)
        .map(|i| {
            let i = i as f64;
            let v =
                0.55 + 0.3 * (t * (1.3 + i * 0.21) + i * 1.7).sin() * (t * 0.37 + i).sin() + 0.1 * (t * 7.0 + i).sin();
            let db = (-60.0 + 60.0 * v.clamp(0.02, 1.0)) as f32;
            (db, (db + 5.0).min(0.0), db - 6.0)
        })
        .collect()
}

fn demo_groups() -> Vec<Group> {
    let chans = |word: &str| -> Vec<Chan> {
        (0..4).map(|i| Chan { number: i + 1, name: format!("{word} {}", i + 1), ..Chan::silent() }).collect()
    };
    vec![Group { label: "IN 4".into(), channels: chans("In") }, Group { label: "OUT 4".into(), channels: chans("Out") }]
}

fn meter_tiles(ui: &mut egui::Ui, pane: Rect, look: &mut MeterLook, skin: &GearSkin) {
    let size = Vec2::new(210.0, 160.0);
    let t = ui.input(|i| i.time);
    let groups = demo_groups();
    let levels = demo_levels(t);
    let ppp = ui.ctx().pixels_per_point();
    for (i, style) in MeterStyle::all().into_iter().enumerate() {
        let r = Rect::from_min_size(pane.min + Vec2::new(PANE_PAD + i as f32 * (size.x + 14.0), 52.0), size);
        if tile(ui, r, Id::new(("meter-tile", i)), style.name(), look.style == style, skin).clicked() {
            look.style = style;
        }
        let well = Rect::from_min_max(r.min + Vec2::new(8.0, 8.0), Pos2::new(r.right() - 8.0, r.bottom() - 30.0));
        let p = ui.painter_at(well.expand(2.0));
        paint::oled_well(&p, well, skin);
        let inner = well.shrink2(Vec2::new(8.0, 6.0));
        if style == MeterStyle::DotMatrix {
            oled_meter::dot_grid(&p.with_clip_rect(well.shrink(2.0)), well.shrink(2.0), inner.height());
        }
        let geom = Geom { readout: false, ..Geom::bridge() }.resolve(style, ppp, inner.height());
        let geom = oled_meter::fit_geom(&groups, inner.width(), geom);
        let layout = oled_meter::meter_layout(&groups, inner, &geom);
        oled_meter::paint_meter(&p, &layout, &groups, &levels, MeterLook { style, ..*look });
    }
    // Peak line and clip, under the tiles.
    let row =
        Rect::from_min_size(pane.min + Vec2::new(PANE_PAD, 52.0 + size.y + 14.0), Vec2::new(640.0, paint::PILL_H));
    let mut child =
        ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)));
    child.spacing_mut().item_spacing.x = 6.0;
    let label = |ui: &mut egui::Ui, text: &str| {
        ui.label(egui::RichText::new(text).color(paint::alpha(skin.ground_ink, 0.7)));
    };
    label(&mut child, "Peak line");
    if paint::pill_lit(&mut child, "Single line", "Single line", !look.double_peak, skin).clicked() {
        look.double_peak = false;
    }
    if paint::pill_lit(&mut child, "Double line", "Double line", look.double_peak, skin).clicked() {
        look.double_peak = true;
    }
    child.add_space(18.0);
    label(&mut child, "Clip");
    if paint::pill_lit(&mut child, "White", "White", !look.clip_red, skin).clicked() {
        look.clip_red = false;
    }
    if paint::pill_lit(&mut child, "Red", "Red", look.clip_red, skin).clicked() {
        look.clip_red = true;
    }
}

fn names_pane(ui: &mut egui::Ui, r: Rect, prefs: &mut ViewPrefs, skin: &GearSkin) {
    let sw = Rect::from_min_size(r.min + Vec2::new(PANE_PAD, 52.0), Vec2::new(r.width() - 2.0 * PANE_PAD, 44.0));
    paint::switch(
        ui,
        sw,
        &mut prefs.only_custom_names,
        "Show only custom names",
        "Hide the device's own name where you've named it.",
        skin,
    );
    let sw2 = sw.translate(Vec2::new(0.0, 48.0));
    paint::switch(
        ui,
        sw2,
        &mut prefs.short_bay_titles,
        "Short bay titles",
        "One word over each group of devices (Hardware, Windows, Virtual\u{2026}).",
        skin,
    );
    // A mini card: its device-name line goes when only custom names show.
    let card = Rect::from_min_size(r.min + Vec2::new(PANE_PAD, 154.0), Vec2::new(190.0, 54.0));
    let p = ui.painter();
    paint::panel(p, card, skin, None);
    let ink = skin.ink;
    p.text(
        card.min + Vec2::new(10.0, 12.0),
        Align2::LEFT_CENTER,
        "VASIO A",
        paint::font(ui.ctx(), "label-bold", 10.0),
        paint::alpha(ink, 0.7),
    );
    p.text(
        card.min + Vec2::new(10.0, 28.0),
        Align2::LEFT_CENTER,
        "Ableton",
        paint::font(ui.ctx(), "label-bold", 13.5),
        ink,
    );
    if !prefs.only_custom_names {
        p.text(
            card.min + Vec2::new(10.0, 43.0),
            Align2::LEFT_CENTER,
            "Confluence VASIO A",
            paint::font(ui.ctx(), "label", 10.5),
            paint::alpha(ink, 0.55),
        );
    }
}

fn motion_pane(ui: &mut egui::Ui, r: Rect, reduce: &mut bool, skin: &GearSkin) {
    let sw = Rect::from_min_size(r.min + Vec2::new(PANE_PAD, 52.0), Vec2::new(r.width() - 2.0 * PANE_PAD, 44.0));
    paint::switch(ui, sw, reduce, "Reduce motion", "Changes land at once. Meters keep their timing.", skin);
    // A dot that glides, and stops when motion is reduced.
    let track = Rect::from_min_size(r.min + Vec2::new(PANE_PAD, 110.0), Vec2::new(r.width() - 2.0 * PANE_PAD, 36.0));
    let p = ui.painter();
    p.rect_filled(track, egui::CornerRadius::same(8), paint::alpha(Color32::BLACK, 0.18));
    let t = ui.input(|i| i.time) as f32;
    let span = track.width() - 36.0;
    let x = if *reduce { span } else { span * (0.5 + 0.5 * (t * 2.0).sin()) };
    let c = Pos2::new(track.left() + 18.0 + x, track.center().y);
    p.circle_filled(c, 9.0, paint::alpha(GOLD, 0.25));
    p.circle_filled(c, 7.0, GOLD);
}
