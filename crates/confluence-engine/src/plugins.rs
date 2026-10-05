//! Finding CLAP plugins, and checking a plugin in a separate process before the
//! engine loads it: a plugin that crashes while loading takes down only that
//! process.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
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

/// Readers report through channels, so a pipe held open by a helper process
/// the plugin started cannot keep us waiting.
fn read_all(mut r: Box<dyn Read + Send>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx
}

/// What became of a scan process.
enum Ended {
    Exited(i32),
    TimedOut,
}

/// Runs `exe` with `args` (a scan process) and returns its stdout, or why it failed.
fn run_scan(exe: &Path, args: &[&std::ffi::OsStr], file: &Path) -> Result<String, String> {
    let (ended, out, err) = spawn_scan(exe, args)?;
    let collect = |rx: std::sync::mpsc::Receiver<String>| rx.recv_timeout(OUTPUT_GRACE).unwrap_or_default();
    match ended {
        Ended::TimedOut => Err(format!("{} did not finish loading in 10 s", file.display())),
        Ended::Exited(0) => Ok(collect(out)),
        Ended::Exited(SCAN_FAILED) => Err(collect(err).trim().to_string()),
        Ended::Exited(_) => Err(format!("{} crashed while loading; it was not loaded", file.display())),
    }
}

/// Starts a scan process inside a job that kills it if the engine ends
/// (`win::spawn_in_job`), waits for it up to the scan timeout, and returns
/// how it ended with readers of its output.
#[cfg(windows)]
fn spawn_scan(
    exe: &Path,
    args: &[&std::ffi::OsStr],
) -> Result<(Ended, std::sync::mpsc::Receiver<String>, std::sync::mpsc::Receiver<String>), String> {
    let child = win::spawn_in_job(exe, args).map_err(|e| format!("could not start the plugin check: {e}"))?;
    let (out, err) = (read_all(Box::new(child.stdout)), read_all(Box::new(child.stderr)));
    let ended = match child.process.wait(SCAN_TIMEOUT) {
        Some(code) => Ended::Exited(code),
        None => {
            child.process.kill();
            Ended::TimedOut
        }
    };
    Ok((ended, out, err))
}

#[cfg(not(windows))]
fn spawn_scan(
    exe: &Path,
    args: &[&std::ffi::OsStr],
) -> Result<(Ended, std::sync::mpsc::Receiver<String>, std::sync::mpsc::Receiver<String>), String> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the plugin check: {e}"))?;
    let empty = || -> Box<dyn Read + Send> { Box::new(std::io::empty()) };
    let out = read_all(child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>).unwrap_or_else(empty));
    let err = read_all(child.stderr.take().map(|o| Box::new(o) as Box<dyn Read + Send>).unwrap_or_else(empty));
    let deadline = Instant::now() + SCAN_TIMEOUT;
    let ended = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ended::Exited(st.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::TimedOut;
            }
        }
    };
    Ok((ended, out, err))
}

/// Scan processes created inside a kill-on-close job, so none outlives the
/// engine, not even one caught half-created when the engine was killed. Only
/// scan processes go in it: whatever plugins start from the engine itself (a
/// browser opened by an editor) is left alone.
#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::Duration;

    use windows::core::{w, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, SetHandleInformation, GENERIC_READ, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    /// A running scan process. Its handle closes when dropped.
    pub struct Process(HANDLE);

    impl Process {
        /// Waits up to `timeout`; its exit code, or `None` if still running.
        pub fn wait(&self, timeout: Duration) -> Option<i32> {
            // SAFETY: a valid process handle we own.
            unsafe {
                if WaitForSingleObject(self.0, timeout.as_millis() as u32) != WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0u32;
                GetExitCodeProcess(self.0, &mut code).ok()?;
                Some(code as i32)
            }
        }

        pub fn kill(&self) {
            // SAFETY: as above.
            unsafe {
                let _ = TerminateProcess(self.0, 1);
                let _ = WaitForSingleObject(self.0, 5000);
            }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Child {
        pub process: Process,
        pub stdout: File,
        pub stderr: File,
    }

    /// The job scan processes are created in, as an integer (handles are not
    /// `Sync`). Never closed: it closes when the engine ends, killing them.
    fn job() -> Result<HANDLE, String> {
        static JOB: OnceLock<Result<isize, String>> = OnceLock::new();
        let job = JOB.get_or_init(|| {
            // SAFETY: an unnamed job, configured before use.
            unsafe {
                let job = CreateJobObjectW(None, None).map_err(|e| e.to_string())?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .map_err(|e| e.to_string())?;
                Ok(job.0 as isize)
            }
        });
        job.clone().map(|h| HANDLE(h as *mut _))
    }

    /// One argument quoted for a Windows command line.
    fn quote(arg: &OsStr, out: &mut Vec<u16>) {
        let a: Vec<u16> = arg.encode_wide().collect();
        let needs = a.is_empty() || a.iter().any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
        if !needs {
            out.extend(a);
            return;
        }
        out.push(b'"' as u16);
        let mut backslashes = 0;
        for &c in &a {
            if c == b'\\' as u16 {
                backslashes += 1;
                continue;
            }
            if c == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            } else {
                out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            }
            backslashes = 0;
            out.push(c);
        }
        out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        out.push(b'"' as u16);
    }

    /// An inheritable anonymous pipe: (our read end, its write end).
    fn pipe(sa: &SECURITY_ATTRIBUTES) -> Result<(HANDLE, HANDLE), String> {
        let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
        // SAFETY: out-params for a new pipe; our end is then made non-inheritable.
        unsafe {
            CreatePipe(&mut r, &mut w, Some(sa), 0).map_err(|e| e.to_string())?;
            SetHandleInformation(r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(|e| e.to_string())?;
        }
        Ok((r, w))
    }

    /// Starts `exe args` without a console, inside the scan job, with its
    /// stdout and stderr piped to us and stdin from NUL. Only those three
    /// handles are inherited.
    pub fn spawn_in_job(exe: &Path, args: &[&OsStr]) -> Result<Child, String> {
        let job = job()?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (out_r, out_w) = pipe(&sa)?;
        let (err_r, err_w) = pipe(&sa)?;
        // SAFETY: opening the NUL device for the child's stdin.
        let nul = unsafe {
            CreateFileW(
                w!("NUL"),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&sa),
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        let close = |hs: &[HANDLE]| {
            for h in hs {
                // SAFETY: handles we created.
                let _ = unsafe { CloseHandle(*h) };
            }
        };

        let mut cmdline = Vec::new();
        quote(exe.as_os_str(), &mut cmdline);
        for a in args {
            cmdline.push(b' ' as u16);
            quote(a, &mut cmdline);
        }
        cmdline.push(0);

        let inherit = [nul, out_w, err_w];
        let jobs = [job];
        let mut size = 0usize;
        // SAFETY: the first call only reports the size the list needs.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 2, None, &mut size) };
        let mut buf = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
        let started = (|| -> Result<PROCESS_INFORMATION, String> {
            // SAFETY: `buf` is the size asked for and outlives the list's use;
            // the attribute values (`inherit`, `jobs`) outlive CreateProcessW.
            unsafe {
                InitializeProcThreadAttributeList(Some(list), 2, None, &mut size).map_err(|e| e.to_string())?;
                let result = (|| {
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                        Some(inherit.as_ptr().cast()),
                        std::mem::size_of_val(&inherit),
                        None,
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                        Some(jobs.as_ptr().cast()),
                        std::mem::size_of_val(&jobs),
                        None,
                        None,
                    )
                    .map_err(|e| e.to_string())?;
                    let mut si = STARTUPINFOEXW::default();
                    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                    si.StartupInfo.hStdInput = nul;
                    si.StartupInfo.hStdOutput = out_w;
                    si.StartupInfo.hStdError = err_w;
                    si.lpAttributeList = list;
                    let mut pi = PROCESS_INFORMATION::default();
                    CreateProcessW(
                        None,
                        Some(PWSTR(cmdline.as_mut_ptr())),
                        None,
                        None,
                        true,
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                        None,
                        None,
                        &si.StartupInfo,
                        &mut pi,
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(pi)
                })();
                DeleteProcThreadAttributeList(list);
                result
            }
        })();
        // The child has its own copies now (or failed to start).
        close(&inherit);
        let pi = match started {
            Ok(pi) => pi,
            Err(e) => {
                close(&[out_r, err_r]);
                return Err(e);
            }
        };
        close(&[pi.hThread]);
        // SAFETY: we own these read ends; File takes them over.
        let (stdout, stderr) = unsafe { (File::from_raw_handle(out_r.0), File::from_raw_handle(err_r.0)) };
        Ok(Child { process: Process(pi.hProcess), stdout, stderr })
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
