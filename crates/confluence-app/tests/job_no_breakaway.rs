//! A job that forbids breakaway must not stop the engine from starting: the
//! launcher falls back to starting it inside the job.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/job.rs"]
mod job;

use std::time::Instant;

use confluence_app::engine_launch::Launcher;

#[test]
fn the_engine_still_starts_when_breakaway_is_not_allowed() {
    let job = job::join_kill_on_close_job(false);
    let (exe, args) = job::long_runner();
    let mut launcher = Launcher::new(exe, args);
    let started = launcher.start(Instant::now());
    let pid = launcher.pid();
    if let Some(pid) = pid {
        assert!(job::in_job(pid, job), "it could only start inside the job");
        job::kill(pid);
    }
    assert!(started.is_ok(), "{started:?}");
    assert!(pid.is_some());
}
