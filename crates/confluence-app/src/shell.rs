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
    let master = if master == "internal" { "INT CLOCK".to_string() } else { master.to_uppercase() };
    let master: String = master.chars().take(18).collect();
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

/// The Matrix | Devices toggle: two pills in one recessed housing.
pub fn segmented(ui: &mut egui::Ui, s: &GearSkin, screen: &mut Screen) {
    let (housing, _) = ui.allocate_exact_size(Vec2::new(4.0, paint::PILL_H + 8.0), Sense::hover());
    let labels = [(Screen::Matrix, "Matrix"), (Screen::Devices, "Devices")];
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
    fn the_readout_is_short_and_loud() {
        let st = EngineStatus {
            master: "asio:GoXLR ASIO Driver".into(),
            sample_rate: 48_000.0,
            block: 256,
            blocks: 0,
            dsp_load: 0.0,
            xruns: 0,
        };
        assert_eq!(readout(&st), "48K \u{b7} 256 \u{b7} GOXLR ASIO DRIVER");
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
