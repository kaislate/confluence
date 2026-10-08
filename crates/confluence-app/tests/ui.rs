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
fn a_click_selects_a_cell_and_space_or_a_double_click_toggles_its_route() {
    let d = EngineDir::new("click");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let mut h = harness_fast(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 2";
    pump_until(&mut h, "the grid", LONG, |h| h.query_by_role_and_label(Role::Button, cell).is_some());
    // A click selects, and routes nothing.
    h.get_by_role_and_label(Role::Button, cell).click();
    settle(&mut h);
    assert_eq!(h.state().selection(), confluence_app::matrix::Selection::Cell { input: i, output: o + 1 });
    std::thread::sleep(Duration::from_millis(600));
    settle(&mut h);
    assert!(h.state().point(i, o + 1).is_none() && !engine_points(&mut c).contains(&(i, o + 1)), "a click routed");
    // Space routes the selected cell.
    h.key_press(eframe::egui::Key::Space);
    pump_until(&mut h, "the route in the engine", LONG, |h| {
        h.state().point(i, o + 1).is_some() && engine_points(&mut c).contains(&(i, o + 1))
    });
    // A double-click removes it again.
    let node = h.get_by_role_and_label(Role::Button, cell);
    node.click();
    node.click();
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
    // A click selects the cell and Space removes its route: the inspector shows its point panel.
    h.get_by_role_and_label(Role::Button, cell).click();
    h.key_press(eframe::egui::Key::Space);
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
fn a_vasio_is_turned_on_from_the_devices_screen() {
    let d = EngineDir::new("devices");
    let _engine = Engine::spawn(&d);
    let mut h = harness_sized(app_for(&d), 1600.0, 1000.0);
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    // The wall is collapsed: only the next switched-off position (B) shows
    // until the group is opened.
    pump_until(&mut h, "the collapsed Virtual group", LONG, |h| {
        h.query_by_label("Show all Virtual positions").is_some()
    });
    assert!(h.query_by_label("Turn on VASIO C").is_none(), "C is folded away");
    h.get_by_label("Show all Virtual positions").click();
    pump_until(&mut h, "VASIO C's card", LONG, |h| h.query_by_label("Turn on VASIO C").is_some());
    h.get_by_label("Turn on VASIO C").click();
    pump_until(&mut h, "the new slot", LONG, |_| slots(&mut client(&d)).iter().any(|s| s.name == "VASIO 3"));
    pump_until(&mut h, "the notification", LONG, |h| h.query_by_label_contains("Turned on VASIO C").is_some());
    pump_until(&mut h, "its Turn off", LONG, |h| h.query_by_label("Turn off VASIO C").is_some());
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
    h.get_by_role_and_label(Role::Button, first).click();
    h.key_press(eframe::egui::Key::Space); // the worker now holds a connection
    pump_until(&mut h, "the first route", LONG, |_| engine_points(&mut client(&d)).contains(&(i, o)));
    engine.kill();
    pump_until(&mut h, "Reconnecting", LONG, |h| h.query_all_by_label_contains("Reconnecting").next().is_some());
    let _engine2 = Engine::spawn(&d);
    pump_until(&mut h, "Live again", LONG, |h| h.query_by_label("Live").is_some());
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, second).click();
    h.key_press(eframe::egui::Key::Space);
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

#[test]
fn an_insert_bus_is_added_routed_and_cannot_loop() {
    let d = EngineDir::new("bus");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, _) = add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "the bus row", LONG, |h| h.query_by_label("Add insert bus").is_some());
    // The list also holds this PC's real devices: bring the row into view first.
    h.get_by_label("Add insert bus").scroll_to_me();
    settle(&mut h);
    h.get_by(|n| n.placeholder() == Some("Bus name")).click();
    settle(&mut h);
    h.get_by(|n| n.placeholder() == Some("Bus name")).type_text("Verb");
    settle(&mut h);
    let r = h.get_by_label("Add insert bus").rect();
    assert!(r.min.y >= 0.0 && r.max.y <= 800.0, "the button is on screen: {r:?}");
    h.get_by_label("Add insert bus").click();
    pump_until(&mut h, "the bus slot", LONG, |_| slots(&mut client(&d)).iter().any(|s| s.is_bus() && s.name == "Verb"));
    pump_until(&mut h, "the notification", LONG, |h| h.query_by_label_contains("Added insert bus Verb").is_some());
    let bus = slots(&mut c).into_iter().find(|s| s.is_bus()).unwrap();
    h.get_by_label("Matrix").click();

    let send = "VASIO 1 in 1 → Verb send 1";
    pump_until(&mut h, "the send cell", LONG, |h| h.query_by_role_and_label(Role::Button, send).is_some());
    h.get_by_role_and_label(Role::Button, send).click();
    h.key_press(eframe::egui::Key::Space);
    pump_until(&mut h, "the send route", LONG, |_| engine_points(&mut c).contains(&(i, bus.first_output)));
    settle(&mut h);
    let looped = "Verb return 1 → Verb send 1";
    h.get_by_role_and_label(Role::Button, looped).click();
    h.key_press(eframe::egui::Key::Space);
    pump_until(&mut h, "the loop error", LONG, |h| h.query_by_label_contains("back into itself").is_some());
    pump_until(&mut h, "no loop route", LONG, |h| h.state().point(bus.first_input, bus.first_output).is_none());
    assert!(!engine_points(&mut c).contains(&(bus.first_input, bus.first_output)));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_plugin_is_loaded_from_the_picker_and_its_gain_set() {
    use eframe::egui::accesskit::{Action as AkAction, ActionData, ActionRequest};
    use egui_kittest::kittest::NodeT;
    let d = EngineDir::new("plugin");
    d.add_test_plugin();
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let add = Command::AddBus { name: "FX".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!() };
    let bus = ids[0];
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the bus header", LONG, |h| h.query_by_label("FX outputs").is_some());
    h.get_by_label("FX outputs").click();
    pump_until(&mut h, "the bus panel", LONG, |h| h.query_by_label("Load plugin…").is_some());
    h.get_by_label("Load plugin…").click();
    settle(&mut h);
    // The picker follows the engine's scan, which may still be running.
    pump_until(&mut h, "the plugin in the picker", LONG, |h| h.query_by_label("Load Confluence Test Gain").is_some());
    settle(&mut h);
    h.get_by_label("Load Confluence Test Gain").click();
    pump_until(&mut h, "the plugin on the bus", LONG, |h| h.query_by_label("Confluence Test Gain").is_some());
    pump_until(&mut h, "the Gain slider", LONG, |h| h.query_by_label("Gain").is_some());
    let (target_node, target_tree) = h.get_by_label("Gain").accesskit_node().locate();
    h.event(eframe::egui::Event::AccessKitActionRequest(ActionRequest {
        action: AkAction::SetValue,
        target_node,
        target_tree,
        data: Some(ActionData::NumericValue(-6.0)),
    }));
    pump_until(&mut h, "the engine's new gain", LONG, |_| {
        let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
        state.bus_plugins.iter().any(|p| p.bus == bus && p.params.first().is_some_and(|q| q.value == -6.0))
    });
    pump_until(&mut h, "the plugin's text", LONG, |h| h.query_by_label("-6.0 dB").is_some());
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_plugins_editor_is_opened_and_closed_from_the_window() {
    let d = EngineDir::new("editor");
    d.add_test_plugin();
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let add = Command::AddBus { name: "FX".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!() };
    let path = d.clap_dir().join("ConfluenceTest.clap").display().to_string();
    let load = Command::LoadPlugin {
        bus: confluence_api::BusRef::Id(ids[0]),
        path,
        plugin_id: "dev.confluence.test.gain".into(),
    };
    assert!(matches!(c.call(load).unwrap(), Response::Applied { .. }));
    let open = |want: bool| {
        let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
        state.bus_plugins.first().is_some_and(|p| p.editor_open == want)
    };
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the bus header", LONG, |h| h.query_by_label("FX outputs").is_some());
    h.get_by_label("FX outputs").click();
    pump_until(&mut h, "the Show editor button", LONG, |h| h.query_by_label("Show editor").is_some());
    settle(&mut h); // the plugin section is laid out on its first frame; click once it has settled
    h.get_by_label("Show editor").click();
    let deadline = std::time::Instant::now() + LONG;
    while !open(true) {
        if std::time::Instant::now() > deadline {
            // Say why: what the engine answers when asked directly.
            let direct = c.call(Command::ShowEditor { bus: confluence_api::BusRef::Id(ids[0]) });
            panic!("the editor did not open from the window; asked directly the engine says {direct:?}");
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    pump_until(&mut h, "the Close editor button", LONG, |h| h.query_by_label("Close editor").is_some());
    settle(&mut h);
    h.get_by_label("Close editor").click();
    pump_until(&mut h, "the editor closed", LONG, |_| open(false));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_scene_saved_from_the_bar_brings_a_route_back() {
    let d = EngineDir::new("scenes");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let set = |gain_db| Command::SetPoint { input: i, output: o, gain_db, mute: false, invert: false };
    assert!(matches!(c.call(set(-6.0)).unwrap(), Response::Applied { .. }));
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the scene bar", LONG, |h| h.query_by_label("+ Scene").is_some());
    h.get_by_label("+ Scene").click();
    pump_until(&mut h, "the scene form", LONG, |h| h.query_by_label("Save scene").is_some());
    h.get_by(|n| n.placeholder() == Some("Scene name")).click();
    settle(&mut h);
    h.get_by(|n| n.placeholder() == Some("Scene name")).type_text("Verse");
    settle(&mut h);
    h.get_by_label("Save scene").click();
    pump_until(&mut h, "the scene button", LONG, |h| h.query_by_label("Scene Verse").is_some());
    assert!(matches!(c.call(set(-30.0)).unwrap(), Response::Applied { .. }));
    settle(&mut h);
    h.get_by_label("Scene Verse").click();
    pump_until(&mut h, "the route back at −6 dB", LONG, |_| {
        let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
        state.points.iter().any(|p| (p.input, p.output) == (i, o) && p.gain_db == -6.0)
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_route_learns_a_midi_control_from_the_window() {
    let d = EngineDir::new("midi");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let (i, o) = add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    let cell = "VASIO 1 in 1 → VASIO 1 out 1";
    pump_until(&mut h, "the cell", LONG, |h| h.query_by_role_and_label(Role::Button, cell).is_some());
    // A click selects the empty cell and Space routes it.
    h.get_by_role_and_label(Role::Button, cell).click();
    h.key_press(eframe::egui::Key::Space);
    pump_until(&mut h, "the route in the engine", LONG, |_| engine_points(&mut c).contains(&(i, o)));
    pump_until(&mut h, "the MIDI Learn button", LONG, |h| h.query_by_label("MIDI Learn").is_some());
    settle(&mut h);
    h.get_by_label("MIDI Learn").click();
    pump_until(&mut h, "learning", LONG, |h| h.query_by_label_contains("Move a control").is_some());
    // A control is moved (injected: no MIDI hardware in tests).
    let moved = Command::InjectMidi { device: "Test Controller".into(), bytes: vec![0xB0, 21, 64] };
    assert!(matches!(c.call(moved).unwrap(), Response::Applied { .. }));
    pump_until(&mut h, "the binding", LONG, |h| h.query_by_label("CC 21 · ch 1 · Test Controller").is_some());
    settle(&mut h);
    h.get_by_label("Forget").click();
    pump_until(&mut h, "no binding in the engine", LONG, |_| {
        let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
        state.midi_bindings.is_empty()
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_script_is_written_in_the_window_and_runs_in_the_engine() {
    let d = EngineDir::new("scripts");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the Scripts button", LONG, |h| h.query_by_label("Scripts…").is_some());
    settle(&mut h);
    h.get_by_label("Scripts…").click();
    pump_until(&mut h, "the scripts window", LONG, |h| h.query_by_label("New script").is_some());
    h.get_by_label("New script").click();
    pump_until(&mut h, "the editor", LONG, |h| h.query_by_label("Save script").is_some());
    h.get_by_label("Save script").click();
    let scripts = |d: &EngineDir| {
        let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
        state.scripts
    };
    pump_until(&mut h, "the script in the engine", LONG, |_| !scripts(&d).is_empty());
    let saved = &scripts(&d)[0];
    assert_eq!((saved.name.as_str(), &saved.status), ("Script 1", &confluence_api::ScriptStatus::Running));
    // The example toggles route 0 → 1 on note 36.
    let route = Command::SetPoint { input: 0, output: 1, gain_db: -6.0, mute: false, invert: false };
    assert!(matches!(c.call(route).unwrap(), Response::Applied { .. }));
    let pad = Command::InjectMidi { device: "Test Pad".into(), bytes: vec![0x90, 36, 100] };
    assert!(matches!(c.call(pad).unwrap(), Response::Applied { .. }));
    let (state, _sub) = confluence_client::Subscription::connect(&d.pipe, Duration::from_secs(5)).unwrap();
    assert!(state.points.iter().any(|p| (p.input, p.output, p.mute) == (0, 1, true)), "{:?}", state.points);
    pump_until(&mut h, "running in the list", LONG, |h| h.query_by_label("running").is_some());
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_stream_from_another_engine_is_received_from_its_net_in_picker() {
    use confluence_net::host::{NetHost, SendSpec};
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let d = EngineDir::new("net");
    let _engine = Engine::spawn_with(&d, &["--net-port", &port.to_string()]);
    let mut c = client(&d);
    // Another engine, played by this test: a stream to ours on loopback.
    let other = NetHost::start("127.0.0.1:0".parse().unwrap(), 77).unwrap();
    let dest = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let (mut side, _tx) = other.add_sender(SendSpec { dest, stream: "Guest".into(), channels: 2, rate: 48_000 });
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let block = confluence_core::buffer::PlanarBuffer::new(2, 256);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                side.write(&block, 0);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "NET IN 1", LONG, |h| h.query_by_label_contains("NET IN 1 · click").is_some());
    h.get_by_label_contains("NET IN 1 · click").scroll_to_me();
    settle(&mut h);
    h.get_by_label_contains("NET IN 1 · click").click();
    let add = "Receive 127.0.0.1/Guest";
    pump_until(&mut h, "the heard stream", LONG, |h| h.query_by_label(add).is_some());
    settle(&mut h);
    h.get_by_label(add).click();
    pump_until(&mut h, "the receive slot in the engine", LONG, |_| {
        slots(&mut c).iter().any(|s| s.device == "net-in:127.0.0.1/Guest" && s.inputs == 2)
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    feeder.join().unwrap();
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_devices_colour_is_picked_in_the_inspector() {
    let d = EngineDir::new("colour");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    add_vasio(&mut c, 1);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "the slot header", LONG, |h| h.query_by_label("VASIO 1 inputs").is_some());
    h.get_by_label("VASIO 1 inputs").click();
    let first = confluence_app::skin::Look::builtin().skin.slot_colors[0];
    let rgb = [first.r(), first.g(), first.b()];
    let swatch = format!("Colour #{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    pump_until(&mut h, "the colour swatches", LONG, |h| h.query_by_label(&swatch).is_some());
    // The rack panel animates into place: click once the swatch stops moving
    // (a slow machine could otherwise click where it was a frame ago).
    let mut last = h.get_by_label(&swatch).rect();
    pump_until(&mut h, "the swatch to settle", LONG, |h| {
        let now = h.get_by_label(&swatch).rect();
        let still = now == last;
        last = now;
        still
    });
    settle(&mut h);
    h.get_by_label(&swatch).click();
    pump_until(&mut h, "the device coloured in the engine", LONG, |_| {
        slots(&mut c).iter().any(|s| s.name == "VASIO 1" && s.color == Some(rgb))
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn the_skin_is_chosen_in_settings() {
    let d = EngineDir::new("skin");
    let _engine = Engine::spawn(&d);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Settings…").click();
    pump_until(&mut h, "the Settings window", LONG, |h| h.query_by_label("Silver").is_some());
    h.get_by_label("Silver").click();
    settle(&mut h);
    assert_eq!(h.state().finish(), confluence_app::gear::skins::Finish::Silver);
    client(&d).call(Command::Shutdown).unwrap();
}

#[test]
fn the_devices_screen_shows_positions_and_vasio_a_is_on() {
    let d = EngineDir::new("screen");
    let _engine = Engine::spawn(&d);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    // The card's own label (its pills say "Turn off VASIO A" and so on).
    pump_until(&mut h, "the VASIO A card", LONG, |h| h.query_by_label_contains("VASIO A · ").is_some());
    assert!(h.query_by_label_contains("ASIO 1 · click to choose a device").is_some());
    assert!(h.query_by_label_contains("NO DAW").is_some());
    client(&d).call(Command::Shutdown).unwrap();
}

#[test]
fn turning_a_vasio_on_from_its_card_reaches_the_engine() {
    let d = EngineDir::new("vasio-on");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let mut h = harness_sized(app_for(&d), 1600.0, 1000.0);
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "VASIO B's Turn on", LONG, |h| h.query_by_label("Turn on VASIO B").is_some());
    h.get_by_label("Turn on VASIO B").click();
    pump_until(&mut h, "VASIO B in the engine", LONG, |_| slots(&mut c).iter().any(|s| s.name == "VASIO 2"));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn an_empty_network_slot_is_filled_from_its_picker() {
    let d = EngineDir::new("fill");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let mut h = harness_sized(app_for(&d), 1600.0, 1000.0);
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "NET OUT 1", LONG, |h| h.query_by_label_contains("NET OUT 1 · click").is_some());
    // The network rows are at the bottom of the screen: bring the card into view.
    h.get_by_label_contains("NET OUT 1 · click").scroll_to_me();
    settle(&mut h);
    h.get_by_label_contains("NET OUT 1 · click").click();
    pump_until(&mut h, "the send picker", LONG, |h| {
        h.query_by(|n| n.placeholder() == Some("Address (ip:port)")).is_some()
    });
    h.get_by(|n| n.placeholder() == Some("Address (ip:port)")).click();
    h.get_by(|n| n.placeholder() == Some("Address (ip:port)")).type_text("127.0.0.1:9");
    h.get_by_label("Send here").click();
    pump_until(&mut h, "a send slot", LONG, |_| slots(&mut c).iter().any(|s| s.device.starts_with("net-out:")));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_filled_position_is_cleared_after_confirming() {
    let d = EngineDir::new("clear");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let pos: confluence_api::PosId = "net-out:1".parse().unwrap();
    let fill = Command::FillPosition { pos, kind: DeviceKind::NetSend, name: "127.0.0.1:9/Main:2".into() };
    assert!(matches!(c.call(fill).unwrap(), Response::Added { .. }));
    let mut h = harness_sized(app_for(&d), 1600.0, 1000.0);
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "the Clear pill", LONG, |h| h.query_by_label("Clear NET OUT 1").is_some());
    h.get_by_label("Clear NET OUT 1").scroll_to_me();
    settle(&mut h);
    h.get_by_label("Clear NET OUT 1").click();
    pump_until(&mut h, "the question", LONG, |h| h.query_by_label_contains("Its routes are removed").is_some());
    h.get_by_label("Clear").click();
    pump_until(&mut h, "the slot gone", LONG, |_| !slots(&mut c).iter().any(|s| s.device.starts_with("net-out:")));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn turning_a_vasio_off_asks_first() {
    let d = EngineDir::new("vasio-off");
    let _engine = Engine::spawn(&d);
    let mut c = client(&d);
    let mut h = harness_sized(app_for(&d), 1600.0, 1000.0);
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Devices").click();
    pump_until(&mut h, "VASIO A's Turn off", LONG, |h| h.query_by_label("Turn off VASIO A").is_some());
    h.get_by_label("Turn off VASIO A").click();
    pump_until(&mut h, "the question", LONG, |h| h.query_by_label_contains("Its routes are removed").is_some());
    assert!(slots(&mut c).iter().any(|s| s.name == "VASIO 1"), "nothing happens before the answer");
    h.get_by_label("Turn off").click();
    pump_until(&mut h, "VASIO A off", LONG, |_| !slots(&mut c).iter().any(|s| s.name == "VASIO 1"));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn the_meter_style_is_chosen_in_settings() {
    let d = EngineDir::new("meter-style");
    let _engine = Engine::spawn(&d);
    let mut h = harness(app_for(&d));
    pump_until(&mut h, "Live", LONG, |h| h.query_by_label("Live").is_some());
    h.get_by_label("Settings…").click();
    pump_until(&mut h, "the meter choices", LONG, |h| h.query_by_label("Dot-matrix").is_some());
    h.get_by_label("Dot-matrix").click();
    h.get_by_label("Double line").click();
    h.get_by_label("Show only custom names").click();
    settle(&mut h);
    let p = h.state().prefs();
    assert_eq!(p.meter.style, confluence_app::gear::oled_meter::MeterStyle::DotMatrix);
    assert!(p.meter.double_peak && p.only_custom_names);
    client(&d).call(Command::Shutdown).unwrap();
}
