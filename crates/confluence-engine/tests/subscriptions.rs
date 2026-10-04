//! The real engine binary, seen through subscriptions.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command as Process};
use std::time::{Duration, Instant};

use confluence_api::{Change, Command, DeviceKind, Event, Response};
use confluence_client::{Client, Subscription};

/// A running engine process, killed if the test ends without shutting it down.
struct Engine(Option<Child>);

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The engine with this test's own pipe, journal and devices file.
fn spawn(pipe: &str, dir: &std::path::Path) -> Engine {
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_confluence-engine"));
    cmd.args(["--pipe", pipe, "--journal"]).arg(dir.join("journal.bin")).arg("--devices").arg(dir.join("devices.json"));
    Engine(Some(cmd.spawn().unwrap()))
}

/// The next versioned event, skipping telemetry.
fn next_change(sub: &mut Subscription) -> (u64, Vec<Change>) {
    loop {
        match sub.recv().unwrap() {
            Event::Changed { version, changes } => return (version, changes),
            Event::Telemetry { .. } => {}
        }
    }
}

fn set(input: u32, output: u32, gain_db: f32) -> Command {
    Command::SetPoint { input, output, gain_db, mute: false, invert: false }
}

#[test]
fn another_clients_command_reaches_the_subscriber_with_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-a-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (snap, mut sub) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    let Response::Applied { version } = c.call(set(1, 2, -6.0)).unwrap() else { panic!() };
    assert_eq!(version, snap.version + 1);
    let (v, changes) = next_change(&mut sub);
    assert_eq!(v, version);
    assert!(matches!(&changes[..], [Change::PointSet(p)] if (p.input, p.output) == (1, 2)), "{changes:?}");
    // Setting it again to the same values changes nothing: same version, no event.
    let Response::Applied { version: again } = c.call(set(1, 2, -6.0)).unwrap() else { panic!() };
    assert_eq!(again, version);
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn adding_and_removing_a_device_are_slot_events() {
    confluence_provider_vasio::isolate_for_tests(); // inherited by the engine
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-b-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (_, mut sub) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    let added = c.call(Command::AddDevice { kind: DeviceKind::Vasio, name: "7".into() }).unwrap();
    let Response::Added { ids, version } = added else { panic!("{added:?}") };
    let (v, changes) = next_change(&mut sub);
    assert_eq!(v, version);
    assert!(changes.iter().any(|ch| matches!(ch, Change::SlotAdded(s) if s.id == ids[0])), "{changes:?}");
    assert!(matches!(c.call(Command::RemoveSlot { id: ids[0] }).unwrap(), Response::Applied { .. }));
    let (_, changes) = next_change(&mut sub);
    assert!(changes.contains(&Change::SlotRemoved { id: ids[0] }), "{changes:?}");
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn telemetry_arrives_about_ten_times_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-c-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (snap, mut sub) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(snap.status.master, "internal");
    // Count from the first telemetry event, so engine start-up does not count.
    while !matches!(sub.recv().unwrap(), Event::Telemetry { .. }) {}
    let start = Instant::now();
    let mut n = 0;
    let mut last = None;
    while start.elapsed() < Duration::from_secs(2) {
        if let Event::Telemetry { status, .. } = sub.recv().unwrap() {
            n += 1;
            last = Some(status);
        }
    }
    assert!((15..=25).contains(&n), "{n} telemetry events in 2 s");
    let s = last.unwrap();
    assert!(s.blocks > 0 && s.dsp_load > 0.0 && s.dsp_load < 1.0, "{s:?}");
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    assert!(matches!(c.call(Command::Status).unwrap(), Response::Status(st) if st.block == 256));
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_stalled_subscriber_is_dropped_while_others_carry_on() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-d-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (_, stalled) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap(); // never read
    let (_, mut live) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    // The stalled queue (256 events) plus the pipe's buffer fill up well within 1000 changes.
    for i in 0..1000u32 {
        assert!(
            matches!(c.call(set(i % 100, i / 100, 0.0)).unwrap(), Response::Applied { .. }),
            "commands keep working"
        );
        let _ = next_change(&mut live); // every SetPoint is a change: keep the live one drained
    }
    drop(stalled);
    for _ in 0..5 {
        live.recv().unwrap();
    }
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_subscriber_joining_mid_traffic_ends_in_sync() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-e-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let writer_pipe = pipe.clone();
    let writer = std::thread::spawn(move || {
        let mut c = Client::connect(&writer_pipe, Duration::from_secs(10)).unwrap();
        for i in 0..300u32 {
            c.call(set(i % 16, i % 7, -((i % 30) as f32))).unwrap();
        }
    });
    std::thread::sleep(Duration::from_millis(30));
    let (mut state, mut sub) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    writer.join().unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    let Response::Points(mut expected) = c.call(Command::ListPoints).unwrap() else { panic!() };
    expected.sort_by_key(|p| (p.input, p.output));
    // Apply everything until the subscriber's copy matches (or time runs out).
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.points != expected {
        assert!(Instant::now() < deadline, "never caught up: {} of {} points", state.points.len(), expected.len());
        if let Event::Changed { version, changes } = sub.recv().unwrap() {
            assert_eq!(version, state.version + 1, "contiguous versions");
            state.apply(&changes);
            state.version = version;
        }
    }
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn shutting_down_with_a_subscriber_ends_its_stream_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-f-{}", std::process::id());
    let mut engine = spawn(&pipe, dir.path());
    let (_, mut sub) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    assert_eq!(c.call(Command::Shutdown).unwrap(), Response::Ok);
    drop(c);
    let start = Instant::now();
    let status = engine.0.take().unwrap().wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(start.elapsed() < Duration::from_secs(3), "exited promptly: {:?}", start.elapsed());
    while sub.recv().is_ok() {}
}
