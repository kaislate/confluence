//! Drawing the matrix in the gear language: a recessed bed of pins, routed
//! crosspoints raised in their input band's colour with a gain badge, a
//! crosshair on the hovered row and column, and the device bands as colour
//! rails around the bed. Only visible cells are drawn; each is an
//! accessibility node labelled `‹slot› in n → ‹slot› out m`.

use std::collections::HashSet;

use confluence_api::PointState;
use eframe::egui::{
    self, Align2, Color32, FontId, Id, Pos2, Rect, ScrollArea, Sense, Stroke, StrokeKind, Vec2, WidgetInfo, WidgetType,
};
use egui::epaint::TextShape;

use crate::commands::Edit;
use crate::gear::motion::{Curve, Motion, POP, SETTLE};
use crate::gear::paint;
use crate::gear::skins::{self, GearSkin};
use crate::matrix::{cell_edit, Band, CellInput, GridLayout, Selection, SEP};
use crate::skin::Look;
use crate::theme;

/// Width of the row headers and height of the column headers: channel
/// names read across on the left and upward along the top.
pub const HEADER_W: f32 = 170.0;
pub const HEADER_H: f32 = 96.0;
/// Badges are drawn from this cell size up; below it gain shows as dot size.
pub const BADGE_FROM: f32 = 20.0;
/// Below this gain a route is a hollow ring rather than a darker square.
pub const RING_BELOW_DB: f32 = -40.0;

#[derive(Default)]
pub struct GridActions {
    pub edits: Vec<Edit>,
    pub select: Option<Selection>,
    /// A new cell size (Ctrl+wheel).
    pub zoom: Option<f32>,
}

/// What the grid remembers between frames.
#[derive(Default)]
pub struct GridState {
    /// The hovered cell (row, column).
    pub hover: Option<(usize, usize)>,
    /// Cells whose lift or pop is still moving.
    pub moving: HashSet<(u32, u32)>,
    /// The grid's scroll offset last frame.
    pub offset: Vec2,
    /// Where the minimap asked the grid to scroll (applied next frame).
    pub scroll_to: Option<Vec2>,
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

/// "From … / To … / state": the arrow of the accessible label is not in
/// every face, so the tooltip spells it out.
fn tooltip(label: &str, p: Option<&PointState>) -> String {
    let (from, to) = label.split_once(" \u{2192} ").unwrap_or((label, ""));
    let ends = if to.is_empty() { from.to_string() } else { format!("From {from}\nTo {to}") };
    match p {
        None => format!("{ends}\nno route"),
        Some(p) => format!(
            "{ends}\n{:+.1} dB{}{}",
            p.gain_db,
            if p.mute { ", muted" } else { "" },
            if p.invert { ", inverted" } else { "" }
        ),
    }
}

/// The gain badge: "0", "−6", "+3".
pub fn badge_text(db: f32) -> String {
    let n = db.round();
    if n == 0.0 || !n.is_finite() {
        "0".into()
    } else if n > 0.0 {
        format!("+{n:.0}")
    } else {
        format!("\u{2212}{:.0}", -n)
    }
}

/// The side of a routed square in a cell of `cell` px at `scale`.
pub fn routed_side(cell: f32, scale: f32) -> f32 {
    ((cell - 6.0).max(cell * 0.6)).max(6.0) * scale
}

/// The band's colour on screen (offline bands go grey).
fn band_colour(look: &Look, skin: &GearSkin, band: &Band) -> Color32 {
    if band.online {
        look.band(band)
    } else {
        skins::desaturate(paint::mix(skin.bed, skin.ground_ink, 0.35), 1.0)
    }
}

/// Draws the grid; `lookup` gives a point's shown route and whether it is
/// pending; `fresh` holds the routes that appeared this frame (they pop).
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    layout: &GridLayout,
    look: &Look,
    skin: &GearSkin,
    motion: &mut Motion,
    gs: &mut GridState,
    lookup: &dyn Fn((u32, u32)) -> (Option<PointState>, bool),
    selected: Option<(u32, u32)>,
    fresh: &HashSet<(u32, u32)>,
    routes: &HashSet<(u32, u32)>,
    editable: bool,
) -> GridActions {
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
    let skinned = look.has_image("cell_routed") || look.has_image("cell_empty");
    let mut hover_now: Option<(usize, usize)> = None;
    let blink = motion.blink(2.0);
    let mut area = ScrollArea::both().auto_shrink(false).id_salt("matrix");
    if let Some(o) = gs.scroll_to.take() {
        area = area.scroll_offset(o);
    }
    area.show_viewport(ui, |ui, viewport| {
        let (outer, _) = ui.allocate_exact_size(Vec2::new(HEADER_W + gw, HEADER_H + gh), Sense::hover());
        let origin = outer.min + Vec2::new(HEADER_W, HEADER_H);
        let (rows, cols) =
            layout.visible(viewport.min.x, viewport.min.y, viewport.max.x - HEADER_W, viewport.max.y - HEADER_H);
        // The part of the screen where cells show: below and right of the sticky headers.
        let cell_area = Rect::from_min_max(
            outer.min + viewport.min.to_vec2() + Vec2::new(HEADER_W, HEADER_H),
            outer.min + viewport.max.to_vec2(),
        );
        let grid = Rect::from_min_size(origin, Vec2::new(gw, gh));
        let bed = ui.painter_at(cell_area);
        paint::recess(&bed, grid, skin, 10);
        // Each device's separator strip runs across the bed, faintly in its colour.
        for (bi, b) in layout.rows.bands.iter().enumerate() {
            let y = origin.y + layout.rows.strip_pos(bi);
            let strip = Rect::from_min_size(Pos2::new(grid.left(), y), Vec2::new(gw, SEP));
            bed.rect_filled(strip, 0.0, paint::alpha(band_colour(look, skin, b), 0.10));
        }
        for (bi, b) in layout.cols.bands.iter().enumerate() {
            let x = origin.x + layout.cols.strip_pos(bi);
            let strip = Rect::from_min_size(Pos2::new(x, grid.top()), Vec2::new(SEP, gh));
            bed.rect_filled(strip, 0.0, paint::alpha(band_colour(look, skin, b), 0.10));
        }
        // The crosshair: the hovered (else selected) row and column, tinted.
        let focus = gs.hover.or_else(|| selected.and_then(|(i, o)| layout.cell_of(i, o)));
        if let Some((r, c)) = focus {
            let ty = motion.tween(Id::new("cross-row"), layout.rows.pos(r), Curve::Enter, 0.09);
            let tx = motion.tween(Id::new("cross-col"), layout.cols.pos(c), Curve::Enter, 0.09);
            let tint = paint::alpha(skin.ground_ink, 0.06);
            bed.rect_filled(Rect::from_min_size(Pos2::new(grid.left(), origin.y + ty), Vec2::new(gw, cell)), 0.0, tint);
            bed.rect_filled(Rect::from_min_size(Pos2::new(origin.x + tx, grid.top()), Vec2::new(cell, gh)), 0.0, tint);
        }
        let ink = skin.ground_ink;
        let badge_font = paint::font(ui.ctx(), "label-bold", 9.5);
        for r in rows.clone() {
            let row_band = layout.rows.at(r).map(|(b, _)| b);
            let row_colour = row_band.map(|b| band_colour(look, skin, b)).unwrap_or(ink);
            for c in cols.clone() {
                let Some(p) = layout.point(r, c) else { continue };
                // Strings are built only when asked for: the label when the
                // accessibility tree is active, the tooltip on hover.
                let label = || layout.label(r, c).unwrap_or_default();
                let (cx, cy) = layout.cell_pos(r, c);
                let rect = Rect::from_min_size(origin + Vec2::new(cx, cy), Vec2::splat(cell));
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
                let online = row_band.is_some_and(|b| b.online) && layout.cols.at(c).is_some_and(|(b, _)| b.online);
                let dim = !online || !editable;
                if resp.hovered() {
                    hover_now = Some((r, c));
                }
                // Motion: a hovered route lifts, a new one pops in.
                let is_fresh = fresh.contains(&p);
                let animate = resp.hovered() || is_fresh || gs.moving.contains(&p);
                let scale = if animate && cur.is_some() {
                    let lift =
                        motion.spring(Id::new(("cell-lift", p)), if resp.hovered() { 1.08 } else { 1.0 }, SETTLE);
                    let pop = if is_fresh {
                        motion.spring_from(Id::new(("cell-pop", p)), 0.6, 1.0, POP)
                    } else {
                        motion.spring(Id::new(("cell-pop", p)), 1.0, POP)
                    };
                    let s = lift * pop;
                    if resp.hovered() || (s - 1.0).abs() > 0.002 {
                        gs.moving.insert(p);
                    } else {
                        gs.moving.remove(&p);
                    }
                    s
                } else {
                    gs.moving.remove(&p);
                    1.0
                };
                if skinned {
                    look.paint_cell(ui.painter(), rect, cur.as_ref(), pending, selected == Some(p), dim);
                } else {
                    cell_face(&bed, rect, cur.as_ref(), pending, blink, dim, scale, row_colour, ink, &badge_font);
                    if selected == Some(p) {
                        bed.rect_stroke(
                            rect.shrink(0.5),
                            egui::CornerRadius::same(5),
                            Stroke::new(2.0, skin.accent),
                            StrokeKind::Inside,
                        );
                    }
                }
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
                if let Some(e) = cell_edit(p, cur.as_ref(), &input) {
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
        if layout.rows.len > 0 && layout.cols.len > 0 && !layout_has_routes(layout, lookup) {
            let hint = "No routes yet. Click a crosspoint and press Space, or double-click it.";
            let at = Pos2::new(cell_area.center().x, (grid.top() + 24.0).max(cell_area.top() + 24.0));
            paint::etched_text(&bed, at, Align2::CENTER_CENTER, hint, skin, ink, 12.0, false, 0.0, 0.55);
        }

        // Sticky headers, drawn over the cells at the viewport's edges.
        let vp = Rect::from_min_size(outer.min + viewport.min.to_vec2(), viewport.size());
        let top =
            Rect::from_min_max(Pos2::new(vp.min.x + HEADER_W, vp.min.y), Pos2::new(vp.max.x, vp.min.y + HEADER_H));
        let left =
            Rect::from_min_max(Pos2::new(vp.min.x, vp.min.y + HEADER_H), Pos2::new(vp.min.x + HEADER_W, vp.max.y));
        let tp = ui.painter_at(top);
        let lp = ui.painter_at(left);
        tp.rect_filled(top, 0.0, skin.ground);
        lp.rect_filled(left, 0.0, skin.ground);
        let number_font = paint::font(ui.ctx(), "label", 10.0);
        let name_font = paint::font(ui.ctx(), "label-bold", 11.0);
        let focus_col = focus.map(|(_, c)| c);
        let focus_row = focus.map(|(r, _)| r);

        // The outputs: each device's strip with its name reading upward, then
        // its channels' numbers and names, also reading upward.
        for (bi, band) in layout.cols.bands.iter().enumerate() {
            let x0 = origin.x + layout.cols.strip_pos(bi);
            let strip = Rect::from_min_size(Pos2::new(x0, top.min.y + 2.0), Vec2::new(SEP, HEADER_H - 6.0));
            let colour = band_colour(look, skin, band);
            let name = band_name(band);
            if strip.intersects(top) {
                let painter = ui.painter_at(top);
                if look.has_image("band") {
                    look.paint_band(&painter, strip.shrink2(Vec2::new(1.0, 0.0)), colour);
                } else {
                    paint::raised(&painter, strip.shrink2(Vec2::new(1.0, 0.0)), colour, 4.0);
                }
                rotated(
                    &painter,
                    Pos2::new(strip.center().x, strip.bottom() - 6.0),
                    &name,
                    name_font.clone(),
                    skins::ink_on(colour),
                    strip.height() - 12.0,
                );
                let hit = strip.intersect(top);
                if hit.is_positive() {
                    let resp = ui.interact(hit, Id::new(("band-out", band.slot)), Sense::click());
                    resp.widget_info(|| {
                        WidgetInfo::labeled(WidgetType::Button, true, format!("{} outputs", band.name))
                    });
                    if resp.clicked() {
                        actions.select = Some(Selection::Slot(band.slot));
                    }
                    resp.on_hover_text(format!("{name}: {} outputs", band.channels));
                }
            }
            for k in 0..band.channels {
                let idx = band.start + k as usize;
                let x = origin.x + layout.cols.pos(idx) + cell / 2.0;
                if x > top.min.x && x < top.max.x {
                    let lit = focus_col == Some(idx);
                    rotated(
                        &tp,
                        Pos2::new(x, top.max.y - 6.0),
                        &channel_text(band, k, if band.bus { "send" } else { "out" }),
                        number_font.clone(),
                        paint::alpha(ink, if lit { 1.0 } else { 0.6 }),
                        HEADER_H - 12.0,
                    );
                }
            }
        }

        // The inputs: each device's strip with its name across, then its
        // channels' numbers and names.
        for (bi, band) in layout.rows.bands.iter().enumerate() {
            let y0 = origin.y + layout.rows.strip_pos(bi);
            let strip = Rect::from_min_size(Pos2::new(left.min.x + 4.0, y0), Vec2::new(HEADER_W - 8.0, SEP));
            let colour = band_colour(look, skin, band);
            let name = band_name(band);
            if strip.intersects(left) {
                let painter = ui.painter_at(left);
                if look.has_image("band") {
                    look.paint_band(&painter, strip.shrink2(Vec2::new(0.0, 1.0)), colour);
                } else {
                    paint::raised(&painter, strip.shrink2(Vec2::new(0.0, 1.0)), colour, 4.0);
                }
                paint::truncated(
                    &painter,
                    Pos2::new(strip.left() + 8.0, strip.center().y),
                    Align2::LEFT_CENTER,
                    &name,
                    name_font.clone(),
                    skins::ink_on(colour),
                    strip.width() - 16.0,
                );
                let hit = strip.intersect(left);
                if hit.is_positive() {
                    let resp = ui.interact(hit, Id::new(("band-in", band.slot)), Sense::click());
                    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} inputs", band.name)));
                    if resp.clicked() {
                        actions.select = Some(Selection::Slot(band.slot));
                    }
                    resp.on_hover_text(format!("{name}: {} inputs", band.channels));
                }
            }
            for k in 0..band.channels {
                let idx = band.start + k as usize;
                let y = origin.y + layout.rows.pos(idx) + cell / 2.0;
                if y > left.min.y && y < left.max.y {
                    let lit = focus_row == Some(idx);
                    paint::truncated(
                        &lp,
                        Pos2::new(left.max.x - 8.0, y),
                        Align2::RIGHT_CENTER,
                        &channel_text(band, k, if band.bus { "return" } else { "in" }),
                        number_font.clone(),
                        paint::alpha(ink, if lit { 1.0 } else { 0.6 }),
                        HEADER_W - 16.0,
                    );
                }
            }
        }
        // The corner: a minimap of the whole matrix that moves the view.
        let corner = Rect::from_min_size(vp.min, Vec2::new(HEADER_W, HEADER_H));
        let view = (vp.size() - Vec2::new(HEADER_W, HEADER_H)).max(Vec2::ZERO);
        let scroll_to = minimap(ui, corner, layout, look, skin, routes, viewport.min.to_vec2(), view);
        if scroll_to.is_some() {
            gs.scroll_to = scroll_to;
            ui.ctx().request_repaint();
        }
        gs.offset = viewport.min.to_vec2();
        // Seams where the headers meet the bed.
        let hp = ui.painter_at(vp);
        paint::seam(
            &hp,
            Pos2::new(vp.min.x, vp.min.y + HEADER_H - 2.0),
            Pos2::new(vp.max.x, vp.min.y + HEADER_H - 2.0),
            skin,
        );
        hp.line_segment(
            [Pos2::new(vp.min.x + HEADER_W - 1.5, vp.min.y), Pos2::new(vp.min.x + HEADER_W - 1.5, vp.max.y)],
            Stroke::new(1.0, Color32::from_black_alpha(if skin.light() { 70 } else { 160 })),
        );
    });
    gs.hover = hover_now;
    actions
}

/// The minimap in `corner`: the matrix scaled down with its devices' colours
/// along the edges, every route as a dot, and the view outlined. Returns the
/// scroll offset to move to when it is clicked or dragged.
#[allow(clippy::too_many_arguments)]
fn minimap(
    ui: &mut egui::Ui,
    corner: Rect,
    layout: &GridLayout,
    look: &Look,
    skin: &GearSkin,
    routes: &HashSet<(u32, u32)>,
    offset: Vec2,
    view: Vec2,
) -> Option<Vec2> {
    let p = ui.painter_at(corner);
    p.rect_filled(corner, 0.0, skin.ground);
    let (gw, gh) = layout.size();
    let content = Vec2::new(gw, gh);
    let mini = crate::minimap::mini_rect(corner, content);
    let resp = ui.interact(corner, Id::new("matrix-minimap"), Sense::click_and_drag());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Matrix overview"));
    if !mini.is_positive() {
        return None;
    }
    paint::recess(&p, mini.expand(3.0), skin, 5);
    let k = mini.width() / gw;
    for (bi, b) in layout.cols.bands.iter().enumerate() {
        let x = mini.left() + layout.cols.strip_pos(bi) * k;
        let w = ((SEP + b.channels as f32 * layout.cell) * k).max(1.0);
        p.rect_filled(
            Rect::from_min_size(Pos2::new(x, mini.top()), Vec2::new(w, 2.5)),
            0.0,
            band_colour(look, skin, b),
        );
    }
    for (bi, b) in layout.rows.bands.iter().enumerate() {
        let y = mini.top() + layout.rows.strip_pos(bi) * k;
        let h = ((SEP + b.channels as f32 * layout.cell) * k).max(1.0);
        p.rect_filled(
            Rect::from_min_size(Pos2::new(mini.left(), y), Vec2::new(2.5, h)),
            0.0,
            band_colour(look, skin, b),
        );
    }
    let dot = (layout.cell * k / 2.0).clamp(0.8, 2.5);
    for &(i, o) in routes {
        let Some((r, c)) = layout.cell_of(i, o) else { continue };
        let (cx, cy) = layout.cell_pos(r, c);
        let at = mini.min + Vec2::new(cx + layout.cell / 2.0, cy + layout.cell / 2.0) * k;
        let colour = layout.rows.at(r).map(|(b, _)| band_colour(look, skin, b)).unwrap_or(skin.ground_ink);
        p.circle_filled(at, dot, colour);
    }
    let v = crate::minimap::view_rect(mini, content, offset, view);
    p.rect_filled(v, egui::CornerRadius::same(2), paint::alpha(skin.accent, 0.10));
    p.rect_stroke(v, egui::CornerRadius::same(2), Stroke::new(1.5, skin.accent), StrokeKind::Inside);
    let moving = resp.clicked() || resp.dragged() || resp.is_pointer_button_down_on();
    resp.interact_pointer_pos().filter(|_| moving).map(|at| crate::minimap::offset_for(mini, content, view, at))
}

/// A device's name on its strip ("… · OFFLINE" while it is missing).
fn band_name(band: &Band) -> String {
    if band.online {
        band.name.clone()
    } else {
        format!("{} \u{b7} OFFLINE", band.name)
    }
}

/// A channel's margin text: its number and name ("3  Front L"), the custom
/// name first, else the device's; "in 3" when it has neither.
pub fn channel_text(band: &Band, k: u32, word: &str) -> String {
    let named = band
        .channel_labels
        .get(k as usize)
        .cloned()
        .flatten()
        .or_else(|| band.channel_names.get(k as usize).cloned())
        .filter(|n| !n.trim().is_empty());
    match named {
        Some(n) => format!("{}  {n}", k + 1),
        None => format!("{word} {}", k + 1),
    }
}

/// Whether any visible-or-not route exists (cheap: asks the lookup for the
/// first row only when the grid is tiny; the caller's state is authoritative).
fn layout_has_routes(layout: &GridLayout, lookup: &dyn Fn((u32, u32)) -> (Option<PointState>, bool)) -> bool {
    if layout.rows.len * layout.cols.len > 4096 {
        return true;
    }
    (0..layout.rows.len)
        .any(|r| (0..layout.cols.len).any(|c| layout.point(r, c).is_some_and(|p| lookup(p).0.is_some())))
}

/// One crosspoint's face: a pin, or a raised square with its marks.
#[allow(clippy::too_many_arguments)]
fn cell_face(
    p: &egui::Painter,
    rect: Rect,
    cur: Option<&PointState>,
    pending: bool,
    blink: bool,
    dim: bool,
    scale: f32,
    colour: Color32,
    ink: Color32,
    badge_font: &FontId,
) {
    let c = rect.center();
    let Some(pt) = cur else {
        paint::pin(p, c, ink, if dim { 0.06 } else { 0.12 });
        if pending {
            p.circle_stroke(c, 3.0, Stroke::new(1.0, paint::alpha(ink, if blink { 0.8 } else { 0.3 })));
        }
        return;
    };
    let cell = rect.width();
    let side = routed_side(cell, scale);
    let r = Rect::from_center_size(c, Vec2::splat(side));
    let radius = (side * 0.25).clamp(2.0, 4.0);
    let fill = theme::scale(colour, theme::gain_brightness(pt.gain_db));
    let fill = if dim { paint::alpha(fill, 0.5) } else { fill };
    if pt.gain_db < RING_BELOW_DB {
        p.rect_stroke(r, egui::CornerRadius::same(radius as u8), Stroke::new(2.0, fill), StrokeKind::Inside);
    } else {
        paint::raised(p, r, fill, radius);
    }
    let mark = skins::ink_on(colour);
    let outline = if mark == Color32::WHITE { Color32::from_black_alpha(160) } else { Color32::from_white_alpha(160) };
    if pt.mute {
        let slash = [r.left_bottom() + Vec2::new(2.0, -2.0), r.right_top() + Vec2::new(-2.0, 2.0)];
        p.line_segment(slash, Stroke::new(3.0, outline));
        p.line_segment(slash, Stroke::new(1.5, mark));
    }
    if pending {
        for k in [-1.0, 1.0] {
            p.circle_filled(c + Vec2::new(k * 2.5, 0.0), 1.5, paint::alpha(mark, if blink { 1.0 } else { 0.35 }));
        }
        return;
    }
    if cell >= BADGE_FROM && !pt.mute {
        let text = badge_text(pt.gain_db);
        // A hollow ring shows the bed through it: its badge takes the bed's ink.
        let badge_ink = if pt.gain_db < RING_BELOW_DB { ink } else { mark };
        p.text(c + Vec2::new(0.0, 0.5), Align2::CENTER_CENTER, text, badge_font.clone(), badge_ink);
    }
    if pt.invert {
        let corner = r.right_top() + Vec2::new(-4.0, 4.0);
        p.circle_filled(corner, 3.5, outline);
        p.text(corner, Align2::CENTER_CENTER, "\u{f8}", FontId::proportional(6.5), mark);
    }
}

/// Text rotated a quarter turn counter-clockwise, reading upward, ending at
/// `bottom` (its centre x, its lowest y), cut to `room` pixels.
fn rotated(p: &egui::Painter, bottom: Pos2, text: &str, font: FontId, colour: Color32, room: f32) {
    if room < 8.0 {
        return;
    }
    // A name that does not fit tries a smaller face before it is cut.
    let mut font = font;
    let fits = |f: &FontId| p.layout_no_wrap(text.to_string(), f.clone(), colour).size().x <= room;
    if !fits(&font) {
        font.size = 9.0;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, colour);
    job.wrap = egui::text::TextWrapping::from_wrap_mode_and_width(egui::TextWrapMode::Truncate, room);
    let galley = p.layout_job(job);
    let h = galley.size().y;
    let pos = Pos2::new(bottom.x - h / 2.0, bottom.y);
    p.add(TextShape::new(pos, galley, colour).with_angle(-std::f32::consts::FRAC_PI_2));
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::MouseWheelUnit;

    #[test]
    fn a_channel_shows_its_number_and_name() {
        let band = Band {
            slot: 1,
            name: "GoXLR".into(),
            channel_labels: vec![Some("Kick".into()), None, None],
            channel_names: vec!["Mic".into(), "Line".into(), String::new()],
            online: true,
            bus: false,
            first_channel: 0,
            channels: 3,
            start: 0,
            palette: 1,
            color: None,
        };
        assert_eq!(channel_text(&band, 0, "in"), "1  Kick", "the custom name wins");
        assert_eq!(channel_text(&band, 1, "in"), "2  Line", "else the device's name");
        assert_eq!(channel_text(&band, 2, "in"), "in 3", "else the number");
        assert_eq!(channel_text(&band, 9, "out"), "out 10");
    }

    #[test]
    fn wheel_units_become_gain_steps() {
        assert_eq!(notches(MouseWheelUnit::Line, -1.0), -1.0, "a mouse notch is one step");
        assert_eq!(notches(MouseWheelUnit::Point, 25.0), 0.5, "a touchpad moves in fractions");
        let swipe: f32 = (0..10).map(|_| notches(MouseWheelUnit::Point, 5.0)).sum();
        assert!((swipe - 1.0).abs() < 1e-6, "50 px of touchpad is one step: {swipe}");
        assert_eq!(notches(MouseWheelUnit::Page, 1.0), 3.0);
    }

    #[test]
    fn the_badge_is_a_rounded_integer_with_its_sign() {
        assert_eq!(badge_text(0.0), "0");
        assert_eq!(badge_text(-0.4), "0");
        assert_eq!(badge_text(-6.0), "\u{2212}6");
        assert_eq!(badge_text(2.6), "+3");
        assert_eq!(badge_text(f32::NAN), "0");
    }

    #[test]
    fn routed_squares_leave_a_gap_and_scale_with_motion() {
        assert_eq!(routed_side(22.0, 1.0), 16.0);
        assert!((routed_side(22.0, 1.08) - 17.28).abs() < 1e-4);
        assert!((routed_side(12.0, 1.0) - 7.2).abs() < 1e-4, "small cells keep 60 % of the cell");
        assert!(routed_side(40.0, 1.0) >= 24.0);
    }
}
