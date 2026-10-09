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
/// (Never this PC's MIDI devices.)
fn engine_command(pipe: &str, journal: &std::path::Path) -> Process {
    let mut cmd = Process::new(env!("CARGO_BIN_EXE_confluence-engine"));
    // Never the real VASIO streams or saved shapes (a fresh setup opens VASIO A).
    cmd.env(confluence_provider_vasio::NAMESPACE_VAR, format!("test-{}", std::process::id())).env(
        confluence_provider_vasio::config::ROOT_VAR,
        format!(r"Software\ConfluenceTest\VASIO.{}", std::process::id()),
    );
    cmd.args(["--pipe", pipe, "--journal"]).arg(journal).arg("--devices").arg(journal.with_file_name("devices.json"));
    cmd.arg("--no-midi");
    // Loopback only, any free port, never advertised: no test touches the LAN.
    cmd.args(["--no-net-discovery", "--net-bind", "127.0.0.1", "--net-port", "0"]);
    cmd
}

/// An empty plugin folder next to the journal: never this PC's own plugins.
fn no_plugins(journal: &std::path::Path) -> std::path::PathBuf {
    let d = journal.with_file_name("no-plugins");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn spawn(pipe: &str, journal: &std::path::Path) -> Engine {
    let mut cmd = engine_command(pipe, journal);
    cmd.arg("--clap-path").arg(no_plugins(journal));
    Engine(Some(cmd.spawn().unwrap()))
}

/// Runs a second engine that is expected to refuse to start. If it is still
/// running after 10 s it wrongly started: kill it and return `None`.
fn run_expecting_exit(pipe: &str, journal: &std::path::Path) -> Option<(std::process::ExitStatus, String)> {
    use std::io::Read;
    let mut cmd = engine_command(pipe, journal);
    cmd.arg("--clap-path").arg(no_plugins(journal));
    let mut child = cmd.stderr(std::process::Stdio::piped()).spawn().unwrap();
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
    assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
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
        assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
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
    assert!(matches!(c.call(set(1)).unwrap(), Response::Applied { .. }));

    // Same pipe and same journal, as with two default-configured engines.
    let (status, stderr) = run_expecting_exit(&pipe, &journal).expect("second instance must not keep running");
    assert!(!status.success(), "second instance must refuse to start");
    assert!(stderr.contains("already running"), "clear message, got: {stderr}");

    // The first engine is unaffected and its journal still records changes.
    assert!(matches!(c.call(set(2)).unwrap(), Response::Applied { .. }));
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
    assert!(matches!(c.call(on_device).unwrap(), Response::Applied { .. }));
    assert!(matches!(c.call(elsewhere).unwrap(), Response::Applied { .. }));
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let offline = slots.iter().find(|s| !s.online).expect("the missing device holds its channels");
    assert!(matches!(c.call(Command::RemoveSlot { id: offline.id }).unwrap(), Response::Applied { .. }));
    shutdown(child, &mut c);

    // Whatever device takes inputs 0..2 next must not inherit the old route.
    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    let routes: Vec<(u32, u32)> = points.iter().map(|p| (p.input, p.output)).collect();
    assert_eq!(routes, vec![(10, 11)]);
    shutdown(child, &mut c);
}

#[test]
fn an_insert_bus_and_its_routes_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    // A saved device that is not installed holds inputs/outputs 0..2 offline.
    let binding = r#"{"master":null,"devices":[{"kind":"Asio","name":"no such driver (test)",
        "first_input":0,"inputs":2,"first_output":0,"outputs":2}]}"#;
    std::fs::write(dir.path().join("devices.json"), binding).unwrap();
    let pipe = format!("confluence-bus-restart-{}", std::process::id());

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let add = Command::AddBus { name: "Verb".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!("AddBus replies Added") };
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let bus = slots.iter().find(|s| s.id == ids[0]).unwrap().clone();
    assert!(bus.is_bus());
    let set = |input, output, gain_db| Command::SetPoint { input, output, gain_db, mute: false, invert: false };
    assert!(matches!(c.call(set(1, bus.first_output, 0.0)).unwrap(), Response::Applied { .. }));
    assert!(matches!(c.call(set(bus.first_input, 1, -3.0)).unwrap(), Response::Applied { .. }));
    let looped = c.call(set(bus.first_input, bus.first_output, 0.0)).unwrap();
    assert_eq!(looped, Response::Error("this route would feed an insert bus back into itself".into()));
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let again = slots.iter().find(|s| s.is_bus()).expect("the bus is back");
    assert_eq!(
        (again.name.as_str(), again.first_input, again.inputs, again.first_output, again.outputs),
        ("Verb", bus.first_input, 2, bus.first_output, 2)
    );
    assert!(slots.iter().any(|s| !s.online), "the offline device kept its channels too");
    let Response::Points(points) = c.call(Command::ListPoints).unwrap() else { panic!() };
    let routes: Vec<(u32, u32)> = points.iter().map(|p| (p.input, p.output)).collect();
    assert_eq!(routes.len(), 2, "{routes:?}");
    assert!(routes.contains(&(1, bus.first_output)) && routes.contains(&(bus.first_input, 1)), "{routes:?}");
    // Removing the bus is permanent too.
    assert!(matches!(c.call(Command::RemoveSlot { id: again.id }).unwrap(), Response::Applied { .. }));
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    assert!(!slots.iter().any(|s| s.is_bus()));
    shutdown(child, &mut c);
}

/// `target/debug/confluence_test_plugin.dll`, built by `cargo build --workspace`.
fn test_plugin_dll() -> std::path::PathBuf {
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_confluence-engine"));
    let path = exe.parent().unwrap().join("confluence_test_plugin.dll");
    assert!(path.is_file(), "{} is missing: run `cargo build --workspace` first", path.display());
    path
}

/// The engine with its plugin folder set to `clap_dir` (never the user's own).
fn spawn_with_plugins(pipe: &str, journal: &std::path::Path, clap_dir: &std::path::Path) -> Engine {
    let mut cmd = engine_command(pipe, journal);
    cmd.arg("--clap-path").arg(clap_dir);
    Engine(Some(cmd.spawn().unwrap()))
}

fn bus_plugins(pipe: &str) -> Vec<confluence_api::LoadedPlugin> {
    let (state, _sub) = confluence_client::Subscription::connect(pipe, Duration::from_secs(10)).unwrap();
    state.bus_plugins
}

#[test]
fn a_plugin_on_a_bus_keeps_its_settings_across_restarts() {
    use confluence_api::{BusRef, PluginStatus};
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let clap_dir = dir.path().join("clap");
    std::fs::create_dir(&clap_dir).unwrap();
    let file = clap_dir.join("Test.clap");
    std::fs::copy(test_plugin_dll(), &file).unwrap();
    let path = file.display().to_string();
    let pipe = format!("confluence-plugin-restart-{}", std::process::id());

    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let add = Command::AddBus { name: "FX".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!() };
    let bus = BusRef::Id(ids[0]);
    let load = Command::LoadPlugin { bus, path: path.clone(), plugin_id: "dev.confluence.test.gain".into() };
    let r = c.call(load).unwrap();
    assert!(matches!(r, Response::Applied { .. }), "{r:?}");
    let r = c.call(Command::SetParam { bus, param: 1, value: -6.0 }).unwrap();
    assert!(matches!(r, Response::Applied { .. }), "{r:?}");
    let shown = bus_plugins(&pipe);
    assert_eq!(shown.len(), 1);
    assert_eq!((shown[0].info.name.as_str(), &shown[0].status), ("Confluence Test Gain", &PluginStatus::Running));
    // The plugin's own text follows shortly (it is asked for without waiting).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while bus_plugins(&pipe)[0].params[0].text != "-6.0 dB" {
        assert!(std::time::Instant::now() < deadline, "the plugin's text never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The crash plugin is caught by the load check; the engine carries on.
    let crash = Command::LoadPlugin { bus, path: path.clone(), plugin_id: "dev.confluence.test.crash".into() };
    let Response::Error(e) = c.call(crash).unwrap() else { panic!("the crash plugin must be refused") };
    assert!(e.contains("crashed while loading"), "{e}");
    assert!(matches!(c.call(Command::Status).unwrap(), Response::Status(_)));
    assert_eq!(bus_plugins(&pipe)[0].info.name, "Confluence Test Gain", "the bus kept its plugin");
    // The plugins found in the folder are listed.
    let Response::Plugins(found) = c.call(Command::ListPlugins).unwrap() else { panic!() };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut found = found;
    while found.len() < 5 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let Response::Plugins(f) = c.call(Command::ListPlugins).unwrap() else { panic!() };
        found = f;
    }
    assert_eq!(found.len(), 5, "{found:?}");
    shutdown(child, &mut c);

    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let shown = bus_plugins(&pipe);
    assert_eq!(shown.len(), 1, "the plugin came back");
    assert_eq!(shown[0].status, PluginStatus::Running);
    assert_eq!(shown[0].params[0].value, -6.0, "with its setting");
    shutdown(child, &mut c);

    // Its file disappears: the bus is kept silent and the plugin remembered.
    std::fs::rename(&file, clap_dir.join("away.bin")).unwrap();
    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let shown = bus_plugins(&pipe);
    assert!(matches!(shown[0].status, PluginStatus::Failed(_)), "{:?}", shown[0].status);
    shutdown(child, &mut c);
    std::fs::rename(clap_dir.join("away.bin"), &file).unwrap();
    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let shown = bus_plugins(&pipe);
    assert_eq!(shown[0].status, PluginStatus::Running, "back when the file is back");
    assert_eq!(shown[0].params[0].value, -6.0);
    assert!(matches!(
        c.call(Command::UnloadPlugin { bus: BusRef::Id(shown[0].bus) }).unwrap(),
        Response::Applied { .. }
    ));
    shutdown(child, &mut c);
    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert!(bus_plugins(&pipe).is_empty(), "unloading is saved too");
    shutdown(child, &mut c);
}

/// A setting made, then the engine killed twice (the second time right after
/// it started and rewrote its journal): the setting survives both.
#[test]
fn a_plugin_setting_survives_an_engine_killed_twice() {
    use confluence_api::BusRef;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let clap_dir = dir.path().join("clap");
    std::fs::create_dir(&clap_dir).unwrap();
    let file = clap_dir.join("Test.clap");
    std::fs::copy(test_plugin_dll(), &file).unwrap();
    let pipe = format!("confluence-plugin-killed-{}", std::process::id());

    let mut child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let add = Command::AddBus { name: "FX".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!() };
    let bus = BusRef::Id(ids[0]);
    let load =
        Command::LoadPlugin { bus, path: file.display().to_string(), plugin_id: "dev.confluence.test.gain".into() };
    assert!(matches!(c.call(load).unwrap(), Response::Applied { .. }));
    for v in 1..=20 {
        let set = Command::SetParam { bus, param: 1, value: -(v as f64) };
        assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
    }
    child.kill();

    let mut child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let _c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(bus_plugins(&pipe)[0].params[0].value, -20.0);
    child.kill();

    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(bus_plugins(&pipe)[0].params[0].value, -20.0, "kept through the second kill too");
    shutdown(child, &mut c);
}

fn editor_window(title: &str) -> Option<windows::Win32::Foundation::HWND> {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    // SAFETY: a read-only lookup.
    unsafe { FindWindowW(None, &HSTRING::from(title)) }.ok()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The engine opens a plugin's editor; it closes when asked, when the plugin
/// is unloaded, and when the engine exits; a change made in it is saved.
#[test]
fn a_plugins_editor_is_opened_by_the_engine_and_its_changes_are_saved() {
    use confluence_api::BusRef;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, SendMessageW, WM_LBUTTONDOWN};
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let clap_dir = dir.path().join("clap");
    std::fs::create_dir(&clap_dir).unwrap();
    let file = clap_dir.join("Test.clap");
    std::fs::copy(test_plugin_dll(), &file).unwrap();
    let pipe = format!("confluence-plugin-editor-{}", std::process::id());
    let title = format!("Confluence Test Gain — Editor bus {}", std::process::id());
    let bus_name = format!("Editor bus {}", std::process::id());

    let mut child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let add = Command::AddBus { name: bus_name, channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(add).unwrap() else { panic!() };
    let bus = BusRef::Id(ids[0]);
    let load =
        Command::LoadPlugin { bus, path: file.display().to_string(), plugin_id: "dev.confluence.test.gain".into() };
    assert!(matches!(c.call(load.clone()).unwrap(), Response::Applied { .. }));
    assert!(bus_plugins(&pipe)[0].has_editor);

    assert_eq!(c.call(Command::ShowEditor { bus }).unwrap(), Response::Ok);
    let w = editor_window(&title).expect("the editor window");
    wait_until("editor_open", || bus_plugins(&pipe)[0].editor_open);
    assert_eq!(c.call(Command::HideEditor { bus }).unwrap(), Response::Ok);
    assert!(editor_window(&title).is_none());

    // Unloading with the editor open closes it.
    assert_eq!(c.call(Command::ShowEditor { bus }).unwrap(), Response::Ok);
    assert!(matches!(c.call(Command::UnloadPlugin { bus }).unwrap(), Response::Applied { .. }));
    wait_until("the window gone after unloading", || editor_window(&title).is_none());
    let _ = w;
    // Let the old instance be destroyed entirely (nothing else holds its file),
    // so loading it again really loads the file again.
    std::thread::sleep(Duration::from_millis(500));

    // A click in the editor (Gain −12 dB) is saved even if the engine is killed.
    assert!(matches!(c.call(load).unwrap(), Response::Applied { .. }));
    assert_eq!(c.call(Command::ShowEditor { bus }).unwrap(), Response::Ok);
    let w = editor_window(&title).unwrap();
    // SAFETY: a lookup, then a click on the plugin's own window in the engine process.
    let inner = unsafe { FindWindowExW(Some(w), None, windows::core::w!("ConfluenceTestGainEditor"), None) }.unwrap();
    unsafe { SendMessageW(inner, WM_LBUTTONDOWN, Some(WPARAM(0)), Some(LPARAM(0))) };
    wait_until("the edit in the engine", || bus_plugins(&pipe)[0].params[0].value == -12.0);
    std::thread::sleep(Duration::from_millis(300)); // the next publish journals it
    child.kill();
    wait_until("the window gone with the engine", || editor_window(&title).is_none());

    let child = spawn_with_plugins(&pipe, &journal, &clap_dir);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(bus_plugins(&pipe)[0].params[0].value, -12.0, "the editor's change was saved");
    // A clean exit with the editor open.
    let bus = BusRef::Id(bus_plugins(&pipe)[0].bus);
    assert_eq!(c.call(Command::ShowEditor { bus }).unwrap(), Response::Ok);
    shutdown(child, &mut c);
    assert!(editor_window(&title).is_none());
}

/// A plugin scan running when the engine dies must not outlive it.
#[test]
fn scan_processes_end_with_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let clap_dir = dir.path().join("clap");
    std::fs::create_dir(&clap_dir).unwrap();
    std::fs::copy(test_plugin_dll(), clap_dir.join("Slow.clap")).unwrap();
    let mark = dir.path().join("scan-finished");
    let pipe = format!("confluence-scan-orphan-{}", std::process::id());
    let mut cmd = engine_command(&pipe, &journal);
    cmd.arg("--clap-path").arg(&clap_dir);
    cmd.env("CONFLUENCE_TEST_PLUGIN_SLOW_LOAD_MS", "4000").env("CONFLUENCE_TEST_PLUGIN_SLOW_LOAD_MARK", &mark);
    let mut child = Engine(Some(cmd.spawn().unwrap()));
    let _c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    std::thread::sleep(Duration::from_millis(1000)); // the scan of Slow.clap is under way
    child.kill();
    std::thread::sleep(Duration::from_millis(5000));
    assert!(!mark.exists(), "the scan process outlived its engine");
}

fn scenes_now(pipe: &str) -> confluence_api::State {
    confluence_client::Subscription::connect(pipe, Duration::from_secs(10)).unwrap().0
}

/// Scenes are saved with the project; a recall survives a restart, even one in
/// the middle of its morph (the mix comes back where it was going).
#[test]
fn scenes_and_recalls_survive_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-scenes-{}", std::process::id());
    let set = |gain_db| Command::SetPoint { input: 1, output: 2, gain_db, mute: false, invert: false };

    let mut child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert!(matches!(c.call(set(-40.0)).unwrap(), Response::Applied { .. }));
    let save = Command::SaveScene { name: "Quiet".into(), morph_ms: 10_000 };
    assert!(matches!(c.call(save).unwrap(), Response::Applied { .. }));
    assert!(matches!(c.call(set(0.0)).unwrap(), Response::Applied { .. }));
    let st = scenes_now(&pipe);
    assert_eq!(st.scenes.len(), 1);
    assert_eq!(st.current_scene, None);
    assert!(matches!(c.call(Command::RecallScene { name: "Quiet".into() }).unwrap(), Response::Applied { .. }));
    let st = scenes_now(&pipe);
    assert_eq!(st.current_scene.as_deref(), Some("Quiet"));
    assert!(st.morphing, "a 10 s morph is under way");
    child.kill(); // in the middle of it

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let st = scenes_now(&pipe);
    assert_eq!(st.scenes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Quiet"]);
    assert_eq!(st.points[0].gain_db, -40.0, "where the morph was going");
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(scenes_now(&pipe).points[0].gain_db, -40.0, "and after a clean restart");
    assert!(matches!(c.call(Command::DeleteScene { name: "Quiet".into() }).unwrap(), Response::Applied { .. }));
    shutdown(child, &mut c);
    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert!(scenes_now(&pipe).scenes.is_empty(), "deleting is saved too");
    shutdown(child, &mut c);
}

/// MIDI Learn and a bound control over the pipe (messages injected: no MIDI
/// hardware needed); the binding survives a restart.
#[test]
fn a_learned_midi_control_drives_a_route_and_is_kept() {
    use confluence_api::MidiBinding;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-midi-{}", std::process::id());
    let set = Command::SetPoint { input: 1, output: 2, gain_db: -6.0, mute: false, invert: false };
    let inject = |v: u8| Command::InjectMidi { device: "nanoKONTROL2".into(), bytes: vec![0xB0, 7, v] };

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
    assert_eq!(c.call(Command::LearnMidi { input: 1, output: 2 }).unwrap(), Response::Ok);
    assert_eq!(scenes_now(&pipe).midi_learning, Some((1, 2)));
    assert!(matches!(c.call(inject(90)).unwrap(), Response::Applied { .. }));
    let st = scenes_now(&pipe);
    let bound = MidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7, input: 1, output: 2 };
    assert_eq!(st.midi_bindings, vec![bound.clone()]);
    assert_eq!(st.midi_learning, None);
    assert!(matches!(c.call(inject(127)).unwrap(), Response::Applied { .. }));
    assert_eq!(scenes_now(&pipe).points[0].gain_db, 12.0);
    let bad = Command::InjectMidi { device: "x".into(), bytes: vec![1, 2, 3, 4] };
    assert!(matches!(c.call(bad).unwrap(), Response::Error(_)));
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let st = scenes_now(&pipe);
    assert_eq!(st.midi_bindings, vec![bound]);
    assert_eq!(st.points[0].gain_db, 12.0);
    shutdown(child, &mut c);
}

#[test]
fn a_script_reacts_to_midi_and_survives_a_restart() {
    use confluence_api::ScriptStatus;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-script-{}", std::process::id());
    let source = "function on_midi(m)
        if m.kind == 'note_on' then confluence.set_route(1, 2, -m.note) end
    end";
    let note = |n: u8| Command::InjectMidi { device: "Pad".into(), bytes: vec![0x90, n, 100] };

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let set = Command::SetScript { name: "notes".into(), source: source.into(), enabled: true };
    assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
    let broken = Command::SetScript { name: "broken".into(), source: "function (".into(), enabled: true };
    assert!(matches!(c.call(broken).unwrap(), Response::Applied { .. }));
    assert!(matches!(c.call(note(10)).unwrap(), Response::Applied { .. }));
    let st = scenes_now(&pipe);
    assert_eq!((st.points.len(), st.points[0].gain_db), (1, -10.0));
    assert_eq!(st.scripts.len(), 2);
    assert!(matches!(&st.scripts[0].status, ScriptStatus::Stopped(why) if !why.is_empty()), "broken");
    assert_eq!(st.scripts[1].status, ScriptStatus::Running);
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let st = scenes_now(&pipe);
    assert_eq!(st.points[0].gain_db, -10.0, "the script's edit was saved");
    assert_eq!(st.scripts[1].status, ScriptStatus::Running, "the script came back");
    assert!(matches!(c.call(note(20)).unwrap(), Response::Applied { .. }));
    assert_eq!(scenes_now(&pipe).points[0].gain_db, -20.0, "and runs");
    shutdown(child, &mut c);

    // That start rewrote the journal as the current state: scripts kept.
    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(scenes_now(&pipe).scripts.len(), 2, "kept by compaction");
    shutdown(child, &mut c);
}

/// A free UDP port on loopback (taken again by an engine just after).
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn spawn_net(pipe: &str, journal: &std::path::Path, port: u16) -> Engine {
    let mut cmd = engine_command(pipe, journal);
    cmd.arg("--clap-path").arg(no_plugins(journal));
    cmd.args(["--net-port", &port.to_string()]);
    Engine(Some(cmd.spawn().unwrap()))
}

/// The health of the slot fed by device `device`.
fn net_health(c: &mut Client, pipe: &str, device: &str) -> Option<confluence_api::SlotHealth> {
    let (state, _sub) = confluence_client::Subscription::connect(pipe, Duration::from_secs(10)).unwrap();
    let id = state.slots.iter().find(|s| s.device == device)?.id;
    match c.call(Command::Health).unwrap() {
        Response::Health { slots, .. } => slots.into_iter().find(|h| h.id == id),
        other => panic!("{other:?}"),
    }
}

#[test]
fn two_engines_stream_to_each_other_over_the_network() {
    use confluence_api::DeviceKind;
    let dir = tempfile::tempdir().unwrap();
    let (ja, jb) = (dir.path().join("a").join("journal.bin"), dir.path().join("b").join("journal.bin"));
    std::fs::create_dir_all(ja.parent().unwrap()).unwrap();
    std::fs::create_dir_all(jb.parent().unwrap()).unwrap();
    let (pa, pb) =
        (format!("confluence-net-a-{}", std::process::id()), format!("confluence-net-b-{}", std::process::id()));
    let (port_a, port_b) = (free_udp_port(), free_udp_port());

    let a = spawn_net(&pa, &ja, port_a);
    let b = spawn_net(&pb, &jb, port_b);
    let mut ca = Client::connect(&pa, Duration::from_secs(10)).unwrap();
    let mut cb = Client::connect(&pb, Duration::from_secs(10)).unwrap();
    let send = Command::AddDevice { kind: DeviceKind::NetSend, name: format!("127.0.0.1:{port_b}/Main:2") };
    assert!(matches!(ca.call(send).unwrap(), Response::Added { .. } | Response::SlotsAdded(_)), "send stream added");

    // B hears the stream before anyone adds it.
    wait_until("the stream heard by B", || match cb.call(Command::ListDevices).unwrap() {
        Response::Devices(d) => {
            d.iter().any(|x| x.kind == DeviceKind::NetReceive && x.name == "127.0.0.1/Main" && x.inputs == 2)
        }
        _ => false,
    });
    let recv = Command::AddDevice { kind: DeviceKind::NetReceive, name: "127.0.0.1/Main".into() };
    assert!(matches!(cb.call(recv).unwrap(), Response::Added { .. } | Response::SlotsAdded(_)), "receive stream added");
    let device = "net-in:127.0.0.1/Main";
    wait_until("packets in B's slot", || {
        net_health(&mut cb, &pb, device).and_then(|h| h.net).is_some_and(|n| n.packets > 500)
    });
    let h = net_health(&mut cb, &pb, device).unwrap();
    let n = h.net.unwrap();
    assert_eq!((n.lost, n.late, n.malformed), (0, 0, 0), "{h:?}");
    assert!(!h.device_lost, "{h:?}");

    // A stops: B's stream is reported lost, its slot kept.
    shutdown(a, &mut ca);
    wait_until("B notices", || net_health(&mut cb, &pb, device).is_some_and(|h| h.device_lost));

    // B restarts: the stream comes back on its own, from a restarted A.
    shutdown(b, &mut cb);
    let a = spawn_net(&pa, &ja, port_a);
    let b = spawn_net(&pb, &jb, port_b);
    let mut ca = Client::connect(&pa, Duration::from_secs(10)).unwrap();
    let mut cb = Client::connect(&pb, Duration::from_secs(10)).unwrap();
    wait_until("packets after both restarted", || {
        net_health(&mut cb, &pb, device).and_then(|h| h.net).is_some_and(|n| n.packets > 100)
    });
    shutdown(a, &mut ca);
    shutdown(b, &mut cb);
}

#[test]
fn a_devices_colour_survives_a_killed_engine_and_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let binding = r#"{"master":null,"devices":[{"kind":"Asio","name":"no such driver (test)",
        "first_input":0,"inputs":2,"first_output":0,"outputs":2}]}"#;
    std::fs::write(dir.path().join("devices.json"), binding).unwrap();
    let pipe = format!("confluence-colour-{}", std::process::id());
    let colour_now = |c: &mut Client| -> Option<[u8; 3]> {
        let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
        slots.iter().find(|s| s.device == "asio:no such driver (test)").expect("the device's slot").color
    };

    let mut child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let id = slots[0].id;
    let set = Command::SetSlotColor { id, color: Some([0x40, 0xa0, 0xff]) };
    assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
    assert_eq!(colour_now(&mut c), Some([0x40, 0xa0, 0xff]));
    child.kill(); // no compaction: the appended record must do

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(colour_now(&mut c), Some([0x40, 0xa0, 0xff]), "kept after a kill");
    shutdown(child, &mut c); // compacts the journal

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(colour_now(&mut c), Some([0x40, 0xa0, 0xff]), "kept by compaction");
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let reset = Command::SetSlotColor { id: slots[0].id, color: None };
    assert!(matches!(c.call(reset).unwrap(), Response::Applied { .. }));
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(colour_now(&mut c), None, "back to the default for good");
    shutdown(child, &mut c);
}

#[test]
fn a_version_1_setup_migrates_once_with_backups() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let devices = dir.path().join("devices.json");
    // Nine Windows outputs (one too many) and a VASIO; no VAIO driver in tests.
    let fixture =
        std::fs::read_to_string(format!("{}/tests/fixtures/devices-v1-overflow.json", env!("CARGO_MANIFEST_DIR")))
            .unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    v["devices"].as_array_mut().unwrap().retain(|d| d["kind"] != "Vaio");
    std::fs::write(&devices, serde_json::to_string(&v).unwrap()).unwrap();
    {
        let (mut j, _) = confluence_engine::journal::Journal::open(&journal).unwrap();
        j.append(&Command::SetColor { key: "wasapi-out:Out 1".into(), color: Some([1, 2, 3]) }).unwrap();
    }
    let pipe = format!("confluence-migrate-{}", std::process::id());

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    // A served request means start-up (and the migration) has finished.
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    assert!(dir.path().join("devices.v1.json").exists(), "devices backed up");
    assert!(dir.path().join("journal.v1.bin").exists(), "journal backed up");
    assert!(std::fs::read_to_string(&devices).unwrap().contains("\"version\": 2"));
    let state = scenes_now(&pipe);
    assert!(state.notices.iter().any(|n| n.contains("Out 9")), "{:?}", state.notices);
    let mut firsts: Vec<u32> =
        slots.iter().filter(|s| s.device.starts_with("wasapi-out:")).map(|s| s.first_output).collect();
    firsts.sort();
    // The outputs are offline here (no such endpoints), but keep their channels.
    assert_eq!(firsts, vec![0, 2, 4, 6, 8, 10, 12, 14]);
    let out1 = slots.iter().find(|s| s.device == "wasapi-out:Out 1").unwrap();
    assert_eq!(out1.color, Some([1, 2, 3]), "the colour moved to its position");
    let backup_len = std::fs::metadata(dir.path().join("devices.v1.json")).unwrap().len();
    shutdown(child, &mut c);

    let child = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    c.call(Command::ListSlots).unwrap();
    assert_eq!(std::fs::metadata(dir.path().join("devices.v1.json")).unwrap().len(), backup_len, "not migrated twice");
    let state = scenes_now(&pipe);
    assert!(!state.notices.iter().any(|n| n.contains("Out 9")), "the note was a one-time one");
    shutdown(child, &mut c);
}

#[test]
fn routes_moved_by_a_swap_survive_an_engine_that_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-swap-kill-{}", std::process::id());
    let pos = |s: &str| -> confluence_api::PosId { s.parse().unwrap() };
    let fill = |p: &str, name: &str| Command::FillPosition {
        pos: pos(p),
        kind: confluence_api::DeviceKind::NetSend,
        name: name.into(),
    };
    let first_output = |c: &mut Client, stream: &str| {
        let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
        let device = format!("net-out:127.0.0.1:9/{stream}");
        slots.iter().find(|s| s.device.starts_with(&device)).unwrap().first_output
    };
    let points = |c: &mut Client| {
        let Response::Points(p) = c.call(Command::ListPoints).unwrap() else { panic!() };
        p.into_iter().map(|p| (p.input, p.output)).collect::<Vec<_>>()
    };
    let mut engine = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert!(matches!(c.call(fill("net-out:1", "127.0.0.1:9/A:2")).unwrap(), Response::Added { .. }));
    assert!(matches!(c.call(fill("net-out:2", "127.0.0.1:9/B:2")).unwrap(), Response::Added { .. }));
    let before = first_output(&mut c, "A");
    let set = Command::SetPoint { input: 0, output: before + 1, gain_db: -6.0, mute: false, invert: false };
    assert!(matches!(c.call(set).unwrap(), Response::Applied { .. }));
    // Another stream, of eight channels, does not fit where it is (net-out:2 follows it): it moves, its route with it.
    let r = c.call(fill("net-out:1", "127.0.0.1:9/C:8")).unwrap();
    assert!(matches!(r, Response::Added { .. }), "{r:?}");
    let after = first_output(&mut c, "C");
    assert_ne!(after, before, "moved to a free block");
    assert_eq!(points(&mut c), vec![(0, after + 1)]);
    engine.kill(); // no clean shutdown: only what was saved counts
    let _engine = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(first_output(&mut c, "C"), after);
    assert_eq!(points(&mut c), vec![(0, after + 1)], "the route is where the swap moved it");
    c.call(Command::Shutdown).unwrap();
}

#[test]
fn custom_names_survive_an_engine_that_is_killed_and_a_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.bin");
    let pipe = format!("confluence-labels-{}", std::process::id());
    let mut engine = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
    let vasio = slots.iter().find(|s| s.name == "VASIO A").expect("a fresh setup has VASIO A").id;
    let set = |name: &str, channel| Command::SetSlotLabel { id: vasio, channel, name: Some(name.into()) };
    let ch = Some(confluence_api::ChannelRef { input: true, index: 0 });
    assert!(matches!(c.call(set("Ableton", None)).unwrap(), Response::Applied { .. }));
    assert!(matches!(c.call(set("Kick", ch)).unwrap(), Response::Applied { .. }));
    engine.kill();
    let mut engine = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    let names = |c: &mut Client| {
        let Response::Slots(slots) = c.call(Command::ListSlots).unwrap() else { panic!() };
        let s = slots.into_iter().find(|s| s.name == "VASIO A").unwrap();
        (s.label, s.input_labels.first().cloned().flatten())
    };
    assert_eq!(names(&mut c), (Some("Ableton".into()), Some("Kick".into())));
    // A compaction (removing a slot rewrites the journal) keeps them too.
    let bus = Command::AddBus { name: "Verb".into(), channels: 2, first_input: None, first_output: None };
    let Response::Added { ids, .. } = c.call(bus).unwrap() else { panic!() };
    assert!(matches!(c.call(Command::RemoveSlot { id: ids[0] }).unwrap(), Response::Applied { .. }));
    engine.kill();
    let _engine = spawn(&pipe, &journal);
    let mut c = Client::connect(&pipe, Duration::from_secs(10)).unwrap();
    assert_eq!(names(&mut c), (Some("Ableton".into()), Some("Kick".into())));
    c.call(Command::Shutdown).unwrap();
}
