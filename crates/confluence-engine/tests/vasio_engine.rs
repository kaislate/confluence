//! VASIO through the whole engine, in real time: a (fake) hardware input is
//! routed to a DAW through VASIO, the DAW adds 0.5, and its output is routed
//! back to the (fake) hardware output.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use confluence_api::{ClockRole, Command, DeviceKind, Response};
use confluence_engine::clock::InternalClock;
use confluence_engine::devices::{AsioOpener, DeviceManager};
use confluence_engine::{Engine, EngineConfig};
use confluence_provider_asio::fake::{FakeConfig, FakeProbe};
use confluence_provider_asio::{AsioCallback, AsioDevice, AsioIo, DriverSource, StreamConfig};

fn fake_opener(probe: Arc<FakeProbe>) -> AsioOpener {
    Box::new(move |name: &str| {
        let mut cfg = FakeConfig::new(name);
        cfg.probe = probe.clone();
        AsioDevice::open(DriverSource::Fake(cfg))
    })
}

/// A DAW on VASIO `instance` that plays input 1 + 0.5 on output 1.
fn daw(instance: u32) -> AsioDevice {
    let mut dev = AsioDevice::open(DriverSource::ClassFactory {
        get_class_object: confluence_vasio::DllGetClassObject,
        clsid: confluence_vasio::clsid(instance),
        name: confluence_vasio::driver_name(instance),
    })
    .unwrap();
    let mut buf = Vec::new();
    let cb: Box<dyn AsioCallback> = Box::new(move |io: &mut AsioIo<'_>| {
        buf.resize(io.frames(), 0.0);
        io.read_input(0, &mut buf);
        buf.iter_mut().for_each(|s| *s += 0.5);
        io.write_output(0, &buf);
    });
    dev.start(StreamConfig::default(), cb).unwrap();
    dev
}

fn route(engine: &mut Engine, input: u32, output: u32) {
    let cmd = Command::SetPoint { input, output, gain_db: 0.0, mute: false, invert: false };
    assert_eq!(engine.handle(&cmd), Response::Ok);
}

#[test]
fn hardware_to_a_daw_and_back_through_vasio() {
    confluence_provider_vasio::isolate_for_tests();
    let root = format!("Software\\ConfluenceTest\\VASIO.engine.{}", std::process::id());
    // Removes the scratch key even if an assertion below fails.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            confluence_provider_vasio::config::delete_root(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let probe = Arc::new(FakeProbe::default());
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None)
        .with_asio_opener(fake_opener(probe.clone()))
        .with_vasio_config_root(Some(root.clone()));
    devices.add(&mut engine, DeviceKind::Asio, "fake:io").unwrap();
    let ids = devices.add(&mut engine, DeviceKind::Vasio, "1:2x2").unwrap();
    assert_eq!(ids.len(), 1, "one strict slot carries both directions");
    for again in ["1", "1:8", " 1:2x2"] {
        let err = devices.add(&mut engine, DeviceKind::Vasio, again).unwrap_err();
        assert!(err.contains("already open"), "{again}: {err}");
    }
    let slots = engine.slots();
    let hw_in = slots.iter().find(|s| s.name == "fake:io in").unwrap().first_input;
    let hw_out = slots.iter().find(|s| s.name == "fake:io out").unwrap().first_output;
    let vasio = slots.iter().find(|s| s.id == ids[0]).unwrap();
    assert_eq!(vasio.role, ClockRole::Strict);
    route(&mut engine, hw_in, vasio.first_output); // hardware in 1 -> DAW in 1
    route(&mut engine, vasio.first_input, hw_out); // DAW out 1 -> hardware out 1
    assert_eq!(
        confluence_provider_vasio::config::load_at(&root, 1).map(|c| (c.daw_inputs, c.block)),
        Some((2, 256)),
        "the shape is remembered for DAWs opened while the engine is down"
    );

    let clock = InternalClock::start(audio, 48_000.0).unwrap();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    let attached = |engine: &mut Engine| {
        let Response::Health { slots, .. } = engine.handle(&Command::Health) else { panic!() };
        slots.iter().find(|h| h.id == ids[0]).unwrap().attached
    };
    assert_eq!(attached(&mut engine), Some(false), "health says when no DAW is attached");
    let mut daw = daw(1);
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    // The fake hardware input carries 0.25; the DAW adds 0.5.
    let last = probe.last_output.lock().unwrap().clone();
    assert!(last.iter().all(|&s| (s - 0.75).abs() < 1e-3), "hardware heard the DAW: {:?}", &last[..4]);
    let Response::Health { slots, .. } = engine.handle(&Command::Health) else { panic!() };
    let h = slots.iter().find(|h| h.id == ids[0]).unwrap();
    assert_eq!((h.underruns, h.overruns), (0, 0), "{h:?}");
    assert_eq!(h.attached, Some(true));

    // Removing the slot releases the instance; the DAW keeps running on silence.
    assert_eq!(devices.handle(&mut engine, &Command::RemoveSlot { id: ids[0] }), Some(Response::Ok));
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    assert!(daw.is_running());
    daw.stop();
    clock.stop();
}
