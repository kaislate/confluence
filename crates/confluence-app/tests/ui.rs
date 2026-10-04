//! The window, driven headlessly against the real engine.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use confluence_api::{Command, DeviceKind, Response};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use support::*;

const LONG: Duration = Duration::from_secs(15);

/// Adds VASIO instance `n` (a slot with 2 inputs and 2 outputs) and returns
/// its (first input, first output).
fn add_vasio(c: &mut confluence_client::Client, n: u32) -> (u32, u32) {
    let r = c.call(Command::AddDevice { kind: DeviceKind::Vasio, name: n.to_string() }).unwrap();
    assert!(matches!(r, Response::Added { .. }), "{r:?}");
    let s = slots(c).into_iter().find(|s| s.name == format!("VASIO {n}")).unwrap();
    (s.first_input, s.first_output)
}

/// The engine's routes as (input, output).
fn engine_points(c: &mut confluence_client::Client) -> Vec<(u32, u32)> {
    match c.call(Command::ListPoints).unwrap() {
        Response::Points(p) => p.into_iter().map(|p| (p.input, p.output)).collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_engine_can_be_started_from_the_window() {
    let d = EngineDir::new("start");
    let _cleanup = ShutdownOnDrop(d.pipe.clone());
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the not-running banner", LONG, |h| h.query_by_label("Engine not running").is_some());
    h.get_by_label("Start engine").click();
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
}

#[test]
fn clicking_cells_routes_and_unroutes() {
    let d = EngineDir::new("click");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 2";
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_role_and_label(Role::Button, cell).is_some());
    h.get_by_role_and_label(Role::Button, cell).click();
    // The cell shows the pending route at once; wait for the engine to have it.
    pump_until(&mut h, "the route in the engine", LONG, |h| {
        h.state().point(i, o + 1).is_some() && engine_points(&mut c).contains(&(i, o + 1))
    });
    std::thread::sleep(Duration::from_millis(600)); // not a double-click
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, cell).click();
    pump_until(&mut h, "the route gone from the engine", LONG, |h| {
        h.state().point(i, o + 1).is_none() && !engine_points(&mut c).contains(&(i, o + 1))
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn another_clients_route_appears_without_interaction() {
    let d = EngineDir::new("other");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    c.call(Command::SetPoint { input: i, output: o, gain_db: -6.0, mute: true, invert: false }).unwrap();
    pump_until(&mut h, "the other client's route", LONG, |h| {
        h.state().point(i, o).is_some_and(|p| p.gain_db == -6.0 && p.mute)
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_restarted_engine_is_followed() {
    let d = EngineDir::new("restart");
    let mut engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    c.call(Command::SetPoint { input: i, output: o, gain_db: 0.0, mute: false, invert: false }).unwrap();
    drop(c);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    engine.kill();
    // Both the badge ("Reconnecting (N s)") and the banner ("Reconnecting…") say it.
    pump_until(&mut h, "Reconnecting", LONG, |h| h.query_all_by_label_contains("Reconnecting").next().is_some());
    let _engine2 = Engine::spawn(&d);
    pump_until(&mut h, "Live again", LONG, |h| h.query_by_label("Live").is_some());
    pump_until(&mut h, "the journaled route", LONG, |h| h.state().point(i, o).is_some());
    client(&d).call(Command::Shutdown).unwrap();
}

#[test]
fn a_skin_folder_is_applied() {
    let d = EngineDir::new("skin");
    let skin = tempfile::tempdir().unwrap();
    image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255]))
        .save_with_format(skin.path().join("cell_on.png"), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(
        skin.path().join("skin.toml"),
        "name = \"Test\"\n[colors]\naccent = \"#ff0000\"\n[images]\ncell_routed = \"cell_on.png\"\n",
    )
    .unwrap();
    let mut h = harness(app_with_skin(&d, Some(skin.path().to_path_buf())));
    settle(&mut h);
    assert_eq!(h.state().look().skin.name, "Test");
    assert!(h.state().look().has_image("cell_routed"));
    assert!(h.query_by_label_contains("Skin:").is_none(), "no warnings");
}

#[test]
fn a_broken_skin_warns_and_the_window_still_works() {
    let d = EngineDir::new("badskin");
    let skin = tempfile::tempdir().unwrap();
    std::fs::write(skin.path().join("skin.toml"), "name = ").unwrap();
    let mut h = harness(app_with_skin(&d, Some(skin.path().to_path_buf())));
    pump_until(&mut h, "the skin warning", LONG, |h| h.query_by_label_contains("Skin:").is_some());
    assert_eq!(h.state().look().skin.name, "Built-in");
    pump_until(&mut h, "the banner", LONG, |h| h.query_by_label("Engine not running").is_some());
}

#[test]
fn the_inspector_shows_a_vasio_slots_idle_note() {
    let d = EngineDir::new("idle");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the slot header", LONG, |h| h.query_by_label("VASIO 1 inputs").is_some());
    h.get_by_label("VASIO 1 inputs").click();
    pump_until(&mut h, "the idle note", LONG, |h| h.query_by_label_contains("no DAW attached").is_some());
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn the_inspector_edits_the_selected_route() {
    let d = EngineDir::new("inspect");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    c.call(Command::SetPoint { input: i, output: o, gain_db: -6.0, mute: false, invert: false }).unwrap();
    let mut h = harness(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 1";
    pump_until(&mut h, "the routed cell", LONG, |h| {
        h.state().point(i, o).is_some() && h.query_by_role_and_label(Role::Button, cell).is_some()
    });
    // A click removes the route and selects the cell: the inspector shows its point panel.
    h.get_by_role_and_label(Role::Button, cell).click();
    pump_until(&mut h, "unrouted in the engine", LONG, |_| !engine_points(&mut c).contains(&(i, o)));
    pump_until(&mut h, "the point panel", LONG, |h| h.query_by_label("Route at 0 dB").is_some());
    settle(&mut h); // let the panel's layout settle before clicking in it
    h.get_by_label("Route at 0 dB").click();
    pump_until(&mut h, "routed again in the engine", LONG, |_| engine_points(&mut c).contains(&(i, o)));
    pump_until(&mut h, "the Mute checkbox", LONG, |h| h.query_by_label("Mute").is_some());
    settle(&mut h);
    h.get_by_label("Mute").click();
    pump_until(&mut h, "muted in the engine", LONG, |_| match c.call(Command::ListPoints).unwrap() {
        Response::Points(p) => p.iter().any(|p| (p.input, p.output, p.mute) == (i, o, true)),
        _ => false,
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_slot_can_be_removed_after_confirming() {
    let d = EngineDir::new("remove");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the slot header", LONG, |h| h.query_by_label("VASIO 1 outputs").is_some());
    h.get_by_label("VASIO 1 outputs").click();
    pump_until(&mut h, "the slot panel", LONG, |h| h.query_by_label("Remove slot…").is_some());
    h.get_by_label("Remove slot…").click();
    pump_until(&mut h, "the confirmation", LONG, |h| h.query_by_label_contains("routes are removed too").is_some());
    settle(&mut h); // the dialog is sized on its first frame and centred on the next
    h.get_by_label("Remove").click();
    pump_until(&mut h, "the slot gone", LONG, |_| slots(&mut client(&d)).is_empty());
    client(&d).call(Command::Shutdown).unwrap();
}

#[test]
fn a_selection_removed_elsewhere_is_cleared() {
    let d = EngineDir::new("gone");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    add_vasio(&mut c, 1);
    let id = slots(&mut c)[0].id;
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the slot header", LONG, |h| h.query_by_label("VASIO 1 inputs").is_some());
    h.get_by_label("VASIO 1 inputs").click();
    pump_until(&mut h, "selected", LONG, |h| h.state().selection() == confluence_app::matrix::Selection::Slot(id));
    c.call(Command::RemoveSlot { id }).unwrap();
    pump_until(&mut h, "the selection cleared", LONG, |h| {
        h.state().selection() == confluence_app::matrix::Selection::None
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_device_can_be_added_from_the_devices_panel() {
    let d = EngineDir::new("devices");
    let _engine = Engine::spawn(&d);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices…").click();
    pump_until(&mut h, "the VASIO entries", LONG, |h| h.query_by_label("Add VASIO 3").is_some());
    // The list also holds this PC's real devices, so VASIO 3 may be scrolled out of
    // view: bring it into view and make sure the click lands on it, never elsewhere.
    h.get_by_label("Add VASIO 3").scroll_to_me();
    settle(&mut h);
    let r = h.get_by_label("Add VASIO 3").rect();
    assert!(r.min.y >= 0.0 && r.max.y <= 800.0, "the button is on screen: {r:?}");
    h.get_by_label("Add VASIO 3").click();
    pump_until(&mut h, "the new slot", LONG, |_| slots(&mut client(&d)).iter().any(|s| s.name == "VASIO 3"));
    pump_until(&mut h, "the notification", LONG, |h| h.query_by_label_contains("Added VASIO 3").is_some());
    pump_until(&mut h, "in use", LONG, |h| h.query_by_label("Add VASIO 3").is_none());
    client(&d).call(Command::Shutdown).unwrap();
}

/// Adds VASIO 1 with 16 channels each way (a grid larger than a small window).
fn add_big_vasio(c: &mut confluence_client::Client) -> (u32, u32) {
    let r = c.call(Command::AddDevice { kind: DeviceKind::Vasio, name: "1:16x16".into() }).unwrap();
    assert!(matches!(r, Response::Added { .. }), "{r:?}");
    let s = slots(c).into_iter().find(|s| s.name == "VASIO 1").unwrap();
    (s.first_input, s.first_output)
}

#[test]
fn the_wheel_on_a_route_changes_gain_without_scrolling() {
    let d = EngineDir::new("wheel");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_big_vasio(&mut c);
    c.call(Command::SetPoint { input: i, output: o, gain_db: -6.0, mute: false, invert: false }).unwrap();
    let mut h = harness_sized(app_for(&d), 520.0, 300.0);
    let cell = "VASIO 1 in 1 → VASIO 1 out 1";
    pump_until(&mut h, "the routed cell", LONG, |h| {
        h.state().point(i, o).is_some() && h.query_by_role_and_label(Role::Button, cell).is_some()
    });
    settle(&mut h);
    let before = h.get_by_role_and_label(Role::Button, cell).rect();
    h.hover_at(before.center());
    h.step();
    h.event(eframe::egui::Event::MouseWheel {
        unit: eframe::egui::MouseWheelUnit::Line,
        delta: eframe::egui::vec2(0.0, -1.0),
        modifiers: Default::default(),
        phase: eframe::egui::TouchPhase::Move,
    });
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = h.get_by_role_and_label(Role::Button, cell).rect();
    assert_eq!(before, after, "the grid did not scroll");
    pump_until(&mut h, "one step down in the engine", LONG, |_| match c.call(Command::ListPoints).unwrap() {
        Response::Points(p) => p.iter().any(|p| (p.input, p.output) == (i, o) && (p.gain_db - -7.0).abs() < 1e-4),
        _ => false,
    });
    c.call(Command::Shutdown).unwrap();
}

/// A cell scrolled partly under the sticky headers must not take clicks (or
/// hovers) there: the header's channel numbers would otherwise toggle a route
/// the user cannot see.
#[test]
fn cells_under_the_sticky_headers_cannot_be_hit() {
    let d = EngineDir::new("headers");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    add_big_vasio(&mut c);
    let mut h = harness_sized(app_for(&d), 520.0, 300.0);
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_label("VASIO 1 inputs").is_some());
    settle(&mut h);
    // Scroll by a part of a cell, with the pointer on the row header (not a cell).
    h.hover_at(h.get_by_label("VASIO 1 inputs").rect().center());
    h.step();
    h.event(eframe::egui::Event::MouseWheel {
        unit: eframe::egui::MouseWheelUnit::Point,
        delta: eframe::egui::vec2(-25.0, -25.0),
        modifiers: Default::default(),
        phase: eframe::egui::TouchPhase::Move,
    });
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let header_bottom = h.get_by_label("VASIO 1 outputs").rect().min.y + confluence_app::grid_view::HEADER_H;
    let header_right = h.get_by_label("VASIO 1 inputs").rect().max.x;
    let cells: Vec<_> = h.query_all_by_label_contains(" → ").map(|n| n.rect()).collect();
    assert!(!cells.is_empty());
    for r in cells {
        assert!(
            r.min.y >= header_bottom - 0.5 && r.min.x >= header_right - 0.5,
            "a cell reaches under a header: {r:?}"
        );
    }
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn the_first_edit_after_an_engine_restart_works() {
    let d = EngineDir::new("restart-edit");
    let mut engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    drop(c);
    let mut h = harness(app_for(&d));
    let first = "VASIO 1 in 1 → VASIO 1 out 1";
    let second = "VASIO 1 in 2 → VASIO 1 out 2";
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_role_and_label(Role::Button, first).is_some());
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, first).click(); // the worker now holds a connection
    pump_until(&mut h, "the first route", LONG, |_| engine_points(&mut client(&d)).contains(&(i, o)));
    engine.kill();
    pump_until(&mut h, "Reconnecting", LONG, |h| h.query_all_by_label_contains("Reconnecting").next().is_some());
    let _engine2 = Engine::spawn(&d);
    pump_until(&mut h, "Live again", LONG, |h| h.query_by_label("Live").is_some());
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, second).click();
    pump_until(&mut h, "the edit's outcome", LONG, |h| {
        h.query_by_label_contains("lost the engine").is_some()
            || engine_points(&mut client(&d)).contains(&(i + 1, o + 1))
    });
    assert!(h.query_by_label_contains("lost the engine").is_none(), "the edit failed on the old connection");
    client(&d).call(Command::Shutdown).unwrap();
}

/// While the engine is away the grid is read-only, but cells can still be
/// selected to look at them in the inspector (as slot headers can).
#[test]
fn cells_can_be_selected_while_disconnected() {
    let d = EngineDir::new("offline-select");
    let mut engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    drop(c);
    let mut h = harness(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 1";
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_role_and_label(Role::Button, cell).is_some());
    engine.kill();
    pump_until(&mut h, "Reconnecting", LONG, |h| h.query_all_by_label_contains("Reconnecting").next().is_some());
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, cell).click();
    settle(&mut h);
    assert_eq!(h.state().selection(), confluence_app::matrix::Selection::Cell { input: i, output: o });
    assert!(h.state().point(i, o).is_none(), "no edit while disconnected");
}

/// A press that moves more than 3 px before release is not a click (spec
/// §4.1), so a small wobble on a cell never toggles its route.
#[test]
fn a_press_that_moves_more_than_three_pixels_is_not_a_click() {
    use eframe::egui::{Event, PointerButton};
    let d = EngineDir::new("click-dist");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 1";
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_role_and_label(Role::Button, cell).is_some());
    settle(&mut h);
    let p = h.get_by_role_and_label(Role::Button, cell).rect().center();
    let q = p + eframe::egui::vec2(0.0, 4.0);
    let button = |pos, pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    h.event(Event::PointerMoved(p));
    h.event(button(p, true));
    h.step();
    h.event(Event::PointerMoved(q));
    h.step();
    h.event(button(q, false));
    for _ in 0..20 {
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.state().point(i, o).is_none(), "a 4 px wobble routed the cell");
    assert!(!engine_points(&mut c).contains(&(i, o)));
    c.call(Command::Shutdown).unwrap();
}
