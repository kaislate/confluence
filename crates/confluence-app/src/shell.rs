//! The rack's own hardware around the screens: the rail along the top (the
//! wordmark, the Matrix | Devices toggle, the engine readout), the toasts,
//! and the powered-off face shown while no engine is running.

use std::collections::HashMap;
use std::time::Instant;

use confluence_api::EngineStatus;
use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, Sense, Vec2, WidgetInfo, WidgetType};

use crate::app::Screen;
use crate::gear::motion::{Curve, Motion, ENTER, PHOSPHOR_TAU};
use crate::gear::paint;
use crate::gear::skins::{GearSkin, AMBER, GREEN, RED};
use crate::notify::{Note, INFO_FOR};

/// Heights of the rail and the scene rail.
pub const RAIL_H: f32 = 48.0;
pub const SCENE_RAIL_H: f32 = 40.0;

/// What the rail's LED says about the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Light {
    /// Green, steady.
    Live,
    /// Amber, blinking at 1 Hz.
    Connecting,
    /// Red, blinking at 2 Hz.
    NotResponding,
}

/// The LED for the connection badge's text.
pub fn light_for(badge: &str) -> Light {
    match badge {
        "Live" => Light::Live,
        "Not responding" => Light::NotResponding,
        _ => Light::Connecting,
    }
}

/// The engine readout: "48K · 256 · GOXLR ASIO DRIVER".
pub fn readout(s: &EngineStatus) -> String {
    let master = s.master.split_once(':').map_or(s.master.as_str(), |(_, m)| m);
    // "GoXLR ASIO Driver" is "GOXLR": the driver words are noise here.
    let master = if master == "internal" {
        "INT CLOCK".to_string()
    } else {
        let words: Vec<&str> = master
            .split_whitespace()
            .filter(|w| !matches!(w.to_ascii_uppercase().as_str(), "ASIO" | "DRIVER"))
            .collect();
        if words.is_empty() {
            master.to_uppercase()
        } else {
            words.join(" ").to_uppercase()
        }
    };
    let master: String = master.chars().take(14).collect();
    let k = s.sample_rate / 1000.0;
    let rate = if (k - k.round()).abs() < 0.05 { format!("{k:.0}K") } else { format!("{k:.1}K") };
    format!("{rate} \u{b7} {} \u{b7} {master}", s.block)
}

/// Paints the rail's face over `r`: the ground with a brushed strip and a
/// seam along its bottom.
pub fn rail_face(p: &egui::Painter, r: Rect, s: &GearSkin) {
    p.rect_filled(r, egui::CornerRadius::ZERO, s.ground);
    paint::grain(p, r, s, s.grain * 1.4);
    // A brushed strip: faint horizontal streaks.
    let mut y = r.top() + 3.0;
    let mut k: u32 = 7;
    while y < r.bottom() - 3.0 {
        k = k.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let a = ((k >> 16) & 0x1f) as f32 / 31.0;
        let light = if s.light() {
            Color32::from_white_alpha((a * 24.0) as u8)
        } else {
            Color32::from_white_alpha((a * 10.0) as u8)
        };
        p.line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], egui::Stroke::new(1.0, light));
        y += 2.0;
    }
    paint::fade(
        p,
        Rect::from_min_size(r.min, Vec2::new(r.width(), r.height() * 0.5)),
        Color32::from_white_alpha((s.etch * 40.0) as u8),
        Color32::TRANSPARENT,
        true,
    );
    paint::seam(p, Pos2::new(r.left(), r.bottom() - 2.0), Pos2::new(r.right(), r.bottom() - 2.0), s);
}

/// The etched wordmark.
pub fn wordmark(ui: &mut egui::Ui, s: &GearSkin) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(118.0, RAIL_H - 8.0), Sense::hover());
    paint::etched_text(
        ui.painter(),
        r.left_center() + Vec2::new(4.0, 0.0),
        Align2::LEFT_CENTER,
        "CONFLUENCE",
        s,
        s.ground_ink,
        13.0,
        true,
        0.22,
        0.9,
    );
}

/// What a window button asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAction {
    Minimize,
    /// Maximize, or restore when maximized.
    ToggleMaximized,
    Close,
}

/// The window's own minimize, maximize/restore and close buttons, drawn in
/// the app's style (the native title bar is off). Add them to a
/// right-to-left row: close ends up rightmost.
pub fn window_buttons(ui: &mut egui::Ui, s: &GearSkin, maximized: bool) -> Option<WindowAction> {
    let mut out = None;
    let ink = s.ground_ink;
    let red = Color32::from_rgb(0xe8, 0x3b, 0x3b);
    let buttons = [
        (WindowAction::Close, "Close"),
        (WindowAction::ToggleMaximized, if maximized { "Restore" } else { "Maximize" }),
        (WindowAction::Minimize, "Minimize"),
    ];
    for (action, label) in buttons {
        let (r, resp) = ui.allocate_exact_size(Vec2::new(36.0, 28.0), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
        let p = ui.painter();
        let hover = resp.hovered();
        if hover {
            let fill = if action == WindowAction::Close { red } else { paint::alpha(ink, 0.12) };
            p.rect_filled(r, egui::CornerRadius::same(7), fill);
        }
        let c = r.center();
        let stroke = egui::Stroke::new(1.4, if hover && action == WindowAction::Close { Color32::WHITE } else { ink });
        match action {
            WindowAction::Minimize => {
                p.line_segment([c + Vec2::new(-5.0, 0.5), c + Vec2::new(5.0, 0.5)], stroke);
            }
            WindowAction::ToggleMaximized if maximized => {
                let back = Rect::from_center_size(c + Vec2::new(1.5, -1.5), Vec2::splat(8.0));
                let front = Rect::from_center_size(c + Vec2::new(-1.0, 1.0), Vec2::splat(8.0));
                p.rect_stroke(back, egui::CornerRadius::same(1), stroke, egui::StrokeKind::Middle);
                p.rect_filled(front, egui::CornerRadius::same(1), s.ground);
                p.rect_stroke(front, egui::CornerRadius::same(1), stroke, egui::StrokeKind::Middle);
            }
            WindowAction::ToggleMaximized => {
                let sq = Rect::from_center_size(c, Vec2::splat(10.0));
                p.rect_stroke(sq, egui::CornerRadius::same(1), stroke, egui::StrokeKind::Middle);
            }
            WindowAction::Close => {
                p.line_segment([c + Vec2::new(-5.0, -5.0), c + Vec2::new(5.0, 5.0)], stroke);
                p.line_segment([c + Vec2::new(-5.0, 5.0), c + Vec2::new(5.0, -5.0)], stroke);
            }
        }
        if resp.clicked() {
            out = Some(action);
        }
    }
    out
}

/// The resize direction for pointer `p` within `band` points of the
/// window's edges (corners win), or `None` away from them.
pub fn resize_dir(window: Rect, p: Pos2, band: f32) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection as D;
    if !window.contains(p) {
        return None;
    }
    let (w, e) = (p.x - window.left() < band, window.right() - p.x < band);
    let (n, s) = (p.y - window.top() < band, window.bottom() - p.y < band);
    match (n, s, w, e) {
        (true, _, true, _) => Some(D::NorthWest),
        (true, _, _, true) => Some(D::NorthEast),
        (_, true, true, _) => Some(D::SouthWest),
        (_, true, _, true) => Some(D::SouthEast),
        (true, ..) => Some(D::North),
        (_, true, ..) => Some(D::South),
        (_, _, true, _) => Some(D::West),
        (_, _, _, true) => Some(D::East),
        _ => None,
    }
}

/// Resizing from the window's edges (the native frame is off): the cursor
/// shows the direction, and a press starts the system resize. Not while
/// maximized. Call once per frame.
pub fn edge_resize(ctx: &egui::Context) {
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    let window = ctx.content_rect();
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    // A floating window (Scripts, a dialog) dragged to the edge keeps its own
    // grips and scrollbars; nothing mid-drag is interrupted either.
    let over_window = ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background);
    if !edge_resize_allowed(maximized, over_window, ctx.dragged_id().is_some()) {
        return;
    }
    let Some(dir) = resize_dir(window, pos, RESIZE_BAND) else { return };
    use egui::ResizeDirection as D;
    ctx.set_cursor_icon(match dir {
        D::North | D::South => egui::CursorIcon::ResizeVertical,
        D::East | D::West => egui::CursorIcon::ResizeHorizontal,
        D::NorthWest | D::SouthEast => egui::CursorIcon::ResizeNwSe,
        D::NorthEast | D::SouthWest => egui::CursorIcon::ResizeNeSw,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
    }
}

/// Whether the window's edges resize it now: not while maximized, over a
/// floating window, or while something is being dragged.
pub fn edge_resize_allowed(maximized: bool, over_window: bool, dragging: bool) -> bool {
    !maximized && !over_window && !dragging
}

/// Whether a press at `press` on the rail may move the window: only from
/// empty rail, not from its controls (`blocked`: their rects) nor the resize
/// band. A slow click on a pill must stay a click.
pub fn may_drag_window(press: Pos2, blocked: &[Rect], at_edge: bool) -> bool {
    !at_edge && !blocked.iter().any(|r| r.contains(press))
}

/// How close to the window's edge a press resizes it.
pub const RESIZE_BAND: f32 = 6.0;

/// The Matrix | Devices | Settings toggle: pills in one recessed housing.
pub fn segmented(ui: &mut egui::Ui, s: &GearSkin, screen: &mut Screen) {
    let (housing, _) = ui.allocate_exact_size(Vec2::new(4.0, paint::PILL_H + 8.0), Sense::hover());
    let labels = [(Screen::Matrix, "Matrix"), (Screen::Devices, "Devices"), (Screen::Settings, "Settings")];
    let widths: Vec<f32> = labels
        .iter()
        .map(|(_, l)| {
            ui.painter().layout_no_wrap(l.to_string(), paint::font(ui.ctx(), "label-bold", 12.0), s.ink).size().x + 18.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0 * (labels.len() as f32 + 1.0);
    let r = Rect::from_min_size(housing.min, Vec2::new(total, housing.height()));
    ui.allocate_rect(r, Sense::hover());
    paint::recess(ui.painter(), r, s, 16);
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(r.shrink2(Vec2::new(6.0, 4.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    inner.spacing_mut().item_spacing.x = 6.0;
    for (which, label) in labels {
        let lit = *screen == which;
        let resp = paint::pill_lit(&mut inner, label, label, lit, s);
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, lit, label));
        if resp.clicked() {
            *screen = which;
        }
    }
}

/// The engine cluster: a status LED with its word, the readout OLED, a DSP
/// meter and the xrun counter.
#[allow(clippy::too_many_arguments)]
pub fn engine_cluster(
    ui: &mut egui::Ui,
    s: &GearSkin,
    motion: &mut Motion,
    badge: &str,
    status: Option<&EngineStatus>,
    xrun_flash: bool,
    dsp_warn: Option<Color32>,
) {
    let light = light_for(badge);
    let (colour, on) = match light {
        Light::Live => (GREEN, true),
        Light::Connecting => (AMBER, motion.blink(1.0)),
        Light::NotResponding => (RED, motion.blink(2.0)),
    };
    let glow = motion.phosphor(Id::new("rail-led"), if on { 1.0 } else { 0.0 }, PHOSPHOR_TAU);
    let (led_r, led) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::hover());
    paint::led_glow(ui.painter(), led_r.center(), s, colour, glow);
    led.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, badge));
    let text_w =
        ui.painter().layout_no_wrap(badge.to_string(), paint::font(ui.ctx(), "label-bold", 11.0), s.ink).size().x + 4.0;
    let (tr, _) = ui.allocate_exact_size(Vec2::new(text_w.max(30.0), 18.0), Sense::hover());
    paint::etched_text(
        ui.painter(),
        tr.left_center(),
        Align2::LEFT_CENTER,
        badge,
        s,
        s.ground_ink,
        11.0,
        true,
        0.06,
        0.85,
    );
    let Some(st) = status else { return };
    ui.add_space(6.0);
    let (oled, _) = ui.allocate_exact_size(Vec2::new(236.0, 26.0), Sense::hover());
    paint::oled_line(ui.painter(), oled.shrink2(Vec2::new(0.0, 1.0)), s, &readout(st), 20.0, s.oled);
    ui.add_space(8.0);
    // DSP: a small horizontal meter.
    let (dr, dsp) = ui.allocate_exact_size(Vec2::new(64.0, 18.0), Sense::hover());
    let label = format!("DSP {:.0}%", st.dsp_load * 100.0);
    dsp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    dsp.on_hover_text(&label);
    paint::etched_text(
        ui.painter(),
        dr.left_center(),
        Align2::LEFT_CENTER,
        "DSP",
        s,
        s.ground_ink,
        9.5,
        true,
        0.1,
        0.6,
    );
    let bar = Rect::from_min_size(Pos2::new(dr.left() + 26.0, dr.center().y - 4.0), Vec2::new(dr.width() - 26.0, 8.0));
    ui.painter().rect_filled(bar, egui::CornerRadius::same(2), s.bed);
    let load = motion.decay(Id::new("rail-dsp"), st.dsp_load.clamp(0.0, 1.0), 0.25);
    let fill = Rect::from_min_size(bar.min, Vec2::new(bar.width() * load, bar.height()));
    ui.painter().rect_filled(fill, egui::CornerRadius::same(2), dsp_warn.unwrap_or(GREEN));
    ui.add_space(8.0);
    let (xr, x) = ui.allocate_exact_size(Vec2::new(44.0, 18.0), Sense::hover());
    let label = format!("Xruns {}", st.xruns);
    x.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    let ink = if xrun_flash && motion.blink(4.0) { RED } else { s.ground_ink };
    paint::etched_text(
        ui.painter(),
        xr.left_center(),
        Align2::LEFT_CENTER,
        &format!("XR {}", st.xruns),
        s,
        ink,
        10.0,
        true,
        0.08,
        if xrun_flash { 1.0 } else { 0.6 },
    );
}

/// Toasts at the bottom right: info notes slide up and fade, errors stay
/// with a dismiss button; engine notices toast once. Returns a dismissed id.
pub fn toasts(
    ctx: &egui::Context,
    s: &GearSkin,
    motion: &mut Motion,
    notes: &[&Note],
    notices: &[String],
    notices_seen: &mut HashMap<String, Instant>,
    now: Instant,
) -> Option<u64> {
    let mut dismiss = None;
    for n in notices {
        notices_seen.entry(n.clone()).or_insert(now);
    }
    notices_seen.retain(|n, _| notices.contains(n));
    let fresh: Vec<(String, Instant)> = notices_seen
        .iter()
        .filter(|(_, at)| now.saturating_duration_since(**at) < INFO_FOR)
        .map(|(n, at)| (n.clone(), *at))
        .collect();
    if notes.is_empty() && fresh.is_empty() {
        return None;
    }
    egui::Area::new(Id::new("notifications"))
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -(SCENE_RAIL_H + 12.0)])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let mut rows: Vec<(Id, String, bool, Option<u64>, Instant)> = Vec::new();
            for (n, at) in &fresh {
                rows.push((Id::new(("notice", n)), n.clone(), false, None, *at));
            }
            for note in notes.iter().rev() {
                let text =
                    if note.count > 1 { format!("{} (\u{d7}{})", note.text, note.count) } else { note.text.clone() };
                rows.push((Id::new(("note", note.id)), text, note.error, note.error.then_some(note.id), note.at));
            }
            for (id, text, error, dismissable, at) in rows {
                let age = now.saturating_duration_since(at).as_secs_f32();
                let enter = motion.tween_from(id.with("in"), 0.0, 1.0, Curve::Enter, 0.18);
                let leaving = if error { 0.0 } else { ((age - (INFO_FOR.as_secs_f32() - 0.2)) / 0.2).clamp(0.0, 1.0) };
                let alpha = enter * (1.0 - Curve::Exit.at(leaving));
                if !error && age > INFO_FOR.as_secs_f32() - 0.3 && leaving < 1.0 {
                    motion.wake(); // frames for the fade-out
                }
                ui.set_opacity(alpha);
                let rise = 12.0 * (1.0 - enter);
                let width = 320.0;
                let (r, _) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::hover());
                let r = r.translate(Vec2::new(0.0, rise));
                let p = ui.painter_at(r.expand(40.0));
                paint::floating(&p, r, s, 10);
                let led_at = r.left_center() + Vec2::new(16.0, 0.0);
                paint::led_glow(&p, led_at, s, if error { RED } else { AMBER }, 1.0);
                let text_w = r.width() - 40.0 - if dismissable.is_some() { 30.0 } else { 0.0 };
                let lr = Rect::from_min_size(r.min + Vec2::new(32.0, 0.0), Vec2::new(text_w, r.height()));
                let label = ui.allocate_rect(lr, Sense::hover());
                label.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &text));
                paint::truncated(
                    &p,
                    lr.left_center(),
                    Align2::LEFT_CENTER,
                    &text,
                    paint::font(ui.ctx(), "label", 12.5),
                    s.ink,
                    lr.width(),
                );
                if let Some(nid) = dismissable {
                    let x = Rect::from_center_size(Pos2::new(r.right() - 18.0, r.center().y), Vec2::splat(20.0));
                    let b = ui.interact(x, id.with("dismiss"), Sense::click());
                    b.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Dismiss"));
                    paint::etched_text(
                        &p,
                        x.center(),
                        Align2::CENTER_CENTER,
                        "\u{2715}",
                        s,
                        s.ink,
                        12.0,
                        true,
                        0.0,
                        if b.hovered() { 1.0 } else { 0.6 },
                    );
                    if b.clicked() {
                        dismiss = Some(nid);
                    }
                }
            }
        });
    dismiss
}

/// The powered-off rack: shown in place of a screen while no engine runs.
/// Returns true when "Start engine" is pressed.
pub fn powered_off(ui: &mut egui::Ui, s: &GearSkin, motion: &mut Motion, ready: bool, reason: &str) -> bool {
    let r = ui.max_rect();
    paint::ground(ui.painter(), r, s);
    let c = r.center();
    let oled = Rect::from_center_size(c - Vec2::new(0.0, 30.0), Vec2::new(360.0, 56.0));
    let p = ui.painter();
    paint::oled_well(p, oled, s);
    let dim = motion.pulse(0.5) * 0.25 + 0.55;
    let f = paint::font(ui.ctx(), "oled", 28.0);
    let galley = p.layout_no_wrap(reason.to_uppercase(), f, paint::alpha(s.oled, dim));
    let at = (oled.center() - galley.size() / 2.0).round();
    for d in [Vec2::new(-1.0, 0.0), Vec2::new(1.0, 0.0)] {
        p.galley(at + d, galley.clone(), paint::alpha(s.oled, dim * 0.25));
    }
    p.galley(at, galley, paint::alpha(s.oled, dim));
    paint::led_glow(p, oled.left_center() - Vec2::new(24.0, 0.0), s, AMBER, 0.0);
    let label = ui.allocate_rect(oled, Sense::hover());
    label.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, reason));
    let row = Rect::from_center_size(c + Vec2::new(0.0, 30.0), Vec2::new(140.0, paint::PILL_H));
    let mut child =
        ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::top_down(egui::Align::Center)));
    child.add_enabled_ui(ready, |ui| paint::pill_labeled(ui, "Start engine", "Start engine", s)).inner.clicked()
        && ready
}

/// The screen-reveal: `ENTER` from a ground-coloured veil.
pub fn reveal(motion: &mut Motion, id: Id) -> f32 {
    motion.tween_from(id, 0.0, 1.0, Curve::Enter, ENTER * 0.7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_drags_only_from_empty_rail() {
        let pills = Rect::from_min_max(Pos2::new(800.0, 6.0), Pos2::new(1000.0, 40.0));
        let tabs = Rect::from_min_max(Pos2::new(140.0, 6.0), Pos2::new(340.0, 40.0));
        assert!(may_drag_window(Pos2::new(500.0, 20.0), &[pills, tabs], false), "empty rail");
        assert!(!may_drag_window(Pos2::new(900.0, 20.0), &[pills, tabs], false), "a long press on a pill");
        assert!(!may_drag_window(Pos2::new(200.0, 20.0), &[pills, tabs], false), "on the tabs");
        assert!(!may_drag_window(Pos2::new(500.0, 2.0), &[pills, tabs], true), "on the resize band");
    }

    #[test]
    fn edge_resizing_is_only_for_the_apps_own_background() {
        assert!(edge_resize_allowed(false, false, false));
        assert!(!edge_resize_allowed(true, false, false), "maximized");
        assert!(!edge_resize_allowed(false, true, false), "over a floating window (Scripts, a dialog)");
        assert!(!edge_resize_allowed(false, false, true), "while something is being dragged");
    }

    #[test]
    fn the_window_edges_and_corners_resize_it() {
        use egui::ResizeDirection as D;
        let w = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0));
        assert_eq!(resize_dir(w, Pos2::new(500.0, 2.0), 6.0), Some(D::North));
        assert_eq!(resize_dir(w, Pos2::new(500.0, 698.0), 6.0), Some(D::South));
        assert_eq!(resize_dir(w, Pos2::new(1.0, 300.0), 6.0), Some(D::West));
        assert_eq!(resize_dir(w, Pos2::new(997.0, 300.0), 6.0), Some(D::East));
        assert_eq!(resize_dir(w, Pos2::new(2.0, 2.0), 6.0), Some(D::NorthWest));
        assert_eq!(resize_dir(w, Pos2::new(998.0, 699.0), 6.0), Some(D::SouthEast));
        assert_eq!(resize_dir(w, Pos2::new(998.0, 1.0), 6.0), Some(D::NorthEast));
        assert_eq!(resize_dir(w, Pos2::new(1.0, 699.0), 6.0), Some(D::SouthWest));
        assert_eq!(resize_dir(w, Pos2::new(500.0, 300.0), 6.0), None, "inside");
        assert_eq!(resize_dir(w, Pos2::new(-5.0, 300.0), 6.0), None, "outside");
    }

    #[test]
    fn the_readout_is_short_and_loud() {
        let st = EngineStatus {
            master: "asio:GoXLR ASIO Driver".into(),
            sample_rate: 48_000.0,
            block: 256,
            blocks: 0,
            dsp_load: 0.0,
            xruns: 0,
        };
        assert_eq!(readout(&st), "48K \u{b7} 256 \u{b7} GOXLR");
        let internal = EngineStatus { master: "internal".into(), sample_rate: 44_100.0, ..st };
        assert_eq!(readout(&internal), "44.1K \u{b7} 256 \u{b7} INT CLOCK");
    }

    #[test]
    fn the_light_follows_the_badge() {
        assert_eq!(light_for("Live"), Light::Live);
        assert_eq!(light_for("Not responding"), Light::NotResponding);
        assert_eq!(light_for("Reconnecting (3 s)"), Light::Connecting);
        assert_eq!(light_for("Connecting\u{2026}"), Light::Connecting);
    }
}
