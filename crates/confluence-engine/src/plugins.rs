//! Finding CLAP plugins, and checking a plugin in a separate process before the
//! engine loads it: a plugin that crashes while loading takes down only that
//! process.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use confluence_api::PluginInfo;

/// How long a scan or load-check process may take.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the plugin folders are looked at again.
const RESCAN: Duration = Duration::from_secs(30);
/// Exit code of a scan process that failed cleanly (its reason is on stderr).
pub const SCAN_FAILED: i32 = 2;
/// What a load check prints when the plugin passed, followed by its id. A
/// plugin that ends the process with code 0 never gets to print it.
pub const CHECK_PASSED: &str = "confluence-check-passed";
/// How long to wait for a finished scan's output: a helper process the plugin
/// started may keep the pipes open.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// Windows `CREATE_NO_WINDOW`: a scan started by an engine without a console
/// (as the window starts it) would otherwise open a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The standard CLAP folders on Windows, then each folder in `CLAP_PATH`.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(common) = std::env::var_os("COMMONPROGRAMFILES") {
        dirs.push(PathBuf::from(common).join("CLAP"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Programs").join("Common").join("CLAP"));
    }
    if let Some(extra) = std::env::var_os("CLAP_PATH") {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs
}

/// Every `*.clap` file under `dirs`, recursively.
fn clap_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = dirs.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("clap")) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// A file's size and modified time: when either changes it is scanned again.
type Stamp = (u64, Option<SystemTime>);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()))
}

/// Runs `exe` with `args` (a scan process) and returns its stdout, or why it failed.
fn run_scan(exe: &Path, args: &[&std::ffi::OsStr], file: &Path) -> Result<String, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start the plugin check: {e}"))?;
    #[cfg(windows)]
    reap_with_engine(&child);
    // Readers report through channels, so a pipe held open by a helper
    // process the plugin started cannot keep us waiting.
    let read = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            let _ = tx.send(s);
        });
        rx
    };
    let out = child.stdout.take().map(|o| read(Box::new(o)));
    let err = child.stderr.take().map(|e| read(Box::new(e)));
    let deadline = Instant::now() + SCAN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not finish loading in 10 s", file.display()));
            }
        }
    };
    let collect = |rx: Option<std::sync::mpsc::Receiver<String>>| {
        rx.and_then(|rx| rx.recv_timeout(OUTPUT_GRACE).ok()).unwrap_or_default()
    };
    let (stdout, stderr) = (collect(out), collect(err));
    match status.code() {
        Some(0) => Ok(stdout),
        Some(SCAN_FAILED) => Err(stderr.trim().to_string()),
        _ => Err(format!("{} crashed while loading; it was not loaded", file.display())),
    }
}

/// Puts a scan process in a job that is closed, killing it, when the engine
/// exits for any reason: a scan must not outlive the engine that started it.
#[cfg(windows)]
fn reap_with_engine(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    /// The job's handle, as an integer (handles are not `Sync`). Never closed:
    /// it closes when the engine process ends, which kills what is in it.
    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    let job = JOB.get_or_init(|| {
        // SAFETY: an unnamed job object, configured before use.
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(job.0 as isize)
        }
    });
    if let Some(job) = job {
        // SAFETY: both handles are valid; failure leaves the child unmanaged.
        let _ = unsafe { AssignProcessToJobObject(HANDLE(*job as *mut _), HANDLE(child.as_raw_handle())) };
    }
}

/// Lists the plugins in `file`, in a separate process.
pub fn describe(exe: &Path, file: &Path) -> Result<Vec<PluginInfo>, String> {
    let out = run_scan(exe, &["--scan".as_ref(), file.as_os_str()], file)?;
    serde_json::from_str(out.trim()).map_err(|e| format!("{}: unreadable scan result: {e}", file.display()))
}

type CheckKey = (PathBuf, Stamp, String, u64, u32);

fn checked() -> &'static Mutex<HashMap<CheckKey, Result<(), String>>> {
    static CHECKED: OnceLock<Mutex<HashMap<CheckKey, Result<(), String>>>> = OnceLock::new();
    CHECKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Instantiates, activates and runs plugin `id` from `file` for one block in a
/// separate process. Results are remembered until the file changes.
pub fn check(exe: &Path, file: &Path, id: &str, rate: f64, block: u32) -> Result<(), String> {
    let st = stamp(file).ok_or_else(|| format!("{} was not found", file.display()))?;
    let key = (file.to_path_buf(), st, id.to_string(), rate.to_bits(), block);
    if let Some(r) = checked().lock().ok().and_then(|m| m.get(&key).cloned()) {
        return r;
    }
    let (rate_s, block_s) = (rate.to_string(), block.to_string());
    let args: [&std::ffi::OsStr; 8] = [
        "--scan".as_ref(),
        file.as_os_str(),
        "--plugin".as_ref(),
        id.as_ref(),
        "--rate".as_ref(),
        rate_s.as_ref(),
        "--block".as_ref(),
        block_s.as_ref(),
    ];
    let passed = format!("{CHECK_PASSED} {id}");
    let r = run_scan(exe, &args, file).and_then(|out| {
        if out.lines().any(|l| l.trim() == passed) {
            Ok(())
        } else {
            Err(format!("{} ended its check early; it was not loaded", file.display()))
        }
    });
    if let Ok(mut m) = checked().lock() {
        m.insert(key, r.clone());
    }
    r
}

#[derive(Default)]
struct Found {
    files: BTreeMap<PathBuf, (Stamp, Result<Vec<PluginInfo>, String>)>,
}

/// Scans the plugin folders in the background: at start, then every 30 s for
/// files that are new or changed. Stops when dropped.
pub struct Scanner {
    found: Arc<Mutex<Found>>,
    stop: Arc<AtomicBool>,
}

impl Scanner {
    pub fn start(exe: PathBuf, dirs: Vec<PathBuf>) -> Scanner {
        let found = Arc::new(Mutex::new(Found::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, s) = (found.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name("confluence-plugin-scan".into()).spawn(move || {
            while !s.load(Ordering::SeqCst) {
                scan_once(&exe, &dirs, &f, &s);
                let next = Instant::now() + RESCAN;
                while !s.load(Ordering::SeqCst) && Instant::now() < next {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("confluence-engine: warning: plugin scan not started: {e}");
        }
        Scanner { found, stop }
    }

    /// Plugins found (sorted by name), and files that failed: (path, why).
    pub fn list(&self) -> (Vec<PluginInfo>, Vec<(String, String)>) {
        let Ok(f) = self.found.lock() else { return (Vec::new(), Vec::new()) };
        let mut plugins = Vec::new();
        let mut bad = Vec::new();
        for (path, (_, r)) in &f.files {
            match r {
                Ok(list) => plugins.extend(list.iter().cloned()),
                Err(why) => bad.push((path.display().to_string(), why.clone())),
            }
        }
        plugins.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        (plugins, bad)
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn scan_once(exe: &Path, dirs: &[PathBuf], found: &Mutex<Found>, stop: &AtomicBool) {
    let files = clap_files(dirs);
    if let Ok(mut f) = found.lock() {
        f.files.retain(|p, _| files.contains(p));
    }
    for file in files {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Some(st) = stamp(&file) else { continue };
        let known = found.lock().ok().and_then(|f| f.files.get(&file).map(|(s, _)| *s));
        if known == Some(st) {
            continue;
        }
        let r = describe(exe, &file);
        if let Ok(mut f) = found.lock() {
            f.files.insert(file, (st, r));
        }
    }
}
