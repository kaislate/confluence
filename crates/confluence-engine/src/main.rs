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
    use confluence_engine::devices::{start_asio_master, DeviceManager};
    use confluence_engine::ipc::{default_pipe_name, pipe_path, PipeServer, Service};
    use confluence_engine::journal::Journal;
    use confluence_engine::plugins::{self, Scanner};
    use confluence_engine::publish::{published_state, Publisher};
    use confluence_engine::rt::disable_power_throttling;
    use confluence_engine::{Engine, EngineConfig, PluginControl, PluginParts};
    use confluence_plugin_host::{PluginLink, PluginThread, Source};
    use confluence_provider_asio::AsioDevice;

    #[derive(clap::Parser)]
    #[command(version, about = "Confluence audio engine")]
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
        #[arg(long, default_value = "internal")]
        master: String,
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

    /// Adds what `published_state` does not know about: plugins.
    fn with_plugins(mut st: confluence_api::State, engine: &mut Engine, scanner: &Scanner) -> confluence_api::State {
        (st.plugins, st.bad_plugins) = scanner.list();
        st.bus_plugins = engine.bus_plugins();
        st.notices.extend(engine.plugin_notices());
        st
    }

    /// How often state is diffed and telemetry sent, in control-loop ticks of 10 ms.
    const PUBLISH_TICKS: u64 = 10;
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
        let list = s.device_list.lock().map(|l| l.clone()).unwrap_or_default();
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
                Command::LoadPlugin { bus, path, plugin_id } => return load_plugin(state, bus, path, plugin_id),
                Command::ListDevices => {
                    return match DeviceManager::list_devices() {
                        Ok(d) => {
                            if let Ok(mut l) = lock(state).device_list.lock() {
                                l.clone_from(&d);
                            }
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
                Command::AddDevice { kind, name } => {
                    let pending = match lock(state).devices.begin_add(*kind, name) {
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
                    let State { engine, devices, .. } = &mut *s;
                    let added = devices.commit_add(engine, started);
                    return match added {
                        Ok(ids) => {
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
                    Command::RemoveSlot { .. } => journal.compact(&state_commands(engine)),
                    _ if cmd.is_mutation() => journal.append(&journal_form(engine, cmd)),
                    _ => Ok(()),
                };
                if let Err(e) = saved {
                    publish(&mut s);
                    return Response::Error(format!("applied but not saved: {e}"));
                }
                if cmd.is_mutation() || matches!(cmd, Command::RemoveSlot { .. }) {
                    // Published before the reply, so its version includes this change.
                    return Response::Applied { version: publish(&mut s) };
                }
            }
            resp
        }

        fn subscribe(&self) -> Option<(confluence_api::State, Receiver<Event>)> {
            self.state.upgrade().map(|state| lock(&state).publisher.subscribe())
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
        if let Response::Points(points) = engine.handle(&Command::ListPoints) {
            out.extend(points.into_iter().map(|p| Command::SetPoint {
                input: p.input,
                output: p.output,
                gain_db: p.gain_db,
                mute: p.mute,
                invert: p.invert,
            }));
        }
        out
    }

    pub fn run(args: Args) -> Result<(), Box<dyn Error>> {
        if let Err(e) = disable_power_throttling() {
            eprintln!("confluence-engine: warning: could not disable power throttling: {e}");
        }
        let pipe = args.pipe.unwrap_or_else(default_pipe_name);
        // Claim the pipe name before touching any state: a second engine must
        // fail here, not after it has replayed and compacted the journal.
        let listener = PipeServer::bind(&pipe)?;
        let (mut journal, replay) = Journal::open(&args.journal.unwrap_or_else(|| data_dir().join("journal.bin")))?;

        // An ASIO master fixes the engine's rate and block: open it first.
        let asio_master = match args.master.strip_prefix("asio:") {
            Some(name) => Some((name.to_string(), AsioDevice::open_installed(name)?)),
            None if args.master == "internal" => None,
            None => return Err(format!("unknown master '{}': use internal or asio:<name>", args.master).into()),
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

        let (mut devices, mut warnings) =
            DeviceManager::open_file(args.devices.unwrap_or_else(|| data_dir().join("devices.json")));
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
            master: args.master.clone(),
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
            master: args.master.clone(),
            plugin_thread,
            scanner,
            exe,
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
        while !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
            let mut s = lock(&state);
            s.engine.tick();
            for p in s.engine.take_returned_processors() {
                s.plugin_thread.reclaim(p);
            }
            ticks += 1;
            if ticks.is_multiple_of(PUBLISH_TICKS) {
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
