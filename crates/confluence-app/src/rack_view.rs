//! Drawing the Devices screen as a rack (spec: round 8): full-width sections
//! with a rail and a recessed bed, and in it a tray per position. A filled
//! tray is a raised module: its LED, tag and status pills, the name on a
//! backlit scribble strip in the device's colour, and a wide OLED meter.

use confluence_api::{PosGroup, PosId, PositionState, PositionStatus};
use eframe::egui::{
    self, epaint::TextShape, Align2, Color32, FontId, Id, Pos2, Rect, Sense, Stroke, TextEdit, Vec2, WidgetInfo,
    WidgetType,
};

use crate::bays::Bay;
use crate::commands::Edit;
use crate::devices_screen::{
    card_label, device_color, device_groups, face, Confirm, Face, ScreenAction, ScreenState, Views, DOUBLE_CLICK_SECS,
    SHAPES,
};
use crate::gear::motion::{Motion, PHOSPHOR_TAU, POP, SETTLE};
use crate::gear::oled_meter::{Geom, MeterStyle};
use crate::gear::paint;
use crate::gear::skins::{self, GearSkin};
use crate::prefs::ViewPrefs;
use crate::rack::{self, SectionSize, GAP};

/// The rail at a section's left, with its name reading upward.
pub const RAIL_W: f32 = 26.0;
/// Around the bed inside a section's plate.
pub const PAD: f32 = 7.0;
/// A collapsed section's height.
pub const COLLAPSED_H: f32 = 30.0;
/// Between sections.
pub const SECTION_GAP: f32 = 10.0;

/// A tray being carried: which, its group, where it was grabbed (from its
/// top-left corner) and the order it would drop into.
#[derive(Clone, Debug, PartialEq)]
pub struct TrayDrag {
    pub pos: PosId,
    pub group: String,
    pub grab: Vec2,
    pub order: Vec<String>,
}

/// A section and its trays: each position group's positions in the user's order.
pub struct SectionView<'a> {
    pub bay: Bay,
    pub groups: Vec<(String, Vec<&'a PositionState>)>,
}

impl SectionView<'_> {
    pub fn trays(&self) -> impl Iterator<Item = &PositionState> {
        self.groups.iter().flat_map(|(_, g)| g.iter().copied())
    }
}

/// A position group's key in the saved order (`win-out`).
pub fn group_key(g: PosGroup) -> String {
    PosId { group: g, index: 0 }.to_string().split(':').next().unwrap_or_default().to_string()
}

/// The sections in screen order, every position in each (spec §1).
pub fn sections<'a>(positions: &'a [PositionState], layout: &crate::prefs::LayoutPrefs) -> Vec<SectionView<'a>> {
    Bay::all()
        .into_iter()
        .filter_map(|bay| {
            let groups: Vec<(String, Vec<&PositionState>)> = bay
                .groups()
                .iter()
                .filter_map(|&g| {
                    let mut of: Vec<&PositionState> = positions.iter().filter(|p| p.pos.group == g).collect();
                    if of.is_empty() {
                        return None;
                    }
                    of.sort_by_key(|p| p.pos.index);
                    let key = group_key(g);
                    let default: Vec<String> = of.iter().map(|p| p.pos.to_string()).collect();
                    let order = crate::arrange::ordered(&default, layout.cards.get(&key).map_or(&[], |v| v.as_slice()));
                    of.sort_by_key(|p| order.iter().position(|k| *k == p.pos.to_string()));
                    Some((key, of))
                })
                .collect();
            (!groups.is_empty()).then_some(SectionView { bay, groups })
        })
        .collect()
}

/// A tray's columns: by its meter's channels; empty and switched-off virtual trays are one.
pub fn span_of(p: &PositionState, v: &Views, col_w: f32, cols: usize) -> usize {
    if matches!(p.status, PositionStatus::Empty | PositionStatus::Off) {
        return 1;
    }
    // At full card bars, whatever the style (so switching style never moves trays).
    let groups = device_groups(p, v);
    let full = Geom::card().resolve(MeterStyle::Segments, 1.0, 40.0);
    rack::tray_span(col_w, cols, |w| {
        let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, 40.0));
        !crate::gear::oled_meter::meter_layout(&groups, r, &full).overflow
    })
}

/// Where a section's trays go (in `trays` order) inside its bed, and its rows.
pub fn tray_rects(
    spans: &[usize],
    breaks: &[bool],
    cols: usize,
    col_w: f32,
    tray_h: f32,
    bed: Pos2,
) -> (Vec<Rect>, usize) {
    let (at, rows) = rack::place(spans, breaks, cols);
    let rects = at
        .iter()
        .zip(spans)
        .map(|(&(row, col), &s)| {
            let s = s.clamp(1, cols.max(1));
            Rect::from_min_size(
                bed + Vec2::new(col as f32 * (col_w + GAP), row as f32 * (tray_h + GAP)),
                Vec2::new(rack::tray_w(s, col_w), tray_h),
            )
        })
        .collect();
    (rects, rows)
}

/// The section's size as saved.
pub fn size_of(prefs: &ViewPrefs, bay: Bay) -> SectionSize {
    prefs.layout.section_sizes.get(bay.title()).copied().unwrap_or_default()
}

/// Draws every section, from the top of `ui`'s remaining space, `width` wide at `left`.
#[allow(clippy::too_many_arguments)]
pub fn show_sections(
    ui: &mut egui::Ui,
    left: f32,
    width: f32,
    positions: &[PositionState],
    v: &Views,
    base: &GearSkin,
    palette: &[Color32],
    st: &mut ScreenState,
    motion: &mut Motion,
    prefs: &mut ViewPrefs,
    actions: &mut Vec<ScreenAction>,
) {
    let secs = sections(positions, &prefs.layout);
    let bed_w = width - RAIL_W - 3.0 * PAD;
    let cols = rack::columns(bed_w);
    let col_w = rack::col_width(bed_w, cols);
    for sec in &secs {
        if prefs.layout.hidden_sections.contains(sec.bay.title()) {
            continue;
        }
        let size = size_of(prefs, sec.bay);
        let full_w = ui.available_width();
        let Some(tray_h) = size.tray_h() else {
            let (band, _) = ui.allocate_exact_size(Vec2::new(full_w, COLLAPSED_H + SECTION_GAP), Sense::hover());
            let plate = Rect::from_min_size(Pos2::new(left, band.top()), Vec2::new(width, COLLAPSED_H));
            collapsed(ui, plate, sec, base, prefs);
            continue;
        };
        let span_at: std::collections::HashMap<PosId, usize> = sec
            .trays()
            .map(|p| (p.pos, if size == SectionSize::Compact { 1 } else { span_of(p, v, col_w, cols) }))
            .collect();
        // Where each tray goes for these groups' orders, from `origin`.
        let layout_of = |groups: &[(String, Vec<&PositionState>)], origin: Pos2| -> (Vec<(PosId, Rect)>, usize) {
            let (mut spans, mut breaks, mut ids) = (Vec::new(), Vec::new(), Vec::new());
            for (_, g) in groups {
                for (i, p) in g.iter().enumerate() {
                    spans.push(span_at.get(&p.pos).copied().unwrap_or(1));
                    breaks.push(i == 0);
                    ids.push(p.pos);
                }
            }
            let (rects, rows) = tray_rects(&spans, &breaks, cols, col_w, tray_h, origin);
            (ids.into_iter().zip(rects).collect(), rows)
        };
        // A tray of this section being carried: its group shows the order it would drop into.
        let carried = st.tray_drag.clone().filter(|d| sec.groups.iter().any(|(k, _)| *k == d.group));
        let mut groups = sec.groups.clone();
        if let Some(d) = &carried {
            for (k, g) in groups.iter_mut().filter(|(k, _)| *k == d.group) {
                let _ = k;
                g.sort_by_key(|p| d.order.iter().position(|o| *o == p.pos.to_string()));
            }
        }
        let (_, rows) = layout_of(&groups, Pos2::ZERO);
        let bed_h = rows.max(1) as f32 * tray_h + rows.saturating_sub(1) as f32 * GAP;
        let plate_h = bed_h + 2.0 * PAD;
        let (band, _) = ui.allocate_exact_size(Vec2::new(full_w, plate_h + SECTION_GAP), Sense::hover());
        let plate = Rect::from_min_size(Pos2::new(left, band.top()), Vec2::new(width, plate_h));
        let rail = Rect::from_min_size(plate.min + Vec2::new(PAD, PAD), Vec2::new(RAIL_W, bed_h));
        let bed = Rect::from_min_max(Pos2::new(rail.right() + PAD, rail.top()), plate.max - Vec2::new(PAD, PAD));
        // Room for the plate's light and shadow (clipped close, their corners show square).
        let p = ui.painter_at(plate.expand(40.0));
        paint::panel(&p, plate, base, None);
        rail_face(ui, rail, sec.bay, size, base, prefs);
        paint::recess(&p, bed.expand(3.0), base, 8);
        let (placed, _) = layout_of(&groups, bed.min);
        let by_pos = |pos: PosId| sec.trays().find(|p| p.pos == pos);
        let pointer = ui.input(|i| i.pointer.latest_pos());
        let mut held = None;
        for (pos, slot) in placed {
            let Some(p) = by_pos(pos) else { continue };
            if carried.as_ref().is_some_and(|d| d.pos == pos) {
                held = Some((p, slot));
                continue;
            }
            // Trays glide to their places (relative to the bed, so scrolling never lags).
            let tid = Id::new(("tray-at", pos.to_string()));
            let rel = slot.min - bed.min;
            let x = motion.spring(tid.with("x"), rel.x, SETTLE);
            let y = motion.spring(tid.with("y"), rel.y, SETTLE);
            let at = Rect::from_min_size(bed.min + Vec2::new(x, y), slot.size());
            let resp = tray(ui, at, p, size, v, base, palette, st, motion, prefs, actions);
            if resp.drag_started() && !crate::bays::vacant(p) {
                let from = ui.input(|i| i.pointer.press_origin()).or(pointer).unwrap_or(at.center());
                let group = group_key(p.pos.group);
                let order = groups
                    .iter()
                    .find(|(k, _)| *k == group)
                    .map(|(_, g)| g.iter().map(|q| q.pos.to_string()).collect())
                    .unwrap_or_default();
                st.tray_drag = Some(TrayDrag { pos, group, grab: from - at.min, order });
            }
        }
        // The carried tray: lifted, shadowed, under the pointer, kept inside its section.
        if let (Some((p, slot)), Some(d)) = (held, carried) {
            let to = pointer.map_or(slot.min, |ptr| ptr - d.grab);
            let to = Pos2::new(
                to.x.clamp(bed.left() - 4.0, bed.right() - slot.width() + 4.0),
                to.y.clamp(bed.top() - 4.0, bed.bottom() - slot.height() + 4.0),
            );
            let at = Rect::from_min_size(to, slot.size());
            let tid = Id::new(("tray-at", p.pos.to_string()));
            motion.snap(tid.with("x"), at.min.x - bed.min.x, SETTLE);
            motion.snap(tid.with("y"), at.min.y - bed.min.y, SETTLE);
            ui.painter().rect_filled(
                at.translate(Vec2::new(0.0, 7.0)).expand(3.0),
                9.0,
                Color32::from_black_alpha(110),
            );
            let lifted = Rect::from_center_size(at.center(), at.size() * 1.03);
            let resp = tray(ui, lifted, p, size, v, base, palette, st, motion, prefs, actions);
            let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
            let let_go = resp.drag_stopped() || !ui.input(|i| i.pointer.any_down());
            if escape {
                st.tray_drag = None;
            } else if let_go {
                prefs.layout.cards.insert(d.group.clone(), d.order.clone());
                st.tray_drag = None;
            } else {
                // Where it would drop: beside the tray under the pointer, as shown now.
                let (all, _) = layout_of(&groups, bed.min);
                let rects: Vec<Rect> = d
                    .order
                    .iter()
                    .map(|k| all.iter().find(|(pos, _)| pos.to_string() == *k).map_or(Rect::NOTHING, |(_, r)| *r))
                    .collect();
                let at_ptr = pointer.unwrap_or(at.center());
                let order = rack::drop_order(&d.order, &d.pos.to_string(), &rects, at_ptr);
                if let Some(t) = st.tray_drag.as_mut() {
                    t.order = order;
                }
                ui.ctx().request_repaint();
            }
        }
    }
}

/// The rail: the section's name reading upward; a click cycles its size,
/// and its menu picks one.
fn rail_face(ui: &mut egui::Ui, r: Rect, bay: Bay, size: SectionSize, base: &GearSkin, prefs: &mut ViewPrefs) {
    let p = ui.painter_at(r.expand(2.0));
    paint::recess(&p, r, base, 6);
    let title = bay.title().to_uppercase();
    upward(&p, Pos2::new(r.center().x, r.bottom() - 8.0), &title, base, r.height() - 16.0);
    let what = format!("{} section: {}", bay.title(), size.word());
    let resp = ui.interact(r, Id::new(("rail", bay.title())), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &what));
    let resp = resp.on_hover_text("Click to change the size of this section");
    if resp.clicked() {
        prefs.layout.section_sizes.insert(bay.title().to_string(), size.next());
    }
    resp.context_menu(|ui| {
        for s in [SectionSize::Full, SectionSize::Compact, SectionSize::Collapsed] {
            let word = s.word();
            let label = format!("{}{}", word[..1].to_uppercase(), &word[1..]);
            if ui.radio(size == s, label).clicked() {
                prefs.layout.section_sizes.insert(bay.title().to_string(), s);
                ui.close();
            }
        }
    });
}

/// Text reading upward from `bottom` (its centre x), etched into the rail.
fn upward(p: &egui::Painter, bottom: Pos2, text: &str, base: &GearSkin, room: f32) {
    let mut size = 10.5;
    let mk = |size: f32| {
        let f = paint::font(&p.ctx().clone(), "label-bold", size);
        p.layout_no_wrap(text.to_string(), f, paint::alpha(base.ground_ink, 0.72))
    };
    let mut g = mk(size);
    while g.size().x > room && size > 7.0 {
        size -= 0.5;
        g = mk(size);
    }
    let h = g.size().y;
    let pos = Pos2::new(bottom.x - h / 2.0, bottom.y);
    p.add(TextShape::new(pos, g, paint::alpha(base.ground_ink, 0.72)).with_angle(-std::f32::consts::FRAC_PI_2));
}

/// A collapsed section: one band, its name and how many positions are in use.
fn collapsed(ui: &mut egui::Ui, r: Rect, sec: &SectionView, base: &GearSkin, prefs: &mut ViewPrefs) {
    let p = ui.painter_at(r.expand(40.0));
    paint::panel(&p, r, base, None);
    let total = sec.trays().count();
    let used = sec.trays().filter(|t| !crate::bays::vacant(t)).count();
    let names: Vec<String> = sec
        .trays()
        .filter(|t| !crate::bays::vacant(t))
        .filter_map(|t| t.device.as_ref().map(|d| crate::devices_screen::split_name(d.kind, &d.name).0))
        .take(4)
        .collect();
    let text = format!(
        "{}  \u{b7}  {used} of {total} in use{}",
        sec.bay.title().to_uppercase(),
        if names.is_empty() { String::new() } else { format!("  \u{b7}  {}", names.join(", ")) }
    );
    paint::truncated(
        &p,
        Pos2::new(r.left() + 14.0, r.center().y),
        Align2::LEFT_CENTER,
        &text,
        paint::font(ui.ctx(), "label-bold", 11.0),
        paint::alpha(base.ground_ink, 0.75),
        r.width() - 28.0,
    );
    let what = format!("{} section: collapsed", sec.bay.title());
    let resp = ui.interact(r, Id::new(("rail", sec.bay.title())), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &what));
    if resp.on_hover_text("Click to show this section").clicked() {
        prefs.layout.section_sizes.insert(sec.bay.title().to_string(), SectionSize::Full);
    }
}

/// A small status pill at `at` (its left centre); clickable when `accessible` is given.
#[allow(clippy::too_many_arguments)]
fn mini_pill(
    ui: &mut egui::Ui,
    p: &egui::Painter,
    at: Pos2,
    text: &str,
    fill: Color32,
    ink: Color32,
    id: Id,
    accessible: Option<&str>,
) -> (Rect, bool) {
    let f = paint::font(ui.ctx(), "label-bold", 8.5);
    let g = p.layout_no_wrap(text.to_string(), f, ink);
    let r = Rect::from_min_size(Pos2::new(at.x, at.y - 7.0), Vec2::new(g.size().x + 10.0, 14.0));
    let mut clicked = false;
    let mut hover = false;
    if let Some(what) = accessible {
        let resp = ui.interact(r.expand(2.0), id, Sense::click());
        let enabled = ui.is_enabled();
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, what));
        hover = resp.hovered();
        clicked = resp.clicked();
    }
    p.rect_filled(r, 7.0, if hover { paint::mix(fill, Color32::WHITE, 0.12) } else { fill });
    p.rect_stroke(r, 7.0, Stroke::new(1.0, Color32::from_black_alpha(90)), egui::StrokeKind::Outside);
    p.galley(r.center() - g.size() / 2.0, g, ink);
    (r, clicked)
}

/// The edit that switches `p` on or off.
fn switch_edit(p: &PositionState, on: bool) -> Edit {
    if p.pos.group.is_virtual() {
        Edit::SetVirtual { pos: p.pos, on, shape: None }
    } else {
        Edit::SetEnabled { pos: p.pos, on }
    }
}

/// One position's tray.
#[allow(clippy::too_many_arguments)]
fn tray(
    ui: &mut egui::Ui,
    r: Rect,
    p: &PositionState,
    size: SectionSize,
    v: &Views,
    base: &GearSkin,
    palette: &[Color32],
    st: &mut ScreenState,
    motion: &mut Motion,
    prefs: &ViewPrefs,
    actions: &mut Vec<ScreenAction>,
) -> egui::Response {
    let filling = st.filling.contains(&p.pos);
    let mut f = face(p, v, filling);
    let custom = p.slots.iter().find_map(|id| v.slots.iter().find(|s| s.id == *id)).and_then(|s| s.label.clone());
    if custom.is_some() {
        let (name, sub) = crate::names::display(custom.as_deref(), &f.name, prefs.only_custom_names);
        f.sub = sub.unwrap_or_default();
        f.name = name;
    }
    let id = Id::new(("position-card", p.pos.to_string()));
    let empty = p.status == PositionStatus::Empty;
    let virtual_off = p.status == PositionStatus::Off;
    let disabled = p.status == PositionStatus::Disabled;
    let sense = if empty { Sense::click() } else { Sense::click_and_drag() };
    let resp = ui.interact(r, id, sense);
    if st.picker == Some(p.pos) {
        st.picker_anchor = r;
    }
    let enabled = ui.is_enabled();
    let label = card_label(&f, empty);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &label));
    let painter = ui.painter_at(r.expand(36.0));
    if empty {
        empty_slot(ui, &painter, r, &f.tag, size, base, resp.hovered(), motion, id);
        if resp.clicked() {
            st.open_picker(p.pos, false, r);
        }
        return resp;
    }
    let colour = device_color(p, palette);
    let compact = size == SectionSize::Compact;
    // The module, raised in its slot; dim while switched off.
    paint::recess(&painter, r, base, 7);
    let hovered = ui.rect_contains_pointer(r);
    let lift = motion.spring(id.with("lift"), if hovered { 1.0 } else { 0.0 }, SETTLE);
    let pop = if st.just_filled.contains(&p.pos) {
        motion.spring_from(id.with("pop"), 0.94, 1.0, POP)
    } else {
        motion.spring(id.with("pop"), 1.0, POP)
    };
    if pop >= 0.999 {
        st.just_filled.remove(&p.pos);
    }
    let m = Rect::from_center_size(r.center(), (r.size() - Vec2::splat(4.0)) * pop);
    let off = virtual_off || disabled;
    let skin = if off { base.powered_off() } else { *base };
    paint::panel_lifted(&painter, m, &skin, None, lift, 6);
    let mut painter = painter.clone();
    if off {
        painter.set_opacity(0.8);
    }
    // The meter window, right: at least two fifths of the tray.
    // (Its inner width is `rack::meter_w`, which sized the tray.)
    let ow = if compact { (m.width() * 0.42).clamp(70.0, 160.0) } else { rack::oled_w(r.width() - 4.0) - 6.0 };
    let well = Rect::from_min_max(Pos2::new(m.right() - 5.0 - ow, m.top() + 5.0), m.max - Vec2::new(5.0, 5.0));
    let info_w = well.left() - m.left() - 10.0;
    // Row 1: LED, tag, pills.
    let row1 = if compact { m.center().y } else { m.top() + 12.0 };
    let led_at = Pos2::new(m.left() + 11.0, row1);
    let now = motion.now();
    let seen = st.led_seen.entry(p.pos).or_insert((f.led, now));
    if seen.0 != f.led {
        *seen = (f.led, now);
    }
    let bloom = 1.0 + 0.6 * (1.0 - motion.since(seen.1, 0.4));
    let want = match f.led {
        Some(_) if f.blink => f32::from(u8::from(motion.blink(2.0))),
        Some(_) => 1.0,
        None => 0.0,
    };
    let glow = motion.phosphor(id.with("led"), want, PHOSPHOR_TAU) * bloom;
    paint::led_glow(&painter, led_at, &skin, f.led.unwrap_or(Color32::GRAY), glow);
    // The LED is the tray's power switch (asking first where routes would go);
    // the master clock is always on.
    let led = ui.interact(Rect::from_center_size(led_at, Vec2::splat(14.0)), id.with("led"), Sense::click());
    if !p.master || p.pos.group.is_virtual() {
        let label = p.pos.label();
        let what = if off { format!("Turn on {label}") } else { format!("Turn off {label}") };
        led.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &what));
        if led.hovered() {
            painter.circle_stroke(led_at, 7.0, Stroke::new(1.0, paint::alpha(skin.ink, 0.5)));
        }
        if led.on_hover_text(if off { "Turn on" } else { "Turn off" }).clicked() {
            if off {
                st.just_filled.insert(p.pos);
                actions.push(ScreenAction::Edit(switch_edit(p, true)));
            } else if p.pos.group.is_virtual() {
                st.confirm = Some(Confirm { pos: p.pos, clear: false, at: now });
            } else {
                actions.push(ScreenAction::Edit(switch_edit(p, false)));
            }
        }
    } else {
        led.on_hover_text("The master clock is always on");
    }
    let tag_r = paint::tag(&painter, led_at + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, &f.tag, &skin, 0.78);
    let mut x = tag_r.right() + 6.0;
    let mut strip_left = m.left() + 8.0;
    if compact {
        strip_left = x;
    } else {
        // Pills only where they fit before the meter window.
        if p.master && x + 52.0 <= m.left() + info_w {
            let gold = Color32::from_rgb(0xf3, 0xc2, 0x4f);
            let (pr, _) = mini_pill(
                ui,
                &painter,
                Pos2::new(x, row1),
                "MASTER",
                gold,
                Color32::from_rgb(0x2a, 0x1e, 0x00),
                id.with("m"),
                None,
            );
            x = pr.right() + 4.0;
        }
        if let (Some((ins, outs)), PosGroup::Vasio, false) = (p.shape, p.pos.group, off) {
            let text = format!("{outs}\u{d7}{ins}");
            // Only where it fits before the meter window.
            if x + 12.0 + 6.5 * text.chars().count() as f32 <= m.left() + info_w {
                let fill = Color32::from_rgb(0x2e, 0x30, 0x35);
                mini_pill(ui, &painter, Pos2::new(x, row1), &text, fill, skin.ink, id.with("shape"), None);
            }
        }
    }
    // The name on its scribble strip, in the device's colour.
    let name_y = if compact { m.center().y } else { m.top() + 32.0 };
    let font = paint::font(ui.ctx(), "label-bold", if compact { 11.5 } else { 13.0 });
    let room = (m.left() + info_w - strip_left).max(30.0);
    let strip_colour = if off { paint::mix(colour, Color32::from_gray(40), 0.6) } else { colour };
    let ink = skins::ink_on(strip_colour);
    let gal = painter.layout_no_wrap(f.name.clone(), font.clone(), ink);
    let strip = Rect::from_min_size(
        Pos2::new(strip_left, name_y - if compact { 8.0 } else { 9.5 }),
        Vec2::new((gal.size().x + 14.0).min(room), if compact { 16.0 } else { 19.0 }),
    );
    // Parts of the tray drawn over it (strip, meter): its menu opens from them too.
    let mut parts: Vec<egui::Response> = Vec::new();
    let renaming = st.renaming.as_ref().is_some_and(|(pos, _)| *pos == p.pos);
    if virtual_off {
        paint::truncated(
            &painter,
            Pos2::new(strip_left, name_y),
            Align2::LEFT_CENTER,
            "Switched off",
            font,
            paint::alpha(skin.ink, 0.5),
            room,
        );
    } else if renaming {
        rename_field(ui, Rect::from_min_size(strip.min, Vec2::new(room, strip.height())), p, &font, st, actions);
    } else {
        paint::raised(&painter, strip, strip_colour, 3.0);
        painter.rect_filled(
            Rect::from_min_size(strip.min + Vec2::new(2.0, 1.0), Vec2::new(strip.width() - 4.0, strip.height() * 0.45)),
            2.0,
            Color32::from_white_alpha(28),
        );
        paint::truncated(
            &painter,
            Pos2::new(strip.left() + 7.0, strip.center().y),
            Align2::LEFT_CENTER,
            &f.name,
            font,
            ink,
            strip.width() - 12.0,
        );
        if !p.slots.is_empty() {
            let name_resp = ui.interact(strip, id.with("name"), Sense::click());
            parts.push(name_resp.clone());
            let what = format!("Rename {}", p.pos.label());
            name_resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &what));
            if name_resp.clicked() {
                let t = ui.input(|i| i.time);
                match st.name_click {
                    Some((pos, at)) if pos == p.pos && t - at < DOUBLE_CLICK_SECS => {
                        st.name_click = None;
                        st.renaming = Some((p.pos, custom.clone().unwrap_or_default()));
                    }
                    _ => st.name_click = Some((p.pos, t)),
                }
            }
        }
    }
    if !compact {
        let sub = if disabled {
            "off \u{b7} routes kept".to_string()
        } else if virtual_off {
            String::new()
        } else if f.sub.is_empty() {
            f.line1.to_lowercase()
        } else {
            f.sub.clone()
        };
        paint::truncated(
            &painter,
            Pos2::new(m.left() + 9.0, m.bottom() - 10.0),
            Align2::LEFT_CENTER,
            &sub,
            paint::font(ui.ctx(), "label", 10.0),
            paint::alpha(skin.ink, 0.55),
            info_w - 4.0,
        );
    }
    // The OLED: a caption and the meter (full), or two thin bars (compact).
    paint::oled_well(&painter, well, &skin);
    let groups = if off { Vec::new() } else { device_groups(p, v) };
    let oled_c = paint::alpha(skin.oled, if off { 0.35 } else { 0.9 });
    if compact {
        for (k, g) in groups.iter().take(2).enumerate() {
            let peak = g.channels.iter().map(|c| c.peak_db).fold(f32::NEG_INFINITY, f32::max);
            let frac = ((peak + 60.0) / 60.0).clamp(0.0, 1.0);
            let track = Rect::from_min_size(
                Pos2::new(well.left() + 6.0, well.center().y - 5.0 + k as f32 * 6.0),
                Vec2::new(well.width() - 12.0, 4.0),
            );
            painter.rect_filled(track, 0.0, paint::alpha(skin.oled, 0.12));
            painter.rect_filled(Rect::from_min_size(track.min, Vec2::new(track.width() * frac, 4.0)), 0.0, skin.oled);
        }
    } else {
        let inner = well.shrink2(Vec2::new(5.0, 4.0));
        if off {
            let caption = if disabled { "OFF  ROUTES KEPT" } else { "OFF" };
            let mut mesh = egui::epaint::Mesh::default();
            crate::gear::pixel_font::draw(&mut mesh, inner.left_top(), caption, 1.0, oled_c, false);
            painter.with_clip_rect(inner).add(egui::Shape::mesh(mesh));
        }
        if !groups.is_empty() {
            let mrect = inner;
            let ppp = ui.ctx().pixels_per_point();
            let geom = crate::gear::oled_meter::fit_geom(
                &groups,
                mrect.width(),
                Geom::card().resolve(prefs.meter.style, ppp, mrect.height()),
            );
            if prefs.meter.style == MeterStyle::DotMatrix {
                let dots = crate::gear::oled_meter::grid_dots(&geom, ppp, mrect.height());
                crate::gear::oled_meter::dot_grid(&painter.with_clip_rect(well.shrink(2.0)), well.shrink(2.0), dots);
            }
            let mw =
                crate::gear::oled_meter::meter_widget(ui, id.with("meter"), mrect, &groups, &geom, prefs.meter, motion);
            parts.push(mw.response.clone());
            let what = format!("Channels of {}", p.pos.label());
            mw.response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &what));
            if mw.response.clicked() {
                st.channels_of = Some(p.pos);
                st.channel_drafts.clear();
            }
        }
    }
    if st.confirm.is_some_and(|c| c.pos == p.pos) {
        confirm_popup(ui.ctx(), r, &skin, st, actions);
    }
    // The tray's menu opens from anywhere on it, its strip and meter too.
    for part in std::iter::once(&resp).chain(&parts) {
        tray_menu(part, p, palette, colour, custom.clone(), off, now, st, actions, r);
    }
    if resp.clicked() {
        if ui.input(|i| i.modifiers.command) && !p.pos.group.is_virtual() {
            st.open_picker(p.pos, true, r);
        } else if let Some(&slot) = p.slots.first() {
            actions.push(ScreenAction::Select(slot));
        }
    }
    resp
}

/// An empty slot: recessed, two screws, its engraved tag and a quiet hint.
#[allow(clippy::too_many_arguments)]
fn empty_slot(
    ui: &egui::Ui,
    p: &egui::Painter,
    r: Rect,
    tag: &str,
    size: SectionSize,
    base: &GearSkin,
    hovered: bool,
    motion: &mut Motion,
    id: Id,
) {
    let hover = motion.spring(id.with("hover"), if hovered { 1.0 } else { 0.0 }, SETTLE);
    paint::recess(p, r, base, 7);
    let ink = base.ground_ink;
    if size == SectionSize::Compact {
        paint::etched_text(
            p,
            Pos2::new(r.left() + 10.0, r.center().y),
            Align2::LEFT_CENTER,
            tag,
            base,
            ink,
            9.5,
            true,
            0.1,
            0.45 + 0.4 * hover,
        );
        paint::etched_text(
            p,
            Pos2::new(r.right() - 12.0, r.center().y),
            Align2::RIGHT_CENTER,
            "+",
            base,
            ink,
            14.0,
            false,
            0.0,
            0.4 + 0.5 * hover,
        );
        return;
    }
    for c in [r.left_top() + Vec2::new(7.0, 7.0), r.right_bottom() - Vec2::new(7.0, 7.0)] {
        screw(p, c);
    }
    paint::etched_text(
        p,
        Pos2::new(r.left() + 16.0, r.top() + 14.0),
        Align2::LEFT_CENTER,
        tag,
        base,
        ink,
        9.5,
        true,
        0.1,
        0.5 + 0.4 * hover,
    );
    let _ = ui;
    paint::etched_text(
        p,
        Pos2::new(r.left() + 16.0, r.bottom() - 14.0),
        Align2::LEFT_CENTER,
        "Click to choose",
        base,
        ink,
        9.5,
        false,
        0.0,
        0.32 + 0.45 * hover,
    );
}

/// A small slotted screw head.
fn screw(p: &egui::Painter, c: Pos2) {
    p.circle_filled(c + Vec2::new(0.0, 0.8), 3.2, Color32::from_black_alpha(120));
    p.circle_filled(c, 3.0, Color32::from_gray(78));
    p.circle_filled(c - Vec2::new(0.8, 0.9), 1.4, Color32::from_white_alpha(40));
    let d = Vec2::new(2.1, -1.2);
    p.line_segment([c - d, c + d], Stroke::new(0.9, Color32::from_gray(30)));
}

/// The strip's text field while the device is being renamed.
fn rename_field(
    ui: &mut egui::Ui,
    r: Rect,
    p: &PositionState,
    font: &FontId,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let mut text = st.renaming.as_ref().map(|r| r.1.clone()).unwrap_or_default();
    let edit = ui.put(r, TextEdit::singleline(&mut text).hint_text("Custom name").font(font.clone()));
    if !edit.has_focus() && !edit.lost_focus() {
        edit.request_focus();
    }
    let (enter, escape) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
    if escape {
        st.renaming = None;
    } else if edit.lost_focus() || enter {
        st.renaming = None;
        if let Some(&id) = p.slots.first() {
            actions.push(ScreenAction::Edit(Edit::SetSlotLabel { id, channel: None, name: Some(text) }));
        }
    } else if let Some(r) = st.renaming.as_mut() {
        r.1 = text;
    }
}

/// "Clear VASIO A? Its routes are removed. [Yes] [No]", floating under the tray.
fn confirm_popup(
    ctx: &egui::Context,
    anchor: Rect,
    skin: &GearSkin,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
) {
    let Some(c) = st.confirm else { return };
    let verb = if c.clear { "Clear" } else { "Turn off" };
    let question = format!("{verb} {}? Its routes are removed.", c.pos.label());
    egui::Area::new(Id::new("tray-confirm"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + Vec2::new(0.0, 4.0))
        .show(ctx, |ui| {
            let r = ui.available_rect_before_wrap();
            let _ = r;
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&question);
                    if paint::pill_tinted(ui, "Yes", verb, skins::RED, skin).clicked() {
                        st.confirm = None;
                        actions.push(ScreenAction::Edit(if c.clear {
                            Edit::ClearPosition { pos: c.pos }
                        } else {
                            Edit::SetVirtual { pos: c.pos, on: false, shape: None }
                        }));
                    }
                    if paint::pill_labeled(ui, "No", "Cancel", skin).clicked() {
                        st.confirm = None;
                    }
                });
            });
        });
}

/// The tray's menu: rename it or its channels, colour, VASIO size, master,
/// on/off, swap and clear.
#[allow(clippy::too_many_arguments)]
fn tray_menu(
    resp: &egui::Response,
    p: &PositionState,
    palette: &[Color32],
    current: Color32,
    custom: Option<String>,
    off: bool,
    now: f64,
    st: &mut ScreenState,
    actions: &mut Vec<ScreenAction>,
    anchor: Rect,
) {
    let slot = p.slots.first().copied();
    resp.context_menu(|ui| {
        if slot.is_some() && ui.button("Rename\u{2026}").clicked() {
            st.renaming = Some((p.pos, custom.clone().unwrap_or_default()));
            ui.close();
        }
        if slot.is_some() && ui.button("Rename channels\u{2026}").clicked() {
            st.channels_of = Some(p.pos);
            st.channel_drafts.clear();
            ui.close();
        }
        if let Some(slot) = slot {
            ui.menu_button("Colour", |ui| {
                ui.horizontal_wrapped(|ui| {
                    for c in palette {
                        let (r, cell) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 8.0, *c);
                        if current == *c {
                            ui.painter().circle_stroke(r.center(), 9.5, Stroke::new(2.0, ui.visuals().text_color()));
                        }
                        let hex = format!("Colour #{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
                        cell.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &hex));
                        if cell.clicked() {
                            let color = Some([c.r(), c.g(), c.b()]);
                            actions.push(ScreenAction::Edit(Edit::SetSlotColor { id: slot, color }));
                            ui.close();
                        }
                    }
                });
                if ui.button("Default").clicked() {
                    actions.push(ScreenAction::Edit(Edit::SetSlotColor { id: slot, color: None }));
                    ui.close();
                }
            });
        }
        if p.pos.group == PosGroup::Vasio && !off {
            let (ins, outs) = p.shape.unwrap_or((8, 8)); // engine side: (DAW outputs, DAW inputs)
            ui.menu_button("VASIO size", |ui| {
                for n in SHAPES {
                    if ui.button(format!("{n}\u{d7}{n}")).clicked() {
                        actions.push(ScreenAction::Edit(Edit::SetVirtual {
                            pos: p.pos,
                            on: true,
                            shape: Some((n, n)),
                        }));
                        ui.close();
                    }
                }
                ui.separator();
                ui.label("DAW inputs");
                ui.horizontal(|ui| {
                    for n in SHAPES {
                        if ui.selectable_label(outs == n, n.to_string()).clicked() {
                            actions.push(ScreenAction::Edit(Edit::SetVirtual {
                                pos: p.pos,
                                on: true,
                                shape: Some((ins, n)),
                            }));
                        }
                    }
                });
                ui.label("DAW outputs");
                ui.horizontal(|ui| {
                    for n in SHAPES {
                        if ui.selectable_label(ins == n, n.to_string()).clicked() {
                            actions.push(ScreenAction::Edit(Edit::SetVirtual {
                                pos: p.pos,
                                on: true,
                                shape: Some((n, outs)),
                            }));
                        }
                    }
                });
            });
        }
        if p.pos.group == PosGroup::Asio && !p.master && ui.button("Make master").clicked() {
            actions.push(ScreenAction::Edit(Edit::SetMaster { pos: Some(p.pos) }));
            ui.close();
        }
        if !off && ui.button("Clear clips").clicked() {
            actions.push(ScreenAction::Edit(Edit::ClearClip));
            ui.close();
        }
        ui.separator();
        let virt = p.pos.group.is_virtual();
        if off {
            if ui.button("Turn on").clicked() {
                st.just_filled.insert(p.pos);
                actions.push(ScreenAction::Edit(switch_edit(p, true)));
                ui.close();
            }
        } else if (!p.master || virt) && ui.button(if virt { "Turn off\u{2026}" } else { "Turn off" }).clicked() {
            if virt {
                st.confirm = Some(Confirm { pos: p.pos, clear: false, at: now });
            } else {
                actions.push(ScreenAction::Edit(switch_edit(p, false)));
            }
            ui.close();
        }
        if !virt {
            if ui.button("Swap device\u{2026}").clicked() {
                st.open_picker(p.pos, true, anchor);
                ui.close();
            }
            if ui.button("Clear\u{2026}").clicked() {
                st.confirm = Some(Confirm { pos: p.pos, clear: true, at: now });
                ui.close();
            }
        }
    });
}

/// The face of a card, kept for callers that only need its text.
pub fn tray_face(p: &PositionState, v: &Views) -> Face {
    face(p, v, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::all_positions;

    fn st(pos: PosId) -> PositionState {
        PositionState {
            pos,
            status: PositionStatus::Empty,
            device: None,
            shape: None,
            daw: None,
            master: false,
            color: None,
            slots: vec![],
        }
    }

    #[test]
    fn every_position_is_in_its_section_and_groups_follow_the_saved_order() {
        let ps: Vec<PositionState> = all_positions().into_iter().map(st).collect();
        let mut lp = crate::prefs::LayoutPrefs::default();
        lp.cards.insert("win-out".into(), vec!["win-out:3".into(), "win-out:1".into()]);
        let secs = sections(&ps, &lp);
        assert_eq!(secs.iter().map(|s| s.trays().count()).sum::<usize>(), ps.len(), "every position, once");
        let win = secs.iter().find(|s| s.bay == Bay::Windows).unwrap();
        let keys: Vec<&str> = win.groups.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["win-in", "win-out"], "INs, then OUTs");
        let outs: Vec<String> = win.groups[1].1.iter().take(3).map(|p| p.pos.to_string()).collect();
        assert_eq!(outs, vec!["win-out:3", "win-out:1", "win-out:2"]);
        assert_eq!(group_key(PosGroup::NetIn), "net-in");
    }

    #[test]
    fn a_trays_meter_holds_full_bars_in_every_style() {
        use crate::gear::oled_meter::{meter_layout, Chan, Group};
        let io = |ins: usize, outs: usize| -> Vec<Group> {
            let chans = |n: usize| (0..n).map(|i| Chan { number: i as u32 + 1, ..Chan::silent() }).collect::<Vec<_>>();
            [("IN", ins), ("OUT", outs)]
                .into_iter()
                .filter(|(_, n)| *n > 0)
                .map(|(l, n)| Group { label: format!("{l} {n}"), channels: chans(n) })
                .collect()
        };
        let col_w = rack::col_width(1500.0, rack::columns(1500.0));
        let seg = Geom::card().resolve(MeterStyle::Segments, 1.0, 40.0);
        for (ins, outs) in [(2, 2), (8, 8), (16, 16), (23, 10), (32, 32)] {
            let g = io(ins, outs);
            let fits = |w: f32| !meter_layout(&g, Rect::from_min_size(Pos2::ZERO, Vec2::new(w, 40.0)), &seg).overflow;
            let span = rack::tray_span(col_w, 6, fits);
            if span == rack::MAX_SPAN {
                // Four columns at most: a bigger device's bars narrow to fit (`fit_geom`).
                continue;
            }
            let w = rack::meter_w(span, col_w);
            for style in MeterStyle::all() {
                let full = Geom::card().resolve(style, 1.0, 40.0);
                let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, 40.0));
                assert!(!meter_layout(&g, r, &full).overflow, "{ins}x{outs} {style:?} at {span} columns");
            }
        }
        let huge = io(32, 32);
        let fits = |w: f32| !meter_layout(&huge, Rect::from_min_size(Pos2::ZERO, Vec2::new(w, 40.0)), &seg).overflow;
        assert_eq!(rack::tray_span(col_w, 8, fits), rack::MAX_SPAN, "a 32x32 device takes four columns");
        assert_eq!(
            rack::tray_span(col_w, 6, |w| !meter_layout(
                &io(2, 2),
                Rect::from_min_size(Pos2::ZERO, Vec2::new(w, 40.0)),
                &seg
            )
            .overflow),
            1,
            "a stereo device takes one column"
        );
    }

    #[test]
    fn trays_are_laid_out_in_the_bed_with_each_group_on_new_rows() {
        let (rects, rows) = tray_rects(&[1, 1, 2], &[true, false, true], 3, 100.0, 66.0, Pos2::new(10.0, 20.0));
        assert_eq!(rows, 2);
        assert_eq!(rects[0], Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(100.0, 66.0)));
        assert_eq!(rects[1].min, Pos2::new(118.0, 20.0));
        assert_eq!(rects[2], Rect::from_min_size(Pos2::new(10.0, 94.0), Vec2::new(208.0, 66.0)));
    }
}
