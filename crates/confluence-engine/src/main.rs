//! `confluence-engine`: the background engine process. Runs the internal clock
//! as master, serves the Control API on a named pipe and journals changes.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    match app::run(app::Args::parse()) {
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
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use confluence_api::{Command, Response};
    use confluence_engine::clock::InternalClock;
    use confluence_engine::devices::{start_asio_master, DeviceManager};
    use confluence_engine::ipc::{default_pipe_name, pipe_path, service_fn, Handler, PipeServer};
    use confluence_engine::journal::Journal;
    use confluence_engine::rt::disable_power_throttling;
    use confluence_engine::{Engine, EngineConfig};
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
    }

    struct State {
        engine: Engine,
        journal: Journal,
        devices: DeviceManager,
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

    /// The minimal commands that recreate the engine's current matrix.
    fn state_commands(engine: &mut Engine) -> Vec<Command> {
        match engine.handle(&Command::ListPoints) {
            Response::Points(points) => points
                .into_iter()
                .map(|p| Command::SetPoint {
                    input: p.input,
                    output: p.output,
                    gain_db: p.gain_db,
                    mute: p.mute,
                    invert: p.invert,
                })
                .collect(),
            _ => Vec::new(),
        }
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
        for cmd in &replay {
            engine.handle(cmd);
        }
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
        let state = Arc::new(Mutex::new(State { engine, journal, devices }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler: Handler = {
            let (state, shutdown) = (state.clone(), shutdown.clone());
            service_fn(move |cmd: &Command| {
                if *cmd == Command::Shutdown {
                    shutdown.store(true, Ordering::SeqCst);
                    return Response::Ok;
                }
                // Device enumeration and driver initialisation can be slow (a bad
                // driver can take seconds): do them without the lock, so the
                // engine keeps ticking and other clients keep being answered.
                match cmd {
                    Command::ListDevices => {
                        return match DeviceManager::list_devices() {
                            Ok(d) => Response::Devices(d),
                            Err(e) => Response::Error(e),
                        };
                    }
                    Command::AddDevice { kind, name } => {
                        let pending = match lock(&state).devices.begin_add(*kind, name) {
                            Ok(p) => p,
                            Err(e) => return Response::Error(e),
                        };
                        let loaded = pending.load();
                        let attached = {
                            let mut s = lock(&state);
                            let State { engine, devices, .. } = &mut *s;
                            match devices.attach_add(engine, loaded) {
                                Ok(a) => a,
                                Err(e) => return Response::Error(e),
                            }
                        };
                        // Starting an ASIO driver can be slow too: not under the lock.
                        let started = attached.start();
                        let mut s = lock(&state);
                        let State { engine, devices, .. } = &mut *s;
                        return match devices.commit_add(engine, started) {
                            Ok(ids) => Response::SlotsAdded(ids),
                            Err(e) => Response::Error(e),
                        };
                    }
                    _ => {}
                }
                let mut s = lock(&state);
                let State { engine, devices, journal } = &mut *s;
                let mut resp = match devices.handle(engine, cmd) {
                    Some(resp) => resp,
                    None => engine.handle(cmd),
                };
                devices.annotate(&mut resp);
                if resp == Response::Ok {
                    // Removing a slot also removes its routes: rewrite the journal
                    // so they do not come back, on other devices, after a restart.
                    let saved = match cmd {
                        Command::RemoveSlot { .. } => journal.compact(&state_commands(engine)),
                        _ if cmd.is_mutation() => journal.append(cmd),
                        _ => Ok(()),
                    };
                    if let Err(e) = saved {
                        return Response::Error(format!("applied but not saved: {e}"));
                    }
                }
                resp
            })
        };
        let server = listener.serve(handler)?;
        eprintln!("confluence-engine: listening on {}", pipe_path(&pipe));

        while !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
            lock(&state).engine.tick();
        }
        server.stop();
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
