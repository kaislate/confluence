//! `confluence-engine`: the background engine process. Runs the internal clock
//! as master, serves the Control API on a named pipe and journals changes.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    let args = app::Args::parse();
    if args.scan.is_some() {
        return app::scan(&args);
    }
    match app::run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("confluence-engine: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("confluence-engine runs on Windows only");
}

#[cfg(windows)]
mod app {
    use std::error::Error;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex, MutexGuard, Weak};
    use std::time::{Duration, Instant};

    use confluence_api::{
        BusRef, Command, DeviceInfo, EngineStatus, Event, ParamState, PluginInfo, Response, SlotHealth,
    };
    use confluence_engine::clock::InternalClock;
    use confluence_engine::devices::{start_asio_master, DeviceManager, NetCtx};
    use confluence_engine::ipc::{default_pipe_name, pipe_path, PipeServer, Service};
    use confluence_engine::journal::Journal;
    use confluence_engine::midi::{MidiEvent, MidiHub, WinmmProvider};
    use confluence_engine::plugins::{self, Scanner};
    use confluence_engine::publish::{published_state, Publisher};
    use confluence_engine::rt::disable_power_throttling;
    use confluence_engine::{Engine, EngineConfig, PluginControl, PluginParts};
    use confluence_net::discovery::{Discovery, DnsSd, FakeDiscovery};
    use confluence_net::host::NetHost;
    use confluence_plugin_host::{PluginLink, PluginThread, Source};
    use confluence_provider_asio::AsioDevice;

    #[derive(clap::Parser)]
    // A later flag wins (tests add their own --net-port after the defaults).
    #[command(version, about = "Confluence audio engine", args_override_self = true)]
    pub struct Args {
        /// Pipe name (default: confluence-<USERNAME>).
        #[arg(long)]
        pipe: Option<String>,
        /// Journal file (default: %LOCALAPPDATA%\Confluence\journal.bin).
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Device bindings file (default: %LOCALAPPDATA%\Confluence\devices.json).
        #[arg(long)]
        devices: Option<PathBuf>,
        /// Master clock: `internal`, or `asio:<driver name>` (e.g. `asio:MOTU Gen 5`).
        /// Default: the master saved with the devices, else the internal clock.
        #[arg(long)]
        master: Option<String>,
        /// Engine sample rate in Hz (internal clock; an ASIO master uses its own).
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        /// Engine block size in frames (internal clock; an ASIO master uses its preferred size).
        #[arg(long, default_value_t = 256)]
        block: usize,
        /// Folders searched for CLAP plugins, `;`-separated (default: the
        /// standard CLAP folders and `CLAP_PATH`).
        #[arg(long)]
        clap_path: Option<String>,
        /// Scan mode: print the plugins in this CLAP file as JSON and exit.
        #[arg(long)]
        pub scan: Option<PathBuf>,
        /// With `--scan`: instead, load, start and run this plugin once (the load check).
        #[arg(long, requires = "scan")]
        plugin: Option<String>,
        /// Do not open MIDI devices (tests; MIDI can still be injected over the pipe).
        #[arg(long)]
        no_midi: bool,
        /// Address network audio listens on (default: every interface).
        #[arg(long, default_value = "0.0.0.0")]
        net_bind: std::net::IpAddr,
        /// UDP port for network audio (0: any free port).
        #[arg(long, default_value_t = confluence_net::DEFAULT_PORT)]
        net_port: u16,
        /// This engine's name on the network (default: the computer's name).
        #[arg(long)]
        net_name: Option<String>,
        /// Do not advertise this engine or look for others (tests; streams by address still work).
        #[arg(long)]
        no_net_discovery: bool,
    }

    /// This engine's network identity: a random number kept in `path`.
    fn engine_id(path: &std::path::Path) -> u64 {
        if let Some(id) = std::fs::read_to_string(path).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok()) {
            return id;
        }
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        let id = h.finish();
        if let Err(e) = std::fs::write(path, format!("{id:x}")) {
            eprintln!("confluence-engine: warning: {} could not be written: {e}", path.display());
        }
        id
    }

    /// Starts network audio; on failure (e.g. the port is taken) the engine runs without it.
    fn start_net(args: &Args, devices_file: &std::path::Path) -> Option<NetCtx> {
        let id = engine_id(&devices_file.with_file_name("engine-id"));
        let host = match NetHost::start(std::net::SocketAddr::new(args.net_bind, args.net_port), id) {
            Ok(h) => h,
            Err(e) => {
                eprintln!(
                    "confluence-engine: warning: network audio is off: UDP port {} could not be opened: {e}",
                    args.net_port
                );
                return None;
            }
        };
        let name =
            args.net_name.clone().or_else(|| std::env::var("COMPUTERNAME").ok()).unwrap_or_else(|| "Confluence".into());
        let discovery: Box<dyn Discovery> = if args.no_net_discovery {
            Box::new(FakeDiscovery::default())
        } else {
            match DnsSd::start(&name, host.port(), id) {
                Ok(d) => Box::new(d),
                Err(e) => {
                    eprintln!("confluence-engine: warning: other engines will not be found: {e}");
                    Box::new(FakeDiscovery::default())
                }
            }
        };
        eprintln!("confluence-engine: network audio on UDP port {} as {name}", host.port());
        Some(NetCtx::new(host, discovery))
    }

    /// `--scan`: runs in a throwaway process, so a plugin that crashes takes
    /// only this process down. A clean failure exits with `SCAN_FAILED` and
    /// its reason on stderr.
    pub fn scan(args: &Args) -> std::process::ExitCode {
        use confluence_engine::plugins::{CHECK_PASSED, SCAN_FAILED};
        let Some(file) = args.scan.as_deref() else { return std::process::ExitCode::FAILURE };
        let result = match &args.plugin {
            Some(id) => confluence_plugin_host::check(file, id, args.rate, args.block as u32)
                .map(|()| format!("{CHECK_PASSED} {id}")),
            None => confluence_plugin_host::describe(file)
                .and_then(|list| serde_json::to_string(&list).map_err(|e| e.to_string())),
        };
        match result {
            Ok(out) => {
                println!("{out}");
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::ExitCode::from(SCAN_FAILED as u8)
            }
        }
    }

    struct State {
        engine: Engine,
        journal: Journal,
        devices: DeviceManager,
        publisher: Publisher,
        /// The device list, refreshed without the lock (enumeration is slow).
        device_list: Arc<Mutex<Vec<DeviceInfo>>>,
        /// The master clock's binding, for status.
        master: String,
        /// Owns every plugin instance (CLAP main-thread calls happen there).
        plugin_thread: PluginThread,
        /// Finds CLAP plugins in the background.
        scanner: Scanner,
        /// This program, run as the load-check process.
        exe: PathBuf,
        /// MIDI inputs and feedback outputs (`None` with `--no-midi`).
        midi: Option<MidiHub>,
    }

    /// A plugin as the engine controls it.
    struct Link(PluginLink);

    impl PluginControl for Link {
        fn info(&self) -> PluginInfo {
            self.0.info().clone()
        }
        fn latency(&self) -> u32 {
            self.0.latency()
        }
        fn params(&self) -> Vec<ParamState> {
            self.0.params().to_vec()
        }
        fn set_param(&mut self, id: u32, value: f64) -> Result<(), String> {
            self.0.set_param(id, value)
        }
        fn poll(&mut self) -> bool {
            self.0.poll()
        }
        fn save_state(&mut self) -> Result<Vec<u8>, String> {
            self.0.save_state()
        }
        fn load_state(&mut self, state: &[u8]) -> Result<(), String> {
            self.0.load_state(state)
        }
        fn has_editor(&self) -> bool {
            self.0.has_editor()
        }
        fn editor_open(&self) -> bool {
            self.0.editor_open()
        }
        fn show_editor(&mut self, title: &str) -> Result<(), String> {
            self.0.show_editor(title)
        }
        fn hide_editor(&mut self) {
            self.0.hide_editor()
        }
        fn take_edited(&mut self) -> Vec<(u32, f64)> {
            self.0.take_edited()
        }
    }

    /// Checks plugin `id` of `path` in a separate process, then loads it here.
    fn open_plugin(
        exe: &std::path::Path,
        thread: &PluginThread,
        path: &str,
        id: &str,
        rate: f64,
        block: u32,
        channels: u32,
    ) -> Result<PluginParts, String> {
        plugins::check(exe, std::path::Path::new(path), id, rate, block)?;
        let (link, processor) = thread.load(Source::File(PathBuf::from(path)), id, rate, block, channels)?;
        Ok((Box::new(Link(link)), processor))
    }

    /// A bus's first send column and channel count.
    fn bus_place(engine: &Engine, bus: u32) -> Option<(u32, u32)> {
        engine.slots().into_iter().find(|s| s.id == bus).map(|s| (s.first_output, s.outputs))
    }

    /// `cmd` as the journal stores it: buses named by send column, not by id
    /// (ids change between runs).
    fn journal_form(engine: &Engine, cmd: &Command) -> Command {
        let at = |bus: &BusRef| match engine.resolve_bus(bus).ok().and_then(|b| bus_place(engine, b)) {
            Some((first, _)) => BusRef::At(first),
            None => *bus,
        };
        match cmd {
            Command::UnloadPlugin { bus } => Command::UnloadPlugin { bus: at(bus) },
            Command::SetParam { bus, param, value } => Command::SetParam { bus: at(bus), param: *param, value: *value },
            Command::SetPluginState { bus, state } => Command::SetPluginState { bus: at(bus), state: state.clone() },
            Command::LoadPlugin { bus, path, plugin_id } => {
                Command::LoadPlugin { bus: at(bus), path: path.clone(), plugin_id: plugin_id.clone() }
            }
            // Slot ids change between runs: a colour is kept under its device.
            Command::SetSlotLabel { id, channel, name } => match engine.label_key(*id, *channel) {
                Some(key) => Command::SetLabel { key, name: confluence_api::clean_label(name.as_deref()) },
                None => cmd.clone(),
            },
            Command::SetSlotColor { id, color } => match engine.color_key(*id) {
                Some(key) => Command::SetColor { key, color: *color },
                None => cmd.clone(),
            },
            // The journal keeps what was captured, not the request to capture.
            Command::SaveScene { name, morph_ms } => match engine.scene(name.trim()) {
                Some(scene) => Command::PutScene { scene: scene.clone() },
                None => Command::SaveScene { name: name.clone(), morph_ms: *morph_ms },
            },
            other => other.clone(),
        }
    }

    /// Replays a journaled `LoadPlugin` at start-up. A plugin that cannot be
    /// loaded keeps its bus silent and stays in the journal.
    fn replay_load(
        engine: &mut Engine,
        exe: &std::path::Path,
        thread: &PluginThread,
        bus: &BusRef,
        path: &str,
        id: &str,
    ) {
        let Ok(b) = engine.resolve_bus(bus) else { return };
        let Some((_, channels)) = bus_place(engine, b) else { return };
        let (rate, block) = (engine.config().sample_rate, engine.config().block as u32);
        match open_plugin(exe, thread, path, id, rate, block, channels) {
            Ok(plugin) => {
                let _ = engine.set_plugin(b, Some(plugin));
            }
            Err(why) => {
                eprintln!("confluence-engine: warning: plugin {id} could not be loaded: {why}");
                let info = PluginInfo {
                    path: path.to_string(),
                    id: id.to_string(),
                    name: id.to_string(),
                    vendor: String::new(),
                    version: String::new(),
                };
                let _ = engine.set_failed_plugin(b, info, why);
            }
        }
    }

    /// `LoadPlugin` over the pipe: the check and the load take seconds, so
    /// they run without the lock.
    fn load_plugin(state: &Mutex<State>, bus: &BusRef, path: &str, id: &str) -> Response {
        let (exe, thread, rate, block, at, channels) = {
            let s = lock(state);
            let b = match s.engine.resolve_bus(bus) {
                Ok(b) => b,
                Err(e) => return Response::Error(e.to_string()),
            };
            let Some((at, channels)) = bus_place(&s.engine, b) else {
                return Response::Error(format!("no slot with id {b}"));
            };
            let cfg = s.engine.config();
            (s.exe.clone(), s.plugin_thread.clone(), cfg.sample_rate, cfg.block as u32, at, channels)
        };
        let opened = open_plugin(&exe, &thread, path, id, rate, block, channels);
        let mut s = lock(state);
        let plugin = match opened {
            Ok(p) => p,
            Err(e) => return Response::Error(e),
        };
        // The bus may have gone while the plugin was loading.
        let b = match s.engine.resolve_bus(&BusRef::At(at)) {
            Ok(b) => b,
            Err(e) => {
                thread.reclaim(plugin.1);
                return Response::Error(e.to_string());
            }
        };
        if let Err(e) = s.engine.set_plugin(b, Some(plugin)) {
            return Response::Error(e.to_string());
        }
        let record = Command::LoadPlugin { bus: BusRef::At(at), path: path.to_string(), plugin_id: id.to_string() };
        if let Err(e) = s.journal.append(&record) {
            publish(&mut s);
            return Response::Error(format!("applied but not saved: {e}"));
        }
        Response::Applied { version: publish(&mut s) }
    }

    /// MIDI each control tick: devices that came or went, messages received,
    /// and feedback for gains that changed.
    fn midi_tick(s: &mut State) {
        let Some(mut hub) = s.midi.take() else {
            // Injected messages still produce feedback state; there is nowhere to send it.
            let _ = s.engine.midi_feedback();
            return;
        };
        let opened = hub.tick(Instant::now());
        if hub.input_names() != s.engine.midi_inputs() {
            s.engine.set_midi_inputs(hub.input_names());
        }
        // A device reopened under its old name (replugged) is brought in line too.
        s.engine.midi_reopened(&opened);
        for ev in hub.events() {
            if let Err(e) = midi_in(s, &ev) {
                eprintln!("confluence-engine: warning: a MIDI change was not saved: {e}");
            }
        }
        for (device, bytes) in s.engine.midi_feedback() {
            if !hub.send(&device, &bytes) {
                s.engine.midi_unsent(&device, bytes);
            }
        }
        s.midi = Some(hub);
    }

    /// A MIDI message, from a device or injected: applied, and what it changed journaled.
    fn midi_in(s: &mut State, ev: &MidiEvent) -> std::io::Result<()> {
        for c in s.engine.midi_event(ev) {
            s.journal.append(&c)?;
        }
        Ok(())
    }

    /// Adds what `published_state` does not know about: plugins.
    fn with_plugins(mut st: confluence_api::State, engine: &mut Engine, scanner: &Scanner) -> confluence_api::State {
        (st.plugins, st.bad_plugins) = scanner.list();
        st.bus_plugins = engine.bus_plugins();
        st.notices.extend(engine.plugin_notices());
        st.midi_inputs = engine.midi_inputs().to_vec();
        st.midi_bindings = engine.midi_bindings().to_vec();
        st.midi_learning = engine.midi_learning();
        st.scripts = engine.script_infos();
        st.scenes = engine.scene_infos();
        st.current_scene = engine.current_scene().map(String::from);
        st.morphing = engine.morphing();
        st
    }

    /// How often state is diffed and telemetry sent, in control-loop ticks of 10 ms.
    const PUBLISH_TICKS: u64 = 10;
    /// The control loop's tick.
    const TICK: Duration = Duration::from_millis(10);
    /// Between meter frames: about 60 a second.
    const METER_PERIOD: Duration = Duration::from_millis(16);
    /// The journal is rewritten as the current state once it grows past this.
    const JOURNAL_COMPACT_BYTES: u64 = 4 << 20;
    /// How often the device list is refreshed.
    const DEVICE_SCAN: Duration = Duration::from_secs(2);

    /// Slot health, with each device's own health added.
    fn health(s: &mut State) -> Vec<SlotHealth> {
        let mut resp = s.engine.handle(&Command::Health);
        s.devices.annotate(&mut resp);
        match resp {
            Response::Health { slots, .. } => slots,
            _ => Vec::new(),
        }
    }

    fn status(s: &State, health: &[SlotHealth]) -> EngineStatus {
        let cfg = s.engine.config();
        EngineStatus {
            master: s.master.clone(),
            sample_rate: cfg.sample_rate,
            block: cfg.block as u32,
            blocks: s.engine.blocks(),
            dsp_load: s.engine.dsp_load(),
            xruns: health.iter().map(|h| h.underruns + h.overruns).sum(),
        }
    }

    /// Diffs the current state against the last published one; returns the version.
    fn publish(s: &mut State) -> u64 {
        let health = health(s);
        let status = status(s, &health);
        let mut list = s.device_list.lock().map(|l| l.clone()).unwrap_or_default();
        list.extend(s.devices.net_devices());
        let now = published_state(&mut s.engine, &s.devices, &list, status);
        let now = with_plugins(now, &mut s.engine, &s.scanner);
        s.publisher.publish(now)
    }

    /// The Control API: commands, and subscriptions to the published state.
    /// Connections can stay open indefinitely (a GUI keeps one), so it holds the
    /// state weakly: only a command in progress keeps it alive at shutdown.
    struct Control {
        state: Weak<Mutex<State>>,
        shutdown: Arc<AtomicBool>,
    }

    impl Service for Control {
        fn handle(&self, cmd: &Command) -> Response {
            if *cmd == Command::Shutdown {
                self.shutdown.store(true, Ordering::SeqCst);
                return Response::Ok;
            }
            let Some(state) = self.state.upgrade() else {
                return Response::Error("the engine is shutting down".into());
            };
            let state = &state;
            // Device enumeration and driver initialisation can be slow (a bad
            // driver can take seconds): do them without the lock, so the
            // engine keeps ticking and other clients keep being answered.
            match cmd {
                Command::ListPlugins => return Response::Plugins(lock(state).scanner.list().0),
                Command::InjectMidi { device, bytes } if (1..=3).contains(&bytes.len()) => {
                    let mut s = lock(state);
                    let ev = MidiEvent { device: device.clone(), bytes: bytes.clone() };
                    return match midi_in(&mut s, &ev) {
                        Ok(()) => Response::Applied { version: publish(&mut s) },
                        Err(e) => Response::Error(format!("applied but not saved: {e}")),
                    };
                }
                Command::LoadPlugin { bus, path, plugin_id } => return load_plugin(state, bus, path, plugin_id),
                Command::ListDevices => {
                    return match DeviceManager::list_devices() {
                        Ok(mut d) => {
                            let s = lock(state);
                            if let Ok(mut l) = s.device_list.lock() {
                                l.clone_from(&d);
                            }
                            d.extend(s.devices.net_devices());
                            Response::Devices(d)
                        }
                        Err(e) => Response::Error(e),
                    };
                }
                Command::Status => {
                    let mut s = lock(state);
                    let h = health(&mut s);
                    return Response::Status(status(&s, &h));
                }
                Command::AddDevice {
                    kind: kind @ (confluence_api::DeviceKind::Vasio | confluence_api::DeviceKind::Vaio),
                    name,
                } => {
                    let mut s = lock(state);
                    let State { engine, devices, journal, .. } = &mut *s;
                    return match devices.add(engine, *kind, name) {
                        Ok(ids) => {
                            // A swap or reshape moves and drops routes: save them as they are now.
                            if let Err(e) = journal.compact(&state_commands(engine)) {
                                publish(&mut s);
                                return Response::Error(format!("applied but not saved: {e}"));
                            }
                            let version = publish(&mut s);
                            Response::Added { ids, version }
                        }
                        Err(e) => Response::Error(e),
                    };
                }
                Command::AddDevice { .. } | Command::FillPosition { .. } => {
                    let begun = {
                        let mut s = lock(state);
                        match cmd {
                            Command::FillPosition { pos, kind, name } if !pos.group.is_virtual() => {
                                s.devices.begin_fill(Some(*pos), *kind, name)
                            }
                            Command::FillPosition { pos, .. } => {
                                Err(format!("{} is a virtual position: turn it on instead", pos.label()))
                            }
                            Command::AddDevice { kind, name } => s.devices.begin_add(*kind, name),
                            _ => unreachable!("matched above"),
                        }
                    };
                    let pending = match begun {
                        Ok(p) => p,
                        Err(e) => return Response::Error(e),
                    };
                    let loaded = pending.load();
                    let attached = {
                        let mut s = lock(state);
                        let State { engine, devices, .. } = &mut *s;
                        match devices.attach_add(engine, loaded) {
                            Ok(a) => a,
                            Err(e) => return Response::Error(e),
                        }
                    };
                    // Starting an ASIO driver can be slow too: not under the lock.
                    let started = attached.start();
                    let mut s = lock(state);
                    let State { engine, devices, journal, .. } = &mut *s;
                    let added = devices.commit_add(engine, started);
                    return match added {
                        Ok(ids) => {
                            // A swap or reshape moves and drops routes: save them as they are now.
                            if let Err(e) = journal.compact(&state_commands(engine)) {
                                publish(&mut s);
                                return Response::Error(format!("applied but not saved: {e}"));
                            }
                            let version = publish(&mut s);
                            Response::Added { ids, version }
                        }
                        Err(e) => Response::Error(e),
                    };
                }
                _ => {}
            }
            let mut s = lock(state);
            let State { engine, devices, journal, .. } = &mut *s;
            let mut resp = match devices.handle(engine, cmd) {
                Some(resp) => resp,
                None => engine.handle(cmd),
            };
            devices.annotate(&mut resp);
            if let (Command::AddBus { .. }, Response::SlotsAdded(ids)) = (cmd, &resp) {
                let ids = ids.clone();
                // The engine placed the bus: save its placement, not the request,
                // so it comes back on the same channels after a restart.
                if let Err(e) = journal.compact(&state_commands(engine)) {
                    publish(&mut s);
                    return Response::Error(format!("applied but not saved: {e}"));
                }
                return Response::Added { ids, version: publish(&mut s) };
            }
            if resp == Response::Ok {
                // Removing a slot also removes its routes: rewrite the journal
                // so they do not come back, on other devices, after a restart.
                let saved = match cmd {
                    Command::RemoveSlot { .. }
                    | Command::ClearPosition { .. }
                    | Command::SetVirtual { .. }
                    | Command::SetMaster { .. } => journal.compact(&state_commands(engine)),
                    _ if cmd.is_mutation() => journal.append(&journal_form(engine, cmd)),
                    _ => Ok(()),
                };
                if let Err(e) = saved {
                    publish(&mut s);
                    return Response::Error(format!("applied but not saved: {e}"));
                }
                if cmd.is_mutation()
                    || matches!(
                        cmd,
                        Command::RemoveSlot { .. }
                            | Command::ClearPosition { .. }
                            | Command::SetVirtual { .. }
                            | Command::SetMaster { .. }
                    )
                {
                    // Published before the reply, so its version includes this change.
                    return Response::Applied { version: publish(&mut s) };
                }
                // Not saved, but shown: published at once so the window follows.
                if matches!(
                    cmd,
                    Command::LearnMidi { .. }
                        | Command::CancelMidiLearn
                        | Command::ShowEditor { .. }
                        | Command::HideEditor { .. }
                ) {
                    publish(&mut s);
                }
            }
            resp
        }

        fn subscribe(&self, meters: bool) -> Option<(confluence_api::State, Receiver<Event>)> {
            self.state.upgrade().map(|state| lock(&state).publisher.subscribe(meters))
        }
    }

    /// Whatever drives the engine; dropping it stops the audio.
    enum Master {
        Internal(InternalClock),
        Asio(Box<AsioDevice>),
    }

    fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
        state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn data_dir() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("Confluence")
    }

    /// The minimal commands that recreate the engine's insert buses (on their
    /// channels) and then its matrix.
    fn state_commands(engine: &mut Engine) -> Vec<Command> {
        let mut buses: Vec<_> = engine.slots().into_iter().filter(|s| s.is_bus()).collect();
        buses.sort_by_key(|s| s.first_output);
        let mut out: Vec<Command> = buses
            .into_iter()
            .map(|s| Command::AddBus {
                name: s.name,
                channels: s.inputs,
                first_input: Some(s.first_input),
                first_output: Some(s.first_output),
            })
            .collect();
        out.extend(engine.plugin_commands());
        out.extend(engine.scene_commands());
        out.extend(engine.midi_commands());
        out.extend(engine.script_commands());
        out.extend(engine.color_commands());
        out.extend(engine.label_commands());
        // Mid-morph, the routes are saved where the morph is taking them.
        out.extend(engine.settled_points().into_iter().map(|p| Command::SetPoint {
            input: p.input,
            output: p.output,
            gain_db: p.gain_db,
            mute: p.mute,
            invert: p.invert,
        }));
        out
    }

    pub fn run(args: Args) -> Result<(), Box<dyn Error>> {
        if let Err(e) = disable_power_throttling() {
            eprintln!("confluence-engine: warning: could not disable power throttling: {e}");
        }
        let pipe = args.pipe.clone().unwrap_or_else(default_pipe_name);
        // Claim the pipe name before touching any state: a second engine must
        // fail here, not after it has replayed and compacted the journal.
        let listener = PipeServer::bind(&pipe)?;
        let journal_path = args.journal.clone().unwrap_or_else(|| data_dir().join("journal.bin"));
        let (mut journal, replay) = Journal::open(&journal_path)?;

        // The saved devices name the master, if no --master does.
        let devices_file = args.devices.clone().unwrap_or_else(|| data_dir().join("devices.json"));
        let (mut devices, mut warnings) = DeviceManager::open_file(devices_file.clone());
        let master_arg = args
            .master
            .clone()
            .or_else(|| devices.saved_master_name().map(|n| format!("asio:{n}")))
            .unwrap_or_else(|| "internal".into());
        // An ASIO master fixes the engine's rate and block: open it first.
        let asio_master = match master_arg.strip_prefix("asio:") {
            Some(name) => Some((name.to_string(), AsioDevice::open_installed(name)?)),
            None if master_arg == "internal" => None,
            None => return Err(format!("unknown master '{master_arg}': use internal or asio:<name>").into()),
        };
        let (rate, block) = match &asio_master {
            Some((_, dev)) => (dev.info().sample_rate, dev.info().preferred_block.max(1) as usize),
            None => (args.rate, args.block),
        };
        let (mut engine, audio) = Engine::new(EngineConfig::new(rate, block));
        let plugin_thread = PluginThread::start()?;
        let exe = std::env::current_exe()?;
        let replay = confluence_engine::journal::collapse_params(replay);
        for cmd in &replay {
            match cmd {
                Command::LoadPlugin { bus, path, plugin_id } => {
                    replay_load(&mut engine, &exe, &plugin_thread, bus, path, plugin_id)
                }
                // At start-up a recall lands at once: no morph before audio runs.
                Command::RecallScene { name } => {
                    let _ = engine.recall_scene_at(name, Instant::now(), true);
                }
                _ => {
                    engine.handle(cmd);
                }
            }
        }
        let plugin_dirs = match &args.clap_path {
            Some(dirs) => std::env::split_paths(dirs).collect(),
            None => plugins::default_dirs(),
        };
        let scanner = Scanner::start(exe.clone(), plugin_dirs);
        journal.compact(&state_commands(&mut engine))?;
        // A version-1 device setup was migrated: colours move to positions.
        let rekeys = devices.take_color_rekeys();
        if !rekeys.is_empty() {
            confluence_engine::migrate::backup(&journal_path)?;
            engine.rekey_colors(&rekeys);
            journal.compact(&state_commands(&mut engine))?;
        }

        if let Some(net) = start_net(&args, &devices_file) {
            devices = devices.with_net(net);
        }
        let master = match asio_master {
            Some((name, mut dev)) => {
                devices.claim_master(&name);
                // A master with a saved placement goes first so it gets its old
                // channels; a new master takes whatever the restored devices leave.
                let placement = devices.saved_master(&name);
                if placement.is_none() {
                    warnings.extend(devices.restore(&mut engine));
                }
                let (master_id, _, ch) = start_asio_master(&mut dev, &mut engine, audio, &name, placement)?;
                devices.watch_master(master_id, dev.health());
                if placement.is_some() {
                    warnings.extend(devices.restore(&mut engine));
                }
                devices.set_master(&name, ch)?;
                eprintln!("confluence-engine: master asio:{name} at {rate} Hz, {block} frames");
                Master::Asio(Box::new(dev))
            }
            None => {
                warnings.extend(devices.restore(&mut engine));
                Master::Internal(InternalClock::start(audio, rate)?)
            }
        };
        for w in warnings {
            eprintln!("confluence-engine: warning: {w}");
        }
        // The device list starts empty and is filled by the scan thread below,
        // so a slow enumeration never delays the engine's start.
        let device_list = Arc::new(Mutex::new(Vec::new()));
        let first = EngineStatus {
            master: master_arg.clone(),
            sample_rate: rate,
            block: block as u32,
            blocks: 0,
            dsp_load: 0.0,
            xruns: 0,
        };
        let first = with_plugins(published_state(&mut engine, &devices, &[], first), &mut engine, &scanner);
        let publisher = Publisher::new(first);
        let state = Arc::new(Mutex::new(State {
            engine,
            journal,
            devices,
            publisher,
            device_list: device_list.clone(),
            master: master_arg.clone(),
            plugin_thread,
            scanner,
            exe,
            midi: (!args.no_midi).then(|| MidiHub::new(Box::new(WinmmProvider))),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        {
            let stop = shutdown.clone();
            std::thread::Builder::new().name("confluence-device-scan".into()).spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(d) = DeviceManager::list_devices() {
                        if let Ok(mut l) = device_list.lock() {
                            *l = d;
                        }
                    }
                    let next = Instant::now() + DEVICE_SCAN;
                    while !stop.load(Ordering::SeqCst) && Instant::now() < next {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            })?;
        }
        let handler = Arc::new(Control { state: Arc::downgrade(&state), shutdown: shutdown.clone() });
        let server = listener.serve(handler)?;
        eprintln!("confluence-engine: listening on {}", pipe_path(&pipe));

        let mut ticks = 0u64;
        let (mut next_tick, mut next_meter) = (Instant::now() + TICK, Instant::now() + METER_PERIOD);
        // The next deadline after `due`, `period` on (or from `now` if it fell behind).
        let advance = |due: Instant, now: Instant, period: Duration| {
            if now > due + period {
                now + period
            } else {
                due + period
            }
        };
        while !shutdown.load(Ordering::SeqCst) {
            let wake = next_tick.min(next_meter);
            let now = Instant::now();
            if wake > now {
                std::thread::sleep(wake - now);
            }
            let now = Instant::now();
            if now >= next_meter {
                next_meter = advance(next_meter, now, METER_PERIOD);
                // Meters about 60 times a second, measured out only for those who asked.
                let mut s = lock(&state);
                if s.publisher.has_meter_subscribers() {
                    let frame = s.engine.meter_frame();
                    s.publisher.meters(frame);
                }
            }
            if now < next_tick {
                continue;
            }
            next_tick = advance(next_tick, now, TICK);
            let mut s = lock(&state);
            s.engine.tick();
            for p in s.engine.take_returned_processors() {
                s.plugin_thread.reclaim(p);
            }
            midi_tick(&mut s);
            ticks += 1;
            if ticks.is_multiple_of(PUBLISH_TICKS) {
                // Network streams whose engine was not found come back once it is.
                let State { devices, engine, .. } = &mut *s;
                devices.retry_offline_net(engine);
                // Catches changes no command made: devices lost or back, a DAW attaching.
                publish(&mut s);
                // Values changed in plugin editors are saved like any other change.
                let edited = s.engine.take_edited_values(Instant::now());
                for c in &edited {
                    if let Err(e) = s.journal.append(c) {
                        eprintln!("confluence-engine: warning: a plugin edit was not saved: {e}");
                    }
                }
                // A long session of edits: rewrite the journal as the current state.
                if s.journal.size() > JOURNAL_COMPACT_BYTES {
                    let State { engine, journal, .. } = &mut *s;
                    if let Err(e) = journal.compact(&state_commands(engine)) {
                        eprintln!("confluence-engine: warning: the journal could not be compacted: {e}");
                    }
                }
                let h = health(&mut s);
                let st = status(&s, &h);
                s.publisher.telemetry(st, h);
            }
        }
        server.stop();
        {
            // Plugins keep settings that no command changed (e.g. a loaded file): save them.
            let mut s = lock(&state);
            let State { engine, journal, .. } = &mut *s;
            if let Err(e) = journal.compact(&state_commands(engine)) {
                eprintln!("confluence-engine: warning: the final save failed: {e}");
            }
        }
        // Ends every subscription, so their connection threads let go of the state.
        lock(&state).publisher.close();
        // Soft devices first (their bridges feed the engine), then the master.
        // A command may still be loading or starting a device without the
        // lock: give it a moment to finish. The master is stopped either way.
        let give_up = Instant::now() + Duration::from_secs(5);
        let mut state = state;
        let state = loop {
            match Arc::try_unwrap(state) {
                Ok(m) => break Some(m.into_inner().unwrap_or_else(|p| p.into_inner())),
                Err(shared) if Instant::now() < give_up => {
                    state = shared;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    eprintln!("confluence-engine: a device was still being added; stopping anyway");
                    break None;
                }
            }
        };
        if let Some(state) = state {
            drop(state.devices);
        }
        match master {
            Master::Internal(clock) => {
                clock.stop();
            }
            Master::Asio(mut dev) => dev.stop(),
        }
        Ok(())
    }
}
