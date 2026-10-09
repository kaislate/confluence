//! Slots carry their channels' names: the device's own (an ASIO driver's),
//! speaker positions, the DAW's ins and outs, or defaults.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use confluence_api::{Command, DeviceKind, Response};
use confluence_core::asrc::AsrcQuality;
use confluence_engine::devices::{speaker_names, AsioOpener, DeviceManager};
use confluence_engine::engine::{BusSpec, SoftSlotSpec};
use confluence_engine::{Engine, EngineConfig};
use confluence_provider_asio::fake::{FakeConfig, FakeProbe};
use confluence_provider_asio::{AsioDevice, AsioHostError, DriverSource};

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

fn soft(name: &str) -> SoftSlotSpec {
    SoftSlotSpec {
        name: name.into(),
        device: String::new(),
        channels: 2,
        device_rate: 48_000.0,
        device_block: 256,
        quality: AsrcQuality::Sinc64,
        first_channel: None,
        margin_frames: None,
        max_growth_frames: None,
    }
}

#[test]
fn slots_without_device_names_get_numbered_ones() {
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (id, _) = e.add_soft_input(&soft("in")).unwrap();
    let s = e.slots().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(s.input_names, vec!["In 1", "In 2"]);
    assert!(s.output_names.is_empty());
}

#[test]
fn an_insert_bus_names_its_sends_and_returns() {
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    e.add_bus(&BusSpec { name: "Verb".into(), channels: 2, first_input: None, first_output: None }).unwrap();
    let s = e.slots().into_iter().find(|s| s.is_bus()).unwrap();
    assert_eq!(s.input_names, vec!["Return 1", "Return 2"]);
    assert_eq!(s.output_names, vec!["Send 1", "Send 2"]);
}

#[test]
fn names_set_for_a_slot_fill_any_missing_ones_with_defaults() {
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (id, _) = e.add_soft_input(&soft("in")).unwrap();
    e.set_channel_names(id, vec!["Mic".into()], Vec::new());
    let s = e.slots().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(s.input_names, vec!["Mic", "In 2"]);
}

#[test]
fn an_asio_device_carries_its_drivers_channel_names() {
    confluence_provider_vasio::isolate_for_tests();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake(&["fake:a"]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let slots = e.slots();
    let input = slots.iter().find(|s| s.name == "fake:a in").unwrap();
    let output = slots.iter().find(|s| s.name == "fake:a out").unwrap();
    assert_eq!(input.input_names[0], "Fake In 1");
    assert_eq!(output.output_names[1], "Fake Out 2");
}

#[test]
fn a_vasio_names_what_the_daw_sends_and_receives() {
    confluence_provider_vasio::isolate_for_tests();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None);
    let on = Command::SetVirtual { pos: "vasio:C".parse().unwrap(), on: true, shape: Some((2, 2)) };
    assert_eq!(d.handle(&mut e, &on), Some(Response::Ok));
    let s = e.slots().into_iter().find(|s| s.name == "VASIO C").unwrap();
    assert_eq!(s.input_names, vec!["DAW out 1", "DAW out 2"]);
    assert_eq!(s.output_names, vec!["DAW in 1", "DAW in 2"]);
}

#[test]
fn windows_devices_are_named_by_speaker_position() {
    assert_eq!(speaker_names(1), vec!["Mono"]);
    assert_eq!(speaker_names(2), vec!["L", "R"]);
    assert_eq!(speaker_names(6)[3], "LFE");
    assert_eq!(speaker_names(8).len(), 8);
    assert_eq!(speaker_names(3), vec!["Ch 1", "Ch 2", "Ch 3"]);
}
