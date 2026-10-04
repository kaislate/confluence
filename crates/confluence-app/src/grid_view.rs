//! Drawing the matrix (only visible cells) with sticky slot headers, and
//! turning pointer input on cells into edits. Each visible cell is an
//! accessibility node labelled `‹slot› in n → ‹slot› out m`.

use confluence_api::PointState;
use eframe::egui::{self, Align2, Color32, FontId, Id, Pos2, Rect, ScrollArea, Sense, Vec2, WidgetInfo, WidgetType};

use crate::commands::Edit;
use crate::matrix::{cell_edit, CellInput, DeferredUnroute, GridLayout, Selection};
use crate::skin::Look;

/// Width of the row headers and height of the column headers.
pub const HEADER_W: f32 = 160.0;
pub const HEADER_H: f32 = 44.0;
const BAND_STRIP: f32 = 20.0;

#[derive(Default)]
pub struct GridActions {
    pub edits: Vec<Edit>,
    pub select: Option<Selection>,
    /// A new cell size (Ctrl+wheel).
    pub zoom: Option<f32>,
}

/// Pixels of touchpad (point-unit) scrolling per gain step.
pub const POINTS_PER_NOTCH: f32 = 50.0;

/// Gain steps for one wheel event: a mouse notch is one step, a touchpad
/// moves in fractions of one, a page is three.
pub fn notches(unit: egui::MouseWheelUnit, dy: f32) -> f32 {
    match unit {
        egui::MouseWheelUnit::Line => dy,
        egui::MouseWheelUnit::Point => dy / POINTS_PER_NOTCH,
        egui::MouseWheelUnit::Page => dy * 3.0,
    }
}

fn wheel_notches(ui: &egui::Ui) -> f32 {
    ui.input(|i| {
        i.events
            .iter()
            .map(|e| match e {
                egui::Event::MouseWheel { unit, delta, .. } => notches(*unit, delta.y),
                _ => 0.0,
            })
            .sum()
    })
}

fn tooltip(label: &str, p: Option<&PointState>) -> String {
    match p {
        None => format!("{label}: no route"),
        Some(p) => format!(
            "{label}: {:+.1} dB{}{}",
            p.gain_db,
            if p.mute { ", muted" } else { "" },
            if p.invert { ", inverted" } else { "" }
        ),
    }
}

/// Draws the grid; `lookup` gives a point's shown route and whether it is pending.
pub fn show(
    ui: &mut egui::Ui,
    layout: &GridLayout,
    look: &Look,
    lookup: &dyn Fn((u32, u32)) -> (Option<PointState>, bool),
    selected: Option<(u32, u32)>,
    editable: bool,
    unroute: &mut DeferredUnroute,
) -> GridActions {
    let now = ui.input(|i| i.time);
    let mut actions = GridActions::default();
    let (ctrl, fine) = ui.input(|i| (i.modifiers.ctrl, i.modifiers.shift));
    if ctrl && ui.ui_contains_pointer() {
        let n = wheel_notches(ui);
        if n != 0.0 {
            actions.zoom = Some(layout.cell + n.signum() * 2.0);
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
    }
    let (gw, gh) = layout.size();
    let cell = layout.cell;
    ScrollArea::both().auto_shrink(false).id_salt("matrix").show_viewport(ui, |ui, viewport| {
        let (outer, _) = ui.allocate_exact_size(Vec2::new(HEADER_W + gw, HEADER_H + gh), Sense::hover());
        let origin = outer.min + Vec2::new(HEADER_W, HEADER_H);
        let (rows, cols) =
            layout.visible(viewport.min.x, viewport.min.y, viewport.max.x - HEADER_W, viewport.max.y - HEADER_H);
        // The part of the screen where cells show: below and right of the sticky headers.
        let cell_area = Rect::from_min_max(
            outer.min + viewport.min.to_vec2() + Vec2::new(HEADER_W, HEADER_H),
            outer.min + viewport.max.to_vec2(),
        );

        for r in rows.clone() {
            for c in cols.clone() {
                let Some(p) = layout.point(r, c) else { continue };
                // Strings are built only when asked for: the label when the
                // accessibility tree is active, the tooltip on hover.
                let label = || layout.label(r, c).unwrap_or_default();
                let rect = Rect::from_min_size(origin + Vec2::new(c as f32 * cell, r as f32 * cell), Vec2::splat(cell));
                // Only the visible part of a cell takes the pointer: a cell scrolled
                // under a header must not take clicks meant for the header.
                let hit = rect.intersect(cell_area);
                if !hit.is_positive() {
                    continue;
                }
                // Read-only (no engine): a click still selects, to inspect the cell.
                let sense = if editable { Sense::click_and_drag() } else { Sense::click() };
                let resp = ui.interact(hit, Id::new(("cell", p)), sense);
                resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, editable, label()));
                let (cur, pending) = lookup(p);
                let online = layout.rows.at(r).is_some_and(|(b, _)| b.online)
                    && layout.cols.at(c).is_some_and(|(b, _)| b.online);
                look.paint_cell(ui.painter(), rect, cur.as_ref(), pending, selected == Some(p), !online || !editable);
                let resp = resp.on_hover_ui(|ui| {
                    ui.label(tooltip(&label(), cur.as_ref()));
                });
                if !editable {
                    if resp.clicked() {
                        actions.select = Some(Selection::Cell { input: p.0, output: p.1 });
                    }
                    continue;
                }
                let mut wheel = 0.0;
                if resp.hovered() && cur.is_some() && !ctrl {
                    wheel = wheel_notches(ui);
                    // egui spreads a wheel notch's scrolling over several frames:
                    // absorb it on every frame the pointer stays on the route, so
                    // the grid never moves another cell under the pointer.
                    ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                }
                let input = CellInput {
                    clicked: resp.clicked(),
                    double_clicked: resp.double_clicked(),
                    drag_dy: if resp.dragged() { resp.drag_delta().y } else { 0.0 },
                    wheel_notches: wheel,
                    fine,
                };
                if input.double_clicked {
                    unroute.double(p); // the first click's unroute must not happen
                    actions.edits.extend(cell_edit(p, cur.as_ref(), &input));
                } else if input.clicked && cur.is_some() {
                    // Unrouted once the double-click window passes (DeferredUnroute).
                    if let Some(q) = unroute.click(p, now) {
                        actions.edits.push(Edit::RemovePoint { input: q.0, output: q.1 });
                    }
                } else if let Some(e) = cell_edit(p, cur.as_ref(), &input) {
                    actions.edits.push(e);
                }
                if resp.clicked() || resp.drag_started() {
                    actions.select = Some(Selection::Cell { input: p.0, output: p.1 });
                }
                resp.context_menu(|ui| {
                    let set = |gain_db: f32, mute: bool, invert: bool| Edit::SetPoint {
                        input: p.0,
                        output: p.1,
                        gain_db,
                        mute,
                        invert,
                    };
                    match &cur {
                        Some(pt) => {
                            if ui.button(if pt.mute { "Unmute" } else { "Mute" }).clicked() {
                                actions.edits.push(set(pt.gain_db, !pt.mute, pt.invert));
                                ui.close();
                            }
                            if ui.button(if pt.invert { "Normal phase" } else { "Invert" }).clicked() {
                                actions.edits.push(set(pt.gain_db, pt.mute, !pt.invert));
                                ui.close();
                            }
                            if ui.button("Set 0 dB").clicked() {
                                actions.edits.push(set(0.0, pt.mute, pt.invert));
                                ui.close();
                            }
                            if ui.button("Remove").clicked() {
                                actions.edits.push(Edit::RemovePoint { input: p.0, output: p.1 });
                                ui.close();
                            }
                        }
                        None => {
                            if ui.button("Route at 0 dB").clicked() {
                                actions.edits.push(set(0.0, false, false));
                                ui.close();
                            }
                        }
                    }
                });
            }
        }

        // Sticky headers, drawn over the cells at the viewport's edges.
        let vp = Rect::from_min_size(outer.min + viewport.min.to_vec2(), viewport.size());
        let bg = ui.visuals().panel_fill;
        let text = ui.visuals().text_color();
        let top =
            Rect::from_min_max(Pos2::new(vp.min.x + HEADER_W, vp.min.y), Pos2::new(vp.max.x, vp.min.y + HEADER_H));
        let left =
            Rect::from_min_max(Pos2::new(vp.min.x, vp.min.y + HEADER_H), Pos2::new(vp.min.x + HEADER_W, vp.max.y));
        ui.painter_at(top).rect_filled(top, 0.0, bg);
        ui.painter_at(left).rect_filled(left, 0.0, bg);
        let font = FontId::proportional(12.0);

        for band in &layout.cols.bands {
            let x0 = origin.x + band.start as f32 * cell;
            let strip =
                Rect::from_min_size(Pos2::new(x0, top.min.y), Vec2::new(band.channels as f32 * cell, BAND_STRIP));
            let colour = if band.online { look.slot(band.slot) } else { look.skin.colors.offline };
            let painter = ui.painter_at(strip.intersect(top));
            look.paint_band(&painter, strip.shrink2(Vec2::new(1.0, 2.0)), colour);
            let name = if band.online { band.name.clone() } else { format!("{} OFFLINE", band.name) };
            painter.text(
                strip.left_center() + Vec2::new(4.0, 0.0),
                Align2::LEFT_CENTER,
                name,
                font.clone(),
                Color32::BLACK,
            );
            let hit = strip.intersect(top);
            if hit.is_positive() {
                let resp = ui.interact(hit, Id::new(("band-out", band.slot)), Sense::click());
                resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} outputs", band.name)));
                if resp.clicked() {
                    actions.select = Some(Selection::Slot(band.slot));
                }
            }
            for k in 0..band.channels {
                let x = x0 + (k as f32 + 0.5) * cell;
                if x > top.min.x && x < top.max.x {
                    ui.painter_at(top).text(
                        Pos2::new(x, top.min.y + BAND_STRIP + 11.0),
                        Align2::CENTER_CENTER,
                        (k + 1).to_string(),
                        font.clone(),
                        text,
                    );
                }
            }
        }

        for band in &layout.rows.bands {
            let y0 = origin.y + band.start as f32 * cell;
            let strip =
                Rect::from_min_size(Pos2::new(left.min.x, y0), Vec2::new(HEADER_W, band.channels as f32 * cell));
            let colour = if band.online { look.slot(band.slot) } else { look.skin.colors.offline };
            let painter = ui.painter_at(strip.intersect(left));
            look.paint_band(
                &painter,
                Rect::from_min_size(strip.min, Vec2::new(6.0, strip.height())).shrink2(Vec2::new(0.0, 1.0)),
                colour,
            );
            let first_visible = y0.max(left.min.y);
            let name = if band.online { band.name.clone() } else { format!("{} OFFLINE", band.name) };
            painter.text(
                Pos2::new(strip.min.x + 10.0, first_visible + cell / 2.0),
                Align2::LEFT_CENTER,
                name,
                font.clone(),
                text,
            );
            let hit = strip.intersect(left);
            if hit.is_positive() {
                let resp = ui.interact(hit, Id::new(("band-in", band.slot)), Sense::click());
                resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} inputs", band.name)));
                if resp.clicked() {
                    actions.select = Some(Selection::Slot(band.slot));
                }
            }
            for k in 0..band.channels {
                let y = y0 + (k as f32 + 0.5) * cell;
                if y > left.min.y && y < left.max.y {
                    painter.text(
                        Pos2::new(left.max.x - 6.0, y),
                        Align2::RIGHT_CENTER,
                        format!("in {}", k + 1),
                        font.clone(),
                        text,
                    );
                }
            }
        }
        let corner = Rect::from_min_size(vp.min, Vec2::new(HEADER_W, HEADER_H));
        ui.painter_at(corner).rect_filled(corner, 0.0, bg);
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::MouseWheelUnit;

    #[test]
    fn wheel_units_become_gain_steps() {
        assert_eq!(notches(MouseWheelUnit::Line, -1.0), -1.0, "a mouse notch is one step");
        assert_eq!(notches(MouseWheelUnit::Point, 25.0), 0.5, "a touchpad moves in fractions");
        let swipe: f32 = (0..10).map(|_| notches(MouseWheelUnit::Point, 5.0)).sum();
        assert!((swipe - 1.0).abs() < 1e-6, "50 px of touchpad is one step: {swipe}");
        assert_eq!(notches(MouseWheelUnit::Page, 1.0), 3.0);
    }
}
