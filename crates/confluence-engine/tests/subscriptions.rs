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
    cmd.env(confluence_provider_vasio::NAMESPACE_VAR, format!("test-{}", std::process::id())).env(
        confluence_provider_vasio::config::ROOT_VAR,
        format!(r"Software\ConfluenceTest\VASIO.{}", std::process::id()),
    );
    cmd.args(["--pipe", pipe, "--journal"]).arg(dir.join("journal.bin")).arg("--devices").arg(dir.join("devices.json"));
    // Never this PC's MIDI devices or plugins.
    let no_plugins = dir.join("no-plugins");
    let _ = std::fs::create_dir_all(&no_plugins);
    cmd.arg("--no-midi").arg("--clap-path").arg(no_plugins);
    cmd.args(["--no-net-discovery", "--net-bind", "127.0.0.1", "--net-port", "0"]);
    Engine(Some(cmd.spawn().unwrap()))
}

/// The next versioned event, skipping telemetry.
fn next_change(sub: &mut Subscription) -> (u64, Vec<Change>) {
    loop {
        match sub.recv().unwrap() {
            Event::Changed { version, changes } => return (version, changes),
            Event::Telemetry { .. } | Event::Meters(_) => {}
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

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_live(store: &confluence_client::StateStore) -> bool {
    matches!(store.view().conn, confluence_client::ConnState::Live)
}

#[test]
fn a_store_follows_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-store-a-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let store = confluence_client::StateStore::spawn(pipe.clone(), Box::new(|_| {}));
    wait_for("live", Duration::from_secs(10), || is_live(&store));
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    c.call(Command::SetPoint { input: 3, output: 4, gain_db: -3.0, mute: true, invert: false }).unwrap();
    wait_for("the point", Duration::from_secs(2), || {
        store
            .view()
            .state
            .as_ref()
            .is_some_and(|s| s.points.iter().any(|p| (p.input, p.output, p.mute) == (3, 4, true)))
    });
    wait_for("telemetry history", Duration::from_secs(2), || {
        store.view().status.as_ref().is_some_and(|s| s.blocks > 0)
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_store_resyncs_after_the_engine_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-store-b-{}", std::process::id());
    let mut engine = spawn(&pipe, dir.path());
    let store = confluence_client::StateStore::spawn(pipe.clone(), Box::new(|_| {}));
    wait_for("live", Duration::from_secs(10), || is_live(&store));
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    c.call(Command::SetPoint { input: 1, output: 1, gain_db: 0.0, mute: false, invert: false }).unwrap();
    wait_for("the point", Duration::from_secs(2), || store.view().state.as_ref().is_some_and(|s| s.points.len() == 1));
    // Kill it, and restart it with a fresh journal: the point must disappear.
    drop(c);
    drop(std::mem::replace(&mut engine, Engine(None)));
    wait_for("reconnecting", Duration::from_secs(5), || {
        matches!(store.view().conn, confluence_client::ConnState::Reconnecting { .. })
    });
    std::fs::remove_file(dir.path().join("journal.bin")).unwrap();
    let _engine2 = spawn(&pipe, dir.path());
    wait_for("a fresh snapshot", Duration::from_secs(15), || {
        let v = store.view();
        matches!(v.conn, confluence_client::ConnState::Live) && v.state.as_ref().is_some_and(|s| s.points.is_empty())
    });
    let mut c2 = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    c2.call(Command::Shutdown).unwrap();
}

/// Connections that stay open (a GUI's command connection, a subscriber that
/// stopped reading) must not keep the engine's state alive at shutdown: it has
/// to exit promptly and stop its devices cleanly.
#[test]
fn open_connections_do_not_hold_up_shutdown() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-sub-g-{}", std::process::id());
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_confluence-engine"));
    cmd.env(confluence_provider_vasio::NAMESPACE_VAR, format!("test-{}", std::process::id())).env(
        confluence_provider_vasio::config::ROOT_VAR,
        format!(r"Software\ConfluenceTest\VASIO.{}", std::process::id()),
    );
    cmd.args(["--pipe", &pipe, "--journal"])
        .arg(dir.path().join("journal.bin"))
        .arg("--devices")
        .arg(dir.path().join("devices.json"))
        .arg("--no-midi")
        .args(["--no-net-discovery", "--net-bind", "127.0.0.1", "--net-port", "0"])
        .arg("--clap-path")
        .arg(dir.path())
        .stderr(std::process::Stdio::piped());
    let mut engine = Engine(Some(cmd.spawn().unwrap()));
    let (_, _never_read) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let mut idle = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    idle.call(Command::ListPoints).unwrap();
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    assert_eq!(c.call(Command::Shutdown).unwrap(), Response::Ok);
    let start = Instant::now();
    let mut child = engine.0.take().unwrap();
    let status = child.wait().unwrap();
    let elapsed = start.elapsed();
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(status.success(), "{status:?}");
    assert!(elapsed < Duration::from_secs(3), "exited promptly: {elapsed:?}\n{err}");
    assert!(!err.contains("stopping anyway"), "devices were stopped cleanly:\n{err}");
}

/// A front end can tell state changes (repaint now) from telemetry (which
/// it may draw less often).
#[test]
fn a_store_says_what_kind_of_update_it_made() {
    use confluence_client::Update;
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-store-c-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let seen: Arc<Mutex<Vec<Update>>> = Arc::default();
    let log = seen.clone();
    let store = confluence_client::StateStore::spawn(pipe.clone(), Box::new(move |u| log.lock().unwrap().push(u)));
    wait_for("live", Duration::from_secs(10), || is_live(&store));
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    c.call(Command::SetPoint { input: 1, output: 2, gain_db: 0.0, mute: false, invert: false }).unwrap();
    wait_for("both kinds", Duration::from_secs(3), || {
        let s = seen.lock().unwrap();
        s.contains(&Update::State) && s.contains(&Update::Telemetry)
    });
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn meter_frames_arrive_about_twenty_times_a_second_only_when_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-meters-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (_, mut plain) = Subscription::connect(&pipe, Duration::from_secs(10)).unwrap();
    let (state, mut metered) = Subscription::connect_with_meters(&pipe, Duration::from_secs(10)).unwrap();
    assert!(state.positions.iter().any(|p| p.pos.to_string() == "vasio:A"), "positions are in the state");
    let start = std::time::Instant::now();
    let mut frames = 0;
    while start.elapsed() < Duration::from_secs(1) {
        if let Event::Meters(_) = metered.recv().unwrap() {
            frames += 1;
        }
    }
    assert!((12..=30).contains(&frames), "{frames} meter frames in a second");
    // The plain subscriber got telemetry, never meters.
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        assert!(!matches!(plain.recv().unwrap(), Event::Meters(_)));
    }
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn a_meter_subscriber_that_never_reads_does_not_stall_others() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-meters-slow-{}", std::process::id());
    let _engine = spawn(&pipe, dir.path());
    let (_s, _never_read) = Subscription::connect_with_meters(&pipe, Duration::from_secs(10)).unwrap();
    std::thread::sleep(Duration::from_secs(3)); // its queue fills
    let mut c = Client::connect(&pipe, Duration::from_secs(5)).unwrap();
    let t = std::time::Instant::now();
    assert!(matches!(c.call(Command::Status).unwrap(), Response::Status(_)));
    assert!(t.elapsed() < Duration::from_millis(500), "control stays responsive");
    c.call(Command::Shutdown).unwrap();
}
