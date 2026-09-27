//! DeviceManager with fake ASIO drivers, in real time: audio flows through
//! driver buffers, bridges and the matrix; bindings persist; missing devices
//! come back as offline slots on their saved channels.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use confluence_api::{Command, DeviceKind, Response};
use confluence_engine::clock::InternalClock;
use confluence_engine::devices::{start_asio_master, AsioOpener, DeviceManager};
use confluence_engine::{Engine, EngineConfig, MasterChannels};
use confluence_provider_asio::fake::{FakeConfig, FakeProbe};
use confluence_provider_asio::{AsioDevice, AsioHostError, DriverSource};

/// Opens `fake:<name>` as a fake driver (2 in / 2 out, input 0.25) whose probe
/// is shared with the test; anything else is "not installed".
fn opener(probes: Vec<(&'static str, Arc<FakeProbe>)>) -> AsioOpener {
    Box::new(move |name: &str| {
        let (_, probe) = probes.iter().find(|(n, _)| *n == name).ok_or(AsioHostError::NotInstalled(name.into()))?;
        let mut cfg = FakeConfig::new(name);
        cfg.probe = probe.clone();
        AsioDevice::open(DriverSource::Fake(cfg))
    })
}

fn route(engine: &mut Engine, input: u32, output: u32) {
    let cmd = Command::SetPoint { input, output, gain_db: 0.0, mute: false, invert: false };
    assert_eq!(engine.handle(&cmd), Response::Ok);
}

#[test]
fn an_asio_device_loops_audio_through_the_engine() {
    let probe = Arc::new(FakeProbe::default());
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None).with_asio_opener(opener(vec![("fake:loop", probe.clone())]));
    let ids = devices.add(&mut engine, DeviceKind::Asio, "fake:loop").unwrap();
    assert_eq!(ids.len(), 2, "one input slot and one output slot");
    let slots = engine.slots();
    let (inp, out) = (slots[0].first_input, slots[1].first_output);
    route(&mut engine, inp, out); // device input 1 -> device output 1
    let clock = InternalClock::start(audio, 48_000.0).unwrap();
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    let last = probe.last_output.lock().unwrap().clone();
    assert!(last.iter().all(|&s| (s - 0.25).abs() < 1e-3), "device heard its own input: {:?}", &last[..4]);
    let Response::Health { slots: health, .. } = engine.handle(&Command::Health) else { panic!() };
    assert_eq!(health.len(), 2);
    clock.stop();
}

#[test]
fn an_asio_master_drives_the_engine_and_a_second_device_hears_it() {
    let master_probe = Arc::new(FakeProbe::default());
    let soft_probe = Arc::new(FakeProbe::default());
    let open = opener(vec![("fake:master", master_probe.clone()), ("fake:soft", soft_probe.clone())]);
    let mut master = open("fake:master").unwrap();
    let block = master.info().preferred_block as usize;
    let (mut engine, audio) = Engine::new(EngineConfig::new(master.info().sample_rate, block));
    let (_, stream, _) = start_asio_master(&mut master, &mut engine, audio, "fake:master", None).unwrap();
    assert_eq!(stream.block, block);
    let mut devices = DeviceManager::new(None).with_asio_opener(open);
    devices.add(&mut engine, DeviceKind::Asio, "fake:soft").unwrap();
    let slots = engine.slots();
    let master_in = slots.iter().find(|s| s.name.contains("master")).unwrap().first_input;
    let soft_out = slots.iter().find(|s| s.name == "fake:soft out").unwrap().first_output;
    route(&mut engine, master_in, soft_out); // master input 1 -> soft device output 1
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    assert!(engine.blocks() > 500, "the master's callbacks drive the engine: {}", engine.blocks());
    // The fake master's inputs carry 0.25: it must arrive at the other device.
    let last = soft_probe.last_output.lock().unwrap().clone();
    assert!(last.iter().all(|&s| (s - 0.25).abs() < 1e-3), "soft device heard the master: {:?}", &last[..4]);
    master.stop();
}

#[test]
fn bindings_persist_and_missing_devices_keep_their_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let (a, b) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    {
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (devices, warnings) = DeviceManager::open_file(path.clone());
        assert!(warnings.is_empty(), "a missing file is simply a fresh start");
        let mut devices = devices.with_asio_opener(opener(vec![("fake:a", a.clone()), ("fake:b", b.clone())]));
        devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap();
        devices.add(&mut engine, DeviceKind::Asio, "fake:b").unwrap();
        assert_eq!(devices.bindings()[1].first_input, 2);
    }
    // Restart with "fake:a" missing: its channels stay reserved, "fake:b" keeps 2..4.
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (devices, _) = DeviceManager::open_file(path.clone());
    let mut devices = devices.with_asio_opener(opener(vec![("fake:b", b.clone())]));
    let warnings = devices.restore(&mut engine);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("asio:fake:a is offline"));
    let slots = engine.slots();
    let offline = slots.iter().find(|s| !s.online).unwrap();
    assert_eq!((offline.first_input, offline.inputs, offline.first_output, offline.outputs), (0, 2, 0, 2));
    let b_in = slots.iter().find(|s| s.name == "fake:b in").unwrap();
    assert_eq!(b_in.first_input, 2, "channels did not shift");
    // Removing the offline slot through the Control API frees its channels.
    assert_eq!(devices.handle(&mut engine, &Command::RemoveSlot { id: offline.id }), Some(Response::Ok));
    assert_eq!(devices.bindings().len(), 1);
}

#[test]
fn a_missing_device_is_a_clear_error_and_leaves_nothing_behind() {
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None).with_asio_opener(opener(vec![]));
    let resp = devices.handle(&mut engine, &Command::AddDevice { kind: DeviceKind::Asio, name: "fake:nope".into() });
    assert!(matches!(resp, Some(Response::Error(ref e)) if e.contains("not installed")), "{resp:?}");
    assert!(engine.slots().is_empty());
    assert_eq!(devices.handle(&mut engine, &Command::ListPoints), None, "not a device command");
}

#[test]
fn removing_a_device_stops_its_driver() {
    let probe = Arc::new(FakeProbe::default());
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None).with_asio_opener(opener(vec![("fake:x", probe.clone())]));
    let ids = devices.add(&mut engine, DeviceKind::Asio, "fake:x").unwrap();
    assert_eq!(devices.handle(&mut engine, &Command::RemoveSlot { id: ids[1] }), Some(Response::Ok));
    assert!(engine.slots().is_empty(), "both of the device's slots are removed");
    assert!(probe.released.load(Ordering::Acquire), "driver released");
}

#[test]
fn the_master_gets_its_saved_channels_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let (m, soft) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    let open = || opener(vec![("fake:m", m.clone()), ("fake:soft", soft.clone())]);
    let first_run_master_channels = {
        // First run: a soft device already exists, so the new master is placed after it.
        let (devices, _) = DeviceManager::open_file(path.clone());
        let mut devices = devices.with_asio_opener(open());
        let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 128));
        devices.add(&mut engine, DeviceKind::Asio, "fake:soft").unwrap();
        let mut master = open()("fake:m").unwrap();
        let (_, _, ch) = start_asio_master(&mut master, &mut engine, audio, "fake:m", None).unwrap();
        devices.set_master("fake:m", ch).unwrap();
        master.stop();
        ch
    };
    assert_eq!((first_run_master_channels.first_input, first_run_master_channels.first_output), (2, 2));
    // Second run: the master starts first, with its saved placement, then the rest is restored.
    let (devices, _) = DeviceManager::open_file(path.clone());
    let mut devices = devices.with_asio_opener(open());
    let placement = devices.saved_master("fake:m");
    assert_eq!(placement, Some((2, 2)));
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 128));
    let mut master = open()("fake:m").unwrap();
    let (_, _, ch) = start_asio_master(&mut master, &mut engine, audio, "fake:m", placement).unwrap();
    assert_eq!((ch.first_input, ch.first_output), (2, 2), "routes to the master keep working");
    assert!(devices.restore(&mut engine).is_empty());
    let soft_in = engine.slots().into_iter().find(|s| s.name == "fake:soft in").unwrap();
    assert_eq!(soft_in.first_input, 0, "the soft device is back on its own channels");
    master.stop();
}

#[test]
fn a_corrupt_bindings_file_is_kept_and_reported_not_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    std::fs::write(&path, "{ this is not json").unwrap();
    let (devices, warnings) = DeviceManager::open_file(path.clone());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("not valid"));
    assert!(dir.path().join("devices.bad").exists(), "the damaged file is preserved for recovery");
    assert!(!path.exists());
    assert!(devices.bindings().is_empty());
}

#[test]
fn a_device_cannot_be_opened_twice_and_the_master_is_not_a_soft_slot() {
    let (a, m) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None).with_asio_opener(opener(vec![("fake:a", a), ("fake:m", m)]));
    devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap();
    let err = devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap_err();
    assert!(err.contains("already open"), "{err}");
    assert_eq!(engine.slots().len(), 2, "no second instance of the driver");
    devices.set_master("fake:m", MasterChannels { first_input: 2, inputs: 2, first_output: 2, outputs: 2 }).unwrap();
    let err = devices.add(&mut engine, DeviceKind::Asio, "fake:m").unwrap_err();
    assert!(err.contains("master"), "{err}");
    assert_eq!(devices.bindings().len(), 1);
}

#[test]
fn adding_an_offline_device_brings_it_back_on_its_saved_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let (a, b) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    let plugged = Arc::new(AtomicBool::new(true));
    let open = || -> AsioOpener {
        let (a, b, plugged) = (a.clone(), b.clone(), plugged.clone());
        Box::new(move |name: &str| {
            if name == "fake:a" && !plugged.load(Ordering::Acquire) {
                return Err(AsioHostError::NotInstalled(name.into()));
            }
            opener(vec![("fake:a", a.clone()), ("fake:b", b.clone())])(name)
        })
    };
    {
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let mut devices = DeviceManager::open_file(path.clone()).0.with_asio_opener(open());
        devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap();
        devices.add(&mut engine, DeviceKind::Asio, "fake:b").unwrap();
    }
    plugged.store(false, Ordering::Release);
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::open_file(path.clone()).0.with_asio_opener(open());
    assert_eq!(devices.restore(&mut engine).len(), 1, "fake:a starts offline");
    // Routes to and from the offline device (replayed from the journal at start-up).
    route(&mut engine, 0, 2); // fake:a input 1 -> fake:b output 1
    route(&mut engine, 2, 1); // fake:b input 1 -> fake:a output 2
    let points = |engine: &mut Engine| match engine.handle(&Command::ListPoints) {
        Response::Points(p) => {
            let mut v: Vec<_> = p.into_iter().map(|p| (p.input, p.output)).collect();
            v.sort();
            v
        }
        other => panic!("{other:?}"),
    };
    // Still unplugged: a clear error, and the offline slot keeps its channels and routes.
    let err = devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap_err();
    assert!(err.contains("not installed"), "{err}");
    let offline = engine.slots().into_iter().find(|s| !s.online).unwrap();
    assert_eq!((offline.first_input, offline.first_output), (0, 0));
    assert_eq!(points(&mut engine), vec![(0, 2), (2, 1)], "a failed re-add keeps the routes");
    // Plugged back in: it comes back online on the same channels.
    plugged.store(true, Ordering::Release);
    let ids = devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap();
    assert_eq!(ids.len(), 2);
    let slots = engine.slots();
    assert!(slots.iter().all(|s| s.online), "{slots:?}");
    let a_in = slots.iter().find(|s| s.name == "fake:a in").unwrap();
    let a_out = slots.iter().find(|s| s.name == "fake:a out").unwrap();
    assert_eq!((a_in.first_input, a_out.first_output), (0, 0), "routes to it keep working");
    assert_eq!(points(&mut engine), vec![(0, 2), (2, 1)], "coming back online keeps the routes");
    assert_eq!(devices.bindings().len(), 2);
}

#[test]
fn a_soft_device_that_becomes_the_master_is_not_opened_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let m = Arc::new(FakeProbe::default());
    {
        // First run on the internal clock: "fake:m" is added as an ordinary device.
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let mut devices =
            DeviceManager::open_file(path.clone()).0.with_asio_opener(opener(vec![("fake:m", m.clone())]));
        devices.add(&mut engine, DeviceKind::Asio, "fake:m").unwrap();
    }
    // Next run uses it as the master: the saved soft binding must not open it a second time.
    let opens = Arc::new(AtomicUsize::new(0));
    let counting: AsioOpener = {
        let (opens, m) = (opens.clone(), m.clone());
        Box::new(move |name: &str| {
            opens.fetch_add(1, Ordering::SeqCst);
            opener(vec![("fake:m", m.clone())])(name)
        })
    };
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::open_file(path.clone()).0.with_asio_opener(counting);
    devices.claim_master("fake:m");
    let warnings = devices.restore(&mut engine);
    assert_eq!(opens.load(Ordering::SeqCst), 0, "the master's driver is not loaded again");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("master"), "{warnings:?}");
    assert!(engine.slots().is_empty());
    assert!(devices.bindings().is_empty(), "the stale soft binding is dropped");
}

#[test]
fn driver_requests_show_in_health() {
    let probe = Arc::new(FakeProbe::default());
    let open: AsioOpener = Box::new(move |name: &str| {
        let mut cfg = FakeConfig::new(name);
        cfg.probe = probe.clone();
        cfg.reset_after = Some(20); // e.g. the user changed the buffer size in the driver panel
        AsioDevice::open(DriverSource::Fake(cfg))
    });
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut devices = DeviceManager::new(None).with_asio_opener(open);
    let ids = devices.add(&mut engine, DeviceKind::Asio, "fake:r").unwrap();
    let clock = InternalClock::start(audio, 48_000.0).unwrap();
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
    }
    let mut resp = engine.handle(&Command::Health);
    devices.annotate(&mut resp);
    let Response::Health { slots, .. } = resp else { panic!() };
    for id in ids {
        let h = slots.iter().find(|h| h.id == id).unwrap();
        assert!(h.driver_requests >= 1, "{h:?}");
        assert!(!h.device_lost);
    }
    clock.stop();
}

/// Adds a device so the manager saves its bindings.
fn trigger_save(devices: &mut DeviceManager) {
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let probe = Arc::new(FakeProbe::default());
    let mut d = std::mem::replace(devices, DeviceManager::new(None));
    d = d.with_asio_opener(opener(vec![("fake:new", probe)]));
    d.add(&mut engine, DeviceKind::Asio, "fake:new").unwrap();
    *devices = d;
}

#[test]
fn an_unreadable_bindings_file_is_never_overwritten() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    std::fs::write(&path, r#"{"master":null,"devices":[]}"#).unwrap();
    let original = std::fs::read(&path).unwrap();
    // Another program holds the file open without sharing: reading fails, but the file is fine.
    let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
    let (mut devices, warnings) = DeviceManager::open_file(path.clone());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("will not be saved"), "{warnings:?}");
    drop(lock);
    trigger_save(&mut devices);
    assert_eq!(std::fs::read(&path).unwrap(), original, "a file we could not read is left alone");
}

#[test]
fn a_corrupt_file_that_cannot_be_moved_aside_is_left_and_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    std::fs::write(&path, "{ this is not json").unwrap();
    std::fs::create_dir(dir.path().join("devices.bad")).unwrap(); // the rename target is taken
    let (mut devices, warnings) = DeviceManager::open_file(path.clone());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(!warnings[0].contains("kept as"), "must not claim a move that failed: {warnings:?}");
    assert!(warnings[0].contains("left in place"), "{warnings:?}");
    trigger_save(&mut devices);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ this is not json", "the damaged file survives");
}

#[test]
fn a_binding_whose_channels_are_taken_stays_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let (a, m) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    {
        // First run: "fake:a" on inputs 0..2.
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let mut devices =
            DeviceManager::open_file(path.clone()).0.with_asio_opener(opener(vec![("fake:a", a.clone())]));
        devices.add(&mut engine, DeviceKind::Asio, "fake:a").unwrap();
    }
    // Next run: a new master takes channels 0.. first, so "fake:a" (now missing) cannot be placed.
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut master = opener(vec![("fake:m", m.clone())])("fake:m").unwrap();
    start_asio_master(&mut master, &mut engine, audio, "fake:m", None).unwrap();
    let (devices, _) = DeviceManager::open_file(path.clone());
    let mut devices = devices.with_asio_opener(opener(vec![]));
    let warnings = devices.restore(&mut engine);
    assert!(warnings.iter().any(|w| w.contains("could not be reserved")), "{warnings:?}");
    devices
        .set_master(
            "fake:m",
            confluence_engine::MasterChannels { first_input: 0, inputs: 2, first_output: 0, outputs: 2 },
        )
        .unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("fake:a"), "the unplaceable binding is kept for a later run: {saved}");
    master.stop();
}
