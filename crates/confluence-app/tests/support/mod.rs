//! Helpers for UI tests: a real engine in a temp dir, and the app in a headless harness.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use confluence_api::{Command, Response, SlotState};
use confluence_app::app::{AppConfig, ConfluenceApp};
use confluence_client::Client;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

/// `target/debug/confluence-engine.exe`, next to this test's `deps` folder.
pub fn engine_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().and_then(|d| d.parent()).unwrap().join("confluence-engine.exe");
    assert!(path.is_file(), "{} is missing: run `cargo build -p confluence-engine` first", path.display());
    path
}

/// A pipe name and a temp dir for one test's engine.
pub struct EngineDir {
    pub dir: tempfile::TempDir,
    pub pipe: String,
}

impl EngineDir {
    pub fn new(tag: &str) -> Self {
        confluence_provider_vasio::isolate_for_tests(); // inherited by every engine started from here
        EngineDir { dir: tempfile::tempdir().unwrap(), pipe: format!("confluence-ui-{tag}-{}", std::process::id()) }
    }

    pub fn args(&self) -> Vec<String> {
        vec![
            "--pipe".into(),
            self.pipe.clone(),
            "--journal".into(),
            self.dir.path().join("journal.bin").display().to_string(),
            "--devices".into(),
            self.dir.path().join("devices.json").display().to_string(),
            // Never the user's own plugins: only what a test puts in this folder.
            "--clap-path".into(),
            self.clap_dir().display().to_string(),
            // Never this PC's MIDI devices (tests inject MIDI over the pipe).
            "--no-midi".into(),
            // Loopback only, any free port, never advertised on the network.
            "--no-net-discovery".into(),
            "--net-bind".into(),
            "127.0.0.1".into(),
            "--net-port".into(),
            "0".into(),
        ]
    }

    /// This test's plugin folder (created on first use).
    pub fn clap_dir(&self) -> PathBuf {
        let d = self.dir.path().join("clap");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// Puts the test plugin file (`cargo build --workspace` builds it) in the plugin folder.
    pub fn add_test_plugin(&self) {
        let dll = engine_exe().parent().unwrap().join("confluence_test_plugin.dll");
        assert!(dll.is_file(), "{} is missing: run `cargo build --workspace` first", dll.display());
        std::fs::copy(dll, self.clap_dir().join("ConfluenceTest.clap")).unwrap();
    }
}

/// An engine process, killed when dropped.
pub struct Engine(Option<Child>);

impl Engine {
    pub fn spawn(d: &EngineDir) -> Engine {
        Self::spawn_with(d, &[])
    }

    /// With more arguments (later ones override `d.args()`).
    pub fn spawn_with(d: &EngineDir, extra: &[&str]) -> Engine {
        let child = Process::new(engine_exe()).args(d.args()).args(extra).stderr(Stdio::null()).spawn().unwrap();
        Engine(Some(child))
    }

    pub fn kill(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Shuts down whatever engine serves the pipe (one the GUI started has no handle here).
pub struct ShutdownOnDrop(pub String);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        if let Ok(mut c) = Client::connect(&self.0, Duration::from_millis(500)) {
            let _ = c.call(Command::Shutdown);
        }
    }
}

pub fn client(d: &EngineDir) -> Client {
    Client::connect(&d.pipe, Duration::from_secs(10)).unwrap()
}

pub fn slots(c: &mut Client) -> Vec<SlotState> {
    match c.call(Command::ListSlots).unwrap() {
        Response::Slots(s) => s,
        other => panic!("{other:?}"),
    }
}

pub fn app_for(d: &EngineDir) -> ConfluenceApp {
    app_with_skin(d, None)
}

pub fn app_with_skin(d: &EngineDir, skin: Option<PathBuf>) -> ConfluenceApp {
    ConfluenceApp::new(AppConfig { pipe: d.pipe.clone(), engine_exe: engine_exe(), engine_args: d.args(), skin })
}

pub fn harness(app: ConfluenceApp) -> Harness<'static, ConfluenceApp> {
    harness_sized(app, 1200.0, 800.0)
}

/// A harness with a small window, so the grid scrolls.
pub fn harness_sized(app: ConfluenceApp, w: f32, h: f32) -> Harness<'static, ConfluenceApp> {
    Harness::builder().with_size([w, h]).build_ui_state(|ui, app: &mut ConfluenceApp| app.draw(ui), app)
}

/// A harness whose frames are 1/60 s apart (the default is 1/4 s, so the
/// clicks of a double-click, a frame per event, would be too far apart).
pub fn harness_fast(app: ConfluenceApp) -> Harness<'static, ConfluenceApp> {
    Harness::builder()
        .with_size([1200.0, 800.0])
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(|ui, app: &mut ConfluenceApp| app.draw(ui), app)
}

/// Runs frames until `cond` holds, failing after `timeout`.
pub fn pump_until(
    h: &mut Harness<'static, ConfluenceApp>,
    what: &str,
    timeout: Duration,
    mut cond: impl FnMut(&Harness<'static, ConfluenceApp>) -> bool,
) {
    let start = Instant::now();
    loop {
        h.step();
        if cond(h) {
            return;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A few frames, for clicks to be processed.
pub fn settle(h: &mut Harness<'static, ConfluenceApp>) {
    for _ in 0..3 {
        h.step();
    }
}

/// Clicks the widget labelled `label` once it has stopped moving (panels
/// and dialogs animate into place; a busy machine could otherwise click
/// where it was a frame earlier).
pub fn click_when_still(h: &mut Harness<'static, ConfluenceApp>, label: &str) {
    let mut last = h.get_by_label(label).rect();
    pump_until(h, label, Duration::from_secs(15), |h| {
        let now = h.get_by_label(label).rect();
        let still = now == last;
        last = now;
        still
    });
    h.get_by_label(label).click();
}
