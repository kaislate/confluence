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
    use std::time::Duration;

    use confluence_api::{Command, Response};
    use confluence_engine::clock::InternalClock;
    use confluence_engine::ipc::{default_pipe_name, pipe_path, Handler, PipeServer};
    use confluence_engine::journal::Journal;
    use confluence_engine::rt::disable_power_throttling;
    use confluence_engine::{Engine, EngineConfig};

    #[derive(clap::Parser)]
    #[command(version, about = "Confluence audio engine")]
    pub struct Args {
        /// Pipe name (default: confluence-<USERNAME>).
        #[arg(long)]
        pipe: Option<String>,
        /// Journal file (default: %LOCALAPPDATA%\Confluence\journal.bin).
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Engine sample rate in Hz.
        #[arg(long, default_value_t = 48_000.0)]
        rate: f64,
        /// Engine block size in frames.
        #[arg(long, default_value_t = 256)]
        block: usize,
    }

    struct State {
        engine: Engine,
        journal: Journal,
    }

    fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
        state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn default_journal() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("Confluence").join("journal.bin")
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
        let (mut journal, replay) = Journal::open(&args.journal.unwrap_or_else(default_journal))?;
        let (mut engine, audio) = Engine::new(EngineConfig::new(args.rate, args.block));
        for cmd in &replay {
            engine.handle(cmd);
        }
        journal.compact(&state_commands(&mut engine))?;

        let clock = InternalClock::start(audio, args.rate)?;
        let state = Arc::new(Mutex::new(State { engine, journal }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler: Handler = {
            let (state, shutdown) = (state.clone(), shutdown.clone());
            Arc::new(move |cmd: &Command| {
                if *cmd == Command::Shutdown {
                    shutdown.store(true, Ordering::SeqCst);
                    return Response::Ok;
                }
                let mut s = lock(&state);
                let resp = s.engine.handle(cmd);
                if resp == Response::Ok && cmd.is_mutation() {
                    if let Err(e) = s.journal.append(cmd) {
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
        clock.stop();
        Ok(())
    }
}
