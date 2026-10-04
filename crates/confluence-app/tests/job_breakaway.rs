//! An engine started from the window must outlive it, even when the window
//! itself runs in a kill-on-close job (as some terminals and IDEs arrange).
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/job.rs"]
mod job;

use std::time::Instant;

use confluence_app::engine_launch::Launcher;

#[test]
fn the_started_engine_leaves_a_kill_on_close_job() {
    let job = job::join_kill_on_close_job(true);
    let (exe, args) = job::long_runner();
    let mut launcher = Launcher::new(exe, args);
    launcher.start(Instant::now()).unwrap();
    let pid = launcher.pid().unwrap();
    let inside = job::in_job(pid, job);
    job::kill(pid);
    assert!(!inside, "the started process is outside the job, so closing the window cannot kill it");
}
