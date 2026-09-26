//! Runs the real engine binary: control over the pipe, journal across restarts.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command as Process};
use std::time::Duration;

use confluence_api::{Command, PointState, Response};
use confluence_engine::ipc::PipeClient;

fn spawn(pipe: &str, journal: &std::path::Path) -> Child {
    Process::new(env!("CARGO_BIN_EXE_confluence-engine"))
        .args(["--pipe", pipe, "--journal"])
        .arg(journal)
        .spawn()
        .unwrap()
}

fn shutdown(mut child: Child, client: &mut PipeClient) {
    assert_eq!(client.call(Command::Shutdown).unwrap(), Response::Ok);
    let status = child.wait().unwrap();
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
    child.kill().unwrap(); // no clean shutdown
    child.wait().unwrap();

    let child = spawn(&pipe, &journal);
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    assert_eq!(points.len(), 5, "every acknowledged change survived");
    shutdown(child, &mut c);
}

#[test]
fn second_instance_on_the_same_pipe_exits_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = format!("confluence-dup-test-{}", std::process::id());
    let first = spawn(&pipe, &dir.path().join("a.bin"));
    let mut c = PipeClient::connect(&pipe, Duration::from_secs(10)).unwrap();

    let status = spawn(&pipe, &dir.path().join("b.bin")).wait().unwrap();
    assert!(!status.success(), "second instance must refuse to start");
    assert!(matches!(c.call(Command::Health).unwrap(), Response::Health { .. }), "first instance unaffected");
    shutdown(first, &mut c);
}
