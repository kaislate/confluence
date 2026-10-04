//! Runs the real engine binary: control over the pipe, journal across restarts.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command as Process};
use std::time::Duration;

use confluence_api::{Command, PointState, Response};
use confluence_client::Client;

/// A running engine process, killed if the test ends (or fails) without
/// shutting it down, so no orphaned engine outlives the test run.
struct Engine(Option<Child>);

impl Engine {
    fn kill(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The engine binary with this test's journal and a devices file next to it,
/// so tests never open the devices saved in the user's own `devices.json`.
fn engine_command(pipe: &str, journal: &std::path::Path) -> Process {
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_confluence-engine"));
    cmd.args(["--pipe", pipe, "--journal"]).arg(journal).arg("--devices").arg(journal.with_file_name("devices.json"));
    cmd
}

fn spawn(pipe: &str, journal: &std::path::Path) -> Engine {
    Engine(Some(engine_command(pipe, journal).spawn().unwrap()))
}

/// Runs a second engine that is expected to refuse to start. If it is still
/// running after 10 s it wrongly started: kill it and return `None`.
fn run_expecting_exit(pipe: &str, journal: &std::path::Path) -> Option<(std::process::ExitStatus, String)> {
    use std::io::Read;
    let mut child = engine_command(pipe, journal).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
            return Some((status, err));
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shutdown(mut engine: Engine, client: &mut Client) {
    assert_eq!(client.call(Command::Shutdown).unwrap(), Response::Ok);
    let status = engine.0.take().unwrap().wait().unwrap();
    assert!(status.success(), "{status:?}");
}

#[test]
fn control_journal_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-proc-test-{}", std::process::id());

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let set = Command::SetPoint { input: 2, output: 3, gain_db: -12.0, mute: false, invert: true };
    assert_eq!(c.call(set).unwrap(), Response::Ok);
    std::thread::sleep(Duration::from_millis(300));
    let Response::Health { blocks, .. } = c.call(Command::Health).unwrap() else { panic!() };
    assert!(blocks > 20, "internal clock is running: {blocks} blocks");
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let points = c.call(Command::ListPoints).unwrap();
    assert_eq!(
        points,
        Response::Points(vec![PointState { input: 2, output: 3, gain_db: -12.0, mute: false, invert: true }])
    );
    shutdown(child, &mut c);
}

#[test]
fn killed_engine_keeps_acknowledged_changes() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-kill-test-{}", std::process::id());

    let mut child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    for i in 0..5 {
        let set = Command::SetPoint { input: i, output: i, gain_db: -1.0, mute: false, invert: false };
        assert_eq!(c.call(set).unwrap(), Response::Ok);
    }
    child.kill(); // no clean shutdown

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    assert_eq!(points.len(), 5, "every acknowledged change survived");
    shutdown(child, &mut c);
}

#[test]
fn second_instance_on_the_same_pipe_exits_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-dup-test-{}", std::process::id());
    let mut first = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let set = |i: u32| Command::SetPoint { input: i, output: i, gain_db: 0.0, mute: false, invert: false };
    assert_eq!(c.call(set(1)).unwrap(), Response::Ok);

    // Same pipe and same journal, as with two default-configured engines.
    let (status, stderr) = run_expecting_exit(&pipe, &journal).expect("second instance must not keep running");
    assert!(!status.success(), "second instance must refuse to start");
    assert!(stderr.contains("already running"), "clear message, got: {stderr}");

    // The first engine is unaffected and its journal still records changes.
    assert_eq!(c.call(set(2)).unwrap(), Response::Ok);
    first.kill();
    let restarted = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    assert_eq!(points.len(), 2, "both acknowledged changes survived: {points:?}");
    shutdown(restarted, &mut c);
}

#[test]
fn second_engine_on_another_pipe_cannot_share_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe_a = format!("confluence-share-a-{}", std::process::id());
    let pipe_b = format!("confluence-share-b-{}", std::process::id());
    let first = spawn(&pipe_a, &journal);
    let mut c = Client::connect(&pipe_a, Duration::from_secs(10)).unwrap();

    let outcome = run_expecting_exit(&pipe_b, &journal);
    let (status, stderr) = outcome.expect("a second engine must not run on a journal that is in use");
    assert!(!status.success(), "a journal has one writer");
    assert!(stderr.contains("in use"), "clear message, got: {stderr}");
    shutdown(first, &mut c);
}

#[test]
fn a_removed_slots_routes_stay_removed_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    // A saved device that is not installed: it starts as an offline slot on inputs/outputs 0..2.
    let binding = r#"{"master":null,"devices":[{"kind":"Asio","name":"no such driver (test)",
        "first_input":0,"inputs":2,"first_output":0,"outputs":2}]}"#;
    std::fs::write(dir.path().join("devices.json"), binding).unwrap();
    let pipe = format!("confluence-removed-routes-{}", std::process::id());

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let on_device = Command::SetPoint { input: 1, output: 7, gain_db: 0.0, mute: false, invert: false };
    let elsewhere = Command::SetPoint { input: 10, output: 11, gain_db: 0.0, mute: false, invert: false };
    assert_eq!(c.call(on_device).unwrap(), Response::Ok);
    assert_eq!(c.call(elsewhere).unwrap(), Response::Ok);
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let offline = slots.iter().find(|s| !s.online).expect("the missing device holds its channels");
    assert_eq!(c.call(Command::RemoveSlot { id: offline.id }).unwrap(), Response::Ok);
    shutdown(child, &mut c);

    // Whatever device takes inputs 0..2 next must not inherit the old route.
    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    let routes: Vec<(u32, u32)> = points.iter().map(|p| (p.input, p.output)).collect();
    assert_eq!(routes, vec![(10, 11)]);
    shutdown(child, &mut c);
}
