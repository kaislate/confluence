//! Starting the engine from the window: `confluence-engine.exe` from the GUI's
//! own folder, detached, so it keeps running when the window closes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const ENGINE_EXE: &str = "confluence-engine.exe";
/// Windows `ERROR_ACCESS_DENIED`: what starting outside a job reports when
/// the job does not allow it.
const ERROR_ACCESS_DENIED: i32 = 5;
/// The Start button ignores presses for this long after one.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// An engine that exits within this long of starting is reported.
const EARLY_EXIT: Duration = Duration::from_secs(3);

/// `confluence-engine.exe` in the same folder as `gui_exe`.
pub fn engine_next_to(gui_exe: &Path) -> PathBuf {
    gui_exe.with_file_name(ENGINE_EXE)
}

/// Engine arguments: the pipe, only when the GUI was given a non-default one.
pub fn engine_args(pipe: &str, default_pipe: &str) -> Vec<String> {
    if pipe == default_pipe {
        Vec::new()
    } else {
        vec!["--pipe".into(), pipe.into()]
    }
}

pub struct Launcher {
    exe: PathBuf,
    args: Vec<String>,
    pressed: Option<Instant>,
    started: Option<(Child, Instant)>,
}

impl Launcher {
    pub fn new(exe: PathBuf, args: Vec<String>) -> Self {
        Launcher { exe, args, pressed: None, started: None }
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// False for a few seconds after a press.
    pub fn ready(&self, now: Instant) -> bool {
        self.pressed.is_none_or(|t| now.saturating_duration_since(t) >= DEBOUNCE)
    }

    /// Starts the engine detached, with no console window and no handles to
    /// this process, so it outlives the GUI.
    pub fn start(&mut self, now: Instant) -> Result<(), String> {
        self.pressed = Some(now);
        if !self.exe.is_file() {
            return Err(format!("cannot start the engine: {} not found", self.exe.display()));
        }
        // Outside the window's job if it is in one (some terminals and IDEs put
        // what they start in a kill-on-close job): closing the window must not
        // stop the engine. A job that forbids breaking away refuses with access
        // denied; then start inside it rather than not at all.
        let child = match self.spawn(true) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => self.spawn(false),
            other => other,
        }
        .map_err(|e| format!("cannot start the engine: {e}"))?;
        self.started = Some((child, now));
        Ok(())
    }

    fn spawn(&self, break_away: bool) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(&self.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
            if break_away {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            cmd.creation_flags(flags);
        }
        #[cfg(not(windows))]
        let _ = break_away;
        cmd.spawn()
    }

    /// The id of the process started last, while it is still being watched.
    pub fn pid(&self) -> Option<u32> {
        self.started.as_ref().map(|(child, _)| child.id())
    }

    /// A message if the engine just started has already exited; call each frame.
    pub fn poll(&mut self, now: Instant) -> Option<String> {
        let (child, at) = self.started.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                self.started = None;
                Some(format!("the engine stopped right after starting ({status})"))
            }
            Ok(None) if now.saturating_duration_since(*at) >= EARLY_EXIT => {
                // Running: stop watching. Dropping a `Child` does not stop it.
                self.started = None;
                None
            }
            Ok(None) => None,
            Err(e) => {
                self.started = None;
                Some(format!("cannot watch the engine: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_is_looked_for_next_to_the_gui() {
        assert_eq!(
            engine_next_to(Path::new(r"C:\Apps\Confluence\confluence.exe")),
            PathBuf::from(r"C:\Apps\Confluence\confluence-engine.exe")
        );
    }

    #[test]
    fn the_pipe_is_passed_only_when_it_is_not_the_default() {
        assert!(engine_args("confluence-me", "confluence-me").is_empty());
        assert_eq!(engine_args("lab", "confluence-me"), vec!["--pipe".to_string(), "lab".to_string()]);
    }

    #[test]
    fn a_missing_engine_is_a_clear_error() {
        let mut l = Launcher::new(PathBuf::from(r"C:\nowhere\confluence-engine.exe"), Vec::new());
        let err = l.start(Instant::now()).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn presses_are_ignored_for_five_seconds() {
        let mut l = Launcher::new(PathBuf::from(r"C:\nowhere\confluence-engine.exe"), Vec::new());
        let t0 = Instant::now();
        assert!(l.ready(t0));
        let _ = l.start(t0);
        assert!(!l.ready(t0 + Duration::from_secs(1)));
        assert!(l.ready(t0 + Duration::from_secs(5)));
    }

    #[test]
    fn an_engine_that_exits_at_once_is_reported() {
        let cmd = PathBuf::from(r"C:\Windows\System32\cmd.exe");
        let mut l = Launcher::new(cmd, vec!["/c".into(), "exit".into(), "3".into()]);
        l.start(Instant::now()).unwrap();
        let start = Instant::now();
        let msg = loop {
            if let Some(m) = l.poll(Instant::now()) {
                break m;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "no report");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(msg.contains('3'), "{msg}");
    }
}
