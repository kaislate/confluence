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
    assert!(e.slots().iter().all(|s| !s.name.starts_with("VASIO 2")), "no stray slot");
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
