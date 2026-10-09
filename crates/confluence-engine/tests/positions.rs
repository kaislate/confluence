//! Fixed positions in the device manager, in-process with fake ASIO drivers
//! and the test-isolated VASIO namespace.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use confluence_api::{Command, DeviceKind, PosId, PositionStatus, Response};
use confluence_engine::devices::{AsioOpener, DeviceManager};
use confluence_engine::{Engine, EngineConfig};
use confluence_provider_asio::fake::{FakeConfig, FakeProbe};
use confluence_provider_asio::{AsioDevice, AsioHostError, DriverSource};

fn setup() {
    confluence_provider_vasio::isolate_for_tests();
}
fn p(s: &str) -> PosId {
    s.parse().unwrap()
}
fn fake(names: &'static [&'static str]) -> AsioOpener {
    Box::new(move |name: &str| {
        if !names.contains(&name) {
            return Err(AsioHostError::NotInstalled(name.into()));
        }
        let mut cfg = FakeConfig::new(name);
        cfg.probe = Arc::new(FakeProbe::default());
        AsioDevice::open(DriverSource::Fake(cfg))
    })
}
fn status(d: &DeviceManager, e: &Engine, pos: &str) -> PositionStatus {
    d.positions(e).into_iter().find(|s| s.pos == p(pos)).unwrap().status
}

#[test]
fn a_fresh_setup_has_vasio_a_on_at_8x8_and_everything_else_off_or_empty() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (devices, _) = DeviceManager::open_file(dir.path().join("devices.json"));
    let mut d = devices.with_asio_opener(fake(&[]));
    assert!(d.restore(&mut e).is_empty());
    let a = d.positions(&e).into_iter().find(|s| s.pos == p("vasio:A")).unwrap();
    assert_eq!(a.status, PositionStatus::On { online: false }, "on, no DAW yet");
    assert_eq!(a.shape, Some((8, 8)));
    let slot = e.slots().into_iter().find(|s| a.slots.contains(&s.id)).unwrap();
    assert_eq!((slot.inputs, slot.outputs), (8, 8));
    assert_eq!(status(&d, &e, "vasio:B"), PositionStatus::Off);
    assert_eq!(status(&d, &e, "asio:1"), PositionStatus::Empty);
    assert_eq!(d.positions(&e).len(), confluence_api::all_positions().len());
}

#[test]
fn devices_fill_the_next_free_position_or_the_one_asked_for() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake(&["fake:a", "fake:b"]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    assert_eq!(status(&d, &e, "asio:1"), PositionStatus::Filled { online: true });
    let r =
        d.handle(&mut e, &Command::FillPosition { pos: p("asio:3"), kind: DeviceKind::Asio, name: "fake:b".into() });
    assert!(matches!(r, Some(Response::Ok)), "{r:?}");
    assert_eq!(status(&d, &e, "asio:3"), PositionStatus::Filled { online: true });
    assert_eq!(status(&d, &e, "asio:2"), PositionStatus::Empty);
    let wrong =
        d.handle(&mut e, &Command::FillPosition { pos: p("win-out:1"), kind: DeviceKind::Asio, name: "fake:b".into() });
    assert!(matches!(wrong, Some(Response::Error(_))), "an ASIO driver does not go in WIN OUT");
}

#[test]
fn our_own_vasio_drivers_are_refused_as_asio_devices() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    let err = d.add(&mut e, DeviceKind::Asio, "Confluence VASIO 2").unwrap_err();
    assert!(err.contains("turn on VASIO B"), "{err}");
}

#[test]
fn clearing_a_position_removes_its_slot_and_routes() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake(&["fake:a"]));
    let ids = d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let inp = e.slots().into_iter().find(|s| ids.contains(&s.id) && s.inputs > 0).unwrap().first_input;
    e.handle(&Command::SetPoint { input: inp, output: inp, gain_db: 0.0, mute: false, invert: false });
    assert!(matches!(d.handle(&mut e, &Command::ClearPosition { pos: p("asio:1") }), Some(Response::Ok)));
    assert_eq!(status(&d, &e, "asio:1"), PositionStatus::Empty);
    assert!(e.slots().iter().all(|s| !ids.contains(&s.id)));
    assert!(e.settled_points().is_empty());
}

#[test]
fn a_vasio_turns_on_and_off_repeatedly_without_leaving_anything_behind() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    for _ in 0..3 {
        let on = Command::SetVirtual { pos: p("vasio:B"), on: true, shape: Some((2, 2)) };
        assert!(matches!(d.handle(&mut e, &on), Some(Response::Ok)));
        assert_eq!(status(&d, &e, "vasio:B"), PositionStatus::On { online: false });
        let off = Command::SetVirtual { pos: p("vasio:B"), on: false, shape: None };
        assert!(matches!(d.handle(&mut e, &off), Some(Response::Ok)));
        assert_eq!(status(&d, &e, "vasio:B"), PositionStatus::Off);
    }
    assert!(e.slots().iter().all(|s| !s.name.starts_with("VASIO B")), "no stray slot");
}

#[test]
fn positions_persist_in_devices_json_version_2() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let shape_c = {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (devices, _) = DeviceManager::open_file(path.clone());
        let mut d = devices.with_asio_opener(fake(&["fake:a"]));
        d.restore(&mut e);
        d.handle(&mut e, &Command::FillPosition { pos: p("asio:2"), kind: DeviceKind::Asio, name: "fake:a".into() });
        d.handle(&mut e, &Command::SetVirtual { pos: p("vasio:C"), on: true, shape: Some((4, 16)) });
        d.positions(&e).into_iter().find(|x| x.pos == p("vasio:C")).unwrap().shape
    };
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"version\": 2") && text.contains("\"asio:2\"") && text.contains("\"vasio:C\""), "{text}");
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (devices, warnings) = DeviceManager::open_file(path);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut d = devices.with_asio_opener(fake(&["fake:a"]));
    d.restore(&mut e);
    assert_eq!(status(&d, &e, "asio:2"), PositionStatus::Filled { online: true });
    assert_eq!(d.positions(&e).into_iter().find(|x| x.pos == p("vasio:C")).unwrap().shape, shape_c);
}

#[test]
fn a_colour_belongs_to_the_position() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    d.handle(&mut e, &Command::SetVirtual { pos: p("vasio:A"), on: true, shape: Some((2, 2)) });
    let id = d.positions(&e).into_iter().find(|x| x.pos == p("vasio:A")).unwrap().slots[0];
    assert_eq!(e.color_key(id).as_deref(), Some("pos:vasio:A"));
}

fn fake_sized(spec: &'static [(&'static str, usize, usize)]) -> AsioOpener {
    Box::new(move |name: &str| {
        let (_, i, o) = spec.iter().find(|(n, _, _)| *n == name).ok_or(AsioHostError::NotInstalled(name.into()))?;
        let mut cfg = FakeConfig::new(name);
        cfg.inputs = *i;
        cfg.outputs = *o;
        AsioDevice::open(DriverSource::Fake(cfg))
    })
}
fn route(e: &mut Engine, i: u32, o: u32) {
    assert_eq!(
        e.handle(&Command::SetPoint { input: i, output: o, gain_db: -3.0, mute: false, invert: false }),
        Response::Ok
    );
}
fn slot_of(d: &DeviceManager, e: &Engine, pos: &str, inputs: bool) -> confluence_api::SlotState {
    let ids = d.positions(e).into_iter().find(|s| s.pos == p(pos)).unwrap().slots;
    e.slots().into_iter().find(|s| ids.contains(&s.id) && if inputs { s.inputs > 0 } else { s.outputs > 0 }).unwrap()
}

#[test]
fn swapping_to_a_same_size_device_keeps_routes_exactly() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake_sized(&[("fake:a", 2, 2), ("fake:b", 2, 2)]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let inp = slot_of(&d, &e, "asio:1", true).first_input;
    route(&mut e, inp, 0);
    let r =
        d.handle(&mut e, &Command::FillPosition { pos: p("asio:1"), kind: DeviceKind::Asio, name: "fake:b".into() });
    assert!(matches!(r, Some(Response::Ok)), "{r:?}");
    assert_eq!(slot_of(&d, &e, "asio:1", true).first_input, inp, "not moved");
    assert_eq!(
        e.points_in(Some((0, 4096)), Some((0, 4096))).iter().map(|p| (p.input, p.output)).collect::<Vec<_>>(),
        vec![(inp, 0)]
    );
    assert_eq!(d.positions(&e).into_iter().find(|s| s.pos == p("asio:1")).unwrap().device.unwrap().name, "fake:b");
}

#[test]
fn a_bigger_device_with_no_room_after_it_moves_and_takes_its_routes() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake_sized(&[
        ("fake:a", 2, 2),
        ("fake:next", 2, 2),
        ("fake:big", 8, 2),
    ]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    d.add(&mut e, DeviceKind::Asio, "fake:next").unwrap(); // takes the inputs right after fake:a
    let old = slot_of(&d, &e, "asio:1", true).first_input;
    route(&mut e, old + 1, 0);
    let r =
        d.handle(&mut e, &Command::FillPosition { pos: p("asio:1"), kind: DeviceKind::Asio, name: "fake:big".into() });
    assert!(matches!(r, Some(Response::Ok)), "{r:?}");
    let new = slot_of(&d, &e, "asio:1", true);
    assert_eq!(new.inputs, 8);
    assert_ne!(new.first_input, old, "moved to a free block");
    let pts: Vec<_> = e.points_in(Some((0, 4096)), Some((0, 4096))).iter().map(|p| (p.input, p.output)).collect();
    assert_eq!(pts, vec![(new.first_input + 1, 0)], "the route followed its channel");
}

#[test]
fn a_smaller_device_drops_only_routes_on_the_lost_channels() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake_sized(&[("fake:wide", 4, 2), ("fake:narrow", 2, 2)]));
    d.add(&mut e, DeviceKind::Asio, "fake:wide").unwrap();
    let f = slot_of(&d, &e, "asio:1", true).first_input;
    for k in 0..4 {
        route(&mut e, f + k, 0);
    }
    d.handle(&mut e, &Command::FillPosition { pos: p("asio:1"), kind: DeviceKind::Asio, name: "fake:narrow".into() });
    let mut ins: Vec<u32> = e.points_in(Some((0, 4096)), Some((0, 4096))).iter().map(|p| p.input).collect();
    ins.sort();
    assert_eq!(ins, vec![f, f + 1]);
}

#[test]
fn a_failed_swap_leaves_the_old_device_in_place() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake_sized(&[("fake:a", 2, 2)]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let before = slot_of(&d, &e, "asio:1", true);
    let r = d.handle(
        &mut e,
        &Command::FillPosition { pos: p("asio:1"), kind: DeviceKind::Asio, name: "fake:missing".into() },
    );
    assert!(matches!(r, Some(Response::Error(_))));
    assert_eq!(slot_of(&d, &e, "asio:1", true).first_input, before.first_input);
    assert_eq!(status(&d, &e, "asio:1"), PositionStatus::Filled { online: true });
}

#[test]
fn reshaping_a_vasio_keeps_routes_on_the_channels_that_remain() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    d.handle(&mut e, &Command::SetVirtual { pos: p("vasio:A"), on: true, shape: Some((8, 8)) });
    let s = slot_of(&d, &e, "vasio:A", false);
    route(&mut e, 0, s.first_output + 1);
    route(&mut e, 0, s.first_output + 6);
    assert!(matches!(
        d.handle(&mut e, &Command::SetVirtual { pos: p("vasio:A"), on: true, shape: Some((2, 2)) }),
        Some(Response::Ok)
    ));
    let s2 = slot_of(&d, &e, "vasio:A", false);
    assert_eq!((s2.outputs, s2.first_output), (2, s.first_output));
    let outs: Vec<u32> = e.points_in(Some((0, 4096)), Some((0, 4096))).iter().map(|p| p.output).collect();
    assert_eq!(outs, vec![s.first_output + 1], "channel 7 went away with its route");
}

#[test]
fn a_device_with_no_room_anywhere_is_refused_and_the_old_one_stays() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake_sized(&[("fake:a", 2, 2), ("fake:huge", 100_000, 2)]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let inp = slot_of(&d, &e, "asio:1", true).first_input;
    route(&mut e, inp, 0);
    let r =
        d.handle(&mut e, &Command::FillPosition { pos: p("asio:1"), kind: DeviceKind::Asio, name: "fake:huge".into() });
    assert_eq!(r, Some(Response::Error("no room for 100000 inputs: remove something first".into())));
    assert_eq!(d.positions(&e).into_iter().find(|s| s.pos == p("asio:1")).unwrap().device.unwrap().name, "fake:a");
    assert_eq!(status(&d, &e, "asio:1"), PositionStatus::Filled { online: true });
    assert_eq!(e.points_in(Some((0, 4096)), None).len(), 1, "its route is kept");
}

#[test]
fn virtual_devices_are_named_by_their_position() {
    setup();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    // (VAIO needs its driver to switch on; its naming is unit-tested in devices.rs.)
    for (pos, want) in [("vasio:B", "VASIO B"), ("vasio:H", "VASIO H")] {
        let on = Command::SetVirtual { pos: p(pos), on: true, shape: Some((2, 2)) };
        assert!(matches!(d.handle(&mut e, &on), Some(Response::Ok)), "{pos}");
        let names: Vec<String> = e.slots().iter().map(|s| s.name.clone()).collect();
        assert!(names.iter().any(|n| n == want), "{pos}: {names:?}");
    }
    assert!(e.slots().iter().all(|s| !s.name.starts_with("VASIO 2") && !s.name.starts_with("VAIO 1")));
}
