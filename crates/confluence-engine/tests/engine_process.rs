//! Runs the real engine binary: control over the pipe, journal across restarts.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command as Process};
use std::time::Duration;

use confluence_api::{Command, PointState, Response};
use confluence_engine::ipc::PipeClient;

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

fn spawn(pipe: &str, journal: &std::path::Path) -> Engine {
    let child = Process::new(env!("CARGO_BIN_EXE_confluence-engine"))
        .args(["--pipe", pipe, "--journal"])
        .arg(journal)
        .spawn()
        .unwrap();
    Engine(Some(child))
}

/// Runs a second engine that is expected to refuse to start. If it is still
/// running after 10 s it wrongly started: kill it and return `None`.
fn run_expecting_exit(pipe: &str, journal: &std::path::Path) -> Option<(std::process::ExitStatus, String)> {
    use std::io::Read;
    let mut child = Process::new(env!("CARGO_BIN_EXE_confluence-engine"))
        .args(["--pipe", pipe, "--journal"])
        .arg(journal)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
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

fn shutdown(mut engine: Engine, client: &mut PipeClient) {
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
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
    let set = Command::SetPoint { input: 2, output: 3, gain_db: -12.0, mute: false, invert: true };
    assert_eq!(c.call(set).unwrap(), Response::Ok);
    std::thread::sleep(Duration::from_millis(300));
    let Response::Health { blocks, .. } = c.call(Command::Health).unwrap() else { panic!() };
    assert!(blocks > 20, "internal clock is running: {blocks} blocks");
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
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
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
    for i in 0..5 {
        let set = Command::SetPoint { input: i, output: i, gain_db: -1.0, mute: false, invert: false };
        assert_eq!(c.call(set).unwrap(), Response::Ok);
    }
    child.kill(); // no clean shutdown

    let child = spawn(&pipe, &journal);
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
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
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
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
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
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
    let mut c = PipeClient::connect(&pipe_a, Duration::from_secs(10)).unwrap();

    let outcome = run_expecting_exit(&pipe_b, &journal);
    let (status, stderr) = outcome.expect("a second engine must not run on a journal that is in use");
    assert!(!status.success(), "a journal has one writer");
    assert!(stderr.contains("in use"), "clear message, got: {stderr}");
    shutdown(first, &mut c);
}
