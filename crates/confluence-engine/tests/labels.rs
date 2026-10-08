//! Custom names for devices and channels: kept with the device's position
//! (like colours), shared by a device's slots, trimmed and bounded.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use confluence_api::{ChannelRef, Command, DeviceKind, Response, SlotState};
use confluence_engine::devices::{AsioOpener, DeviceManager};
use confluence_engine::engine::BusSpec;
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

fn slot(e: &Engine, name: &str) -> SlotState {
    e.slots().into_iter().find(|s| s.name == name).unwrap()
}

fn label(id: u32, channel: Option<ChannelRef>, name: Option<&str>) -> Command {
    Command::SetSlotLabel { id, channel, name: name.map(str::to_string) }
}

#[test]
fn a_device_name_is_shared_by_its_slots_and_a_channel_name_lands_on_its_channel() {
    confluence_provider_vasio::isolate_for_tests();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake(&["fake:a"]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let (input, output) = (slot(&e, "fake:a in"), slot(&e, "fake:a out"));
    assert_eq!(e.handle(&label(input.id, None, Some("  Desk  "))), Response::Ok);
    assert_eq!(slot(&e, "fake:a in").label.as_deref(), Some("Desk"), "trimmed");
    assert_eq!(slot(&e, "fake:a out").label.as_deref(), Some("Desk"), "the device's other slot too");
    let ch = Some(ChannelRef { input: false, index: 1 });
    assert_eq!(e.handle(&label(output.id, ch, Some("Monitor R"))), Response::Ok);
    let out = slot(&e, "fake:a out");
    assert_eq!(out.output_labels.len(), out.outputs as usize);
    assert_eq!(out.output_labels[1].as_deref(), Some("Monitor R"));
    assert_eq!(out.output_labels[0], None);
    // Cleared by None or a blank name.
    assert_eq!(e.handle(&label(output.id, ch, Some("   "))), Response::Ok);
    assert_eq!(slot(&e, "fake:a out").output_labels[1], None);
    assert_eq!(e.handle(&label(input.id, None, None)), Response::Ok);
    assert_eq!(slot(&e, "fake:a in").label, None);
}

#[test]
fn bad_slots_and_channels_are_refused_and_long_names_are_capped() {
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    e.add_bus(&BusSpec { name: "Verb".into(), channels: 2, first_input: None, first_output: None }).unwrap();
    let bus = e.slots().into_iter().find(|s| s.is_bus()).unwrap();
    assert!(matches!(e.handle(&label(99, None, Some("x"))), Response::Error(_)));
    let past = Some(ChannelRef { input: true, index: 2 });
    assert!(matches!(e.handle(&label(bus.id, past, Some("x"))), Response::Error(_)), "a bus has 2 returns");
    assert_eq!(e.handle(&label(bus.id, None, Some(&"v".repeat(100)))), Response::Ok);
    let named = e.slots().into_iter().find(|s| s.is_bus()).unwrap();
    assert_eq!(named.label.unwrap().chars().count(), confluence_api::MAX_LABEL);
}

#[test]
fn names_stay_with_the_position_when_its_device_is_swapped() {
    confluence_provider_vasio::isolate_for_tests();
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut d = DeviceManager::new(None).with_asio_opener(fake(&["fake:a", "fake:b"]));
    d.add(&mut e, DeviceKind::Asio, "fake:a").unwrap();
    let input = slot(&e, "fake:a in");
    e.handle(&label(input.id, None, Some("Desk")));
    e.handle(&label(input.id, Some(ChannelRef { input: true, index: 0 }), Some("Voice")));
    let swap = Command::FillPosition { pos: "asio:1".parse().unwrap(), kind: DeviceKind::Asio, name: "fake:b".into() };
    assert_eq!(d.handle(&mut e, &swap), Some(Response::Ok));
    let b = slot(&e, "fake:b in");
    assert_eq!(b.label.as_deref(), Some("Desk"));
    assert_eq!(b.input_labels[0].as_deref(), Some("Voice"));
}

#[test]
fn names_are_part_of_the_state_the_journal_keeps() {
    let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    e.add_bus(&BusSpec { name: "Verb".into(), channels: 2, first_input: None, first_output: None }).unwrap();
    let bus = e.slots().into_iter().find(|s| s.is_bus()).unwrap();
    e.handle(&label(bus.id, Some(ChannelRef { input: false, index: 0 }), Some("Verb L")));
    let key = e.label_key(bus.id, Some(ChannelRef { input: false, index: 0 })).unwrap();
    assert!(key.ends_with("/out/1"), "{key}");
    assert_eq!(e.label_commands(), vec![Command::SetLabel { key, name: Some("Verb L".into()) }]);
    assert!(Command::SetSlotLabel { id: 1, channel: None, name: None }.is_mutation());
    assert!(Command::SetLabel { key: "k".into(), name: None }.is_mutation());
}
