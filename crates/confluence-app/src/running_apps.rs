//! The programs running now, for choosing an app to capture (spec: round 3
//! §4): which have a window, which are playing audio and how loud. Read
//! on a worker thread while the app picker is open.

/// One running process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proc {
    pub pid: u32,
    /// Its executable's file name ("discord.exe").
    pub exe: String,
    pub path: Option<String>,
    /// The executable's FileDescription ("Discord").
    pub description: Option<String>,
    /// It has a visible top-level window.
    pub window: bool,
    /// It has an active audio session.
    pub audio: bool,
    /// Its sessions' loudest peak, 0..1.
    pub level: f32,
}

/// One row of the picker: every process of one executable.
#[derive(Clone, Debug, PartialEq)]
pub struct AppRow {
    pub exe: String,
    pub name: String,
    pub pids: Vec<u32>,
    pub audio: bool,
    pub level: f32,
    /// No window and no audio.
    pub background: bool,
}

/// Windows' own shell hosts: they have windows but are not apps anyone
/// captures, so they count as background.
const SHELL_HOSTS: [&str; 6] = [
    "applicationframehost.exe",
    "textinputhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "lockapp.exe",
];

/// The picker's rows from `procs`: (playing audio, other apps), one row per
/// executable (case-insensitive), matching `filter` on name or executable.
/// Playing rows are loudest first; other rows are alphabetical. Background
/// processes (no window, no audio) appear only with `advanced`; `own_pids`
/// are left out.
pub fn rows(procs: &[Proc], filter: &str, advanced: bool, own_pids: &[u32]) -> (Vec<AppRow>, Vec<AppRow>) {
    let mut by_exe: Vec<(String, AppRow)> = Vec::new();
    // PIDs 0 and 4 are System Idle and System.
    for p in procs.iter().filter(|p| !own_pids.contains(&p.pid) && p.pid != 0 && p.pid != 4 && !p.exe.is_empty()) {
        let key = p.exe.to_ascii_lowercase();
        let i = match by_exe.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let stem = p.exe.rsplit_once('.').map_or(p.exe.as_str(), |(s, _)| s).to_string();
                by_exe.push((
                    key,
                    AppRow {
                        exe: p.exe.clone(),
                        name: stem,
                        pids: Vec::new(),
                        audio: false,
                        level: 0.0,
                        background: true,
                    },
                ));
                by_exe.len() - 1
            }
        };
        let row = &mut by_exe[i].1;
        row.pids.push(p.pid);
        row.audio |= p.audio;
        row.level = row.level.max(if p.audio { p.level } else { 0.0 });
        row.background &= (!p.window || SHELL_HOSTS.contains(&row_key(&p.exe).as_str())) && !p.audio;
        if let Some(d) = p.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            row.name = d.to_string();
        }
    }
    let needle = filter.trim().to_lowercase();
    let mut playing = Vec::new();
    let mut other = Vec::new();
    for (_, row) in by_exe {
        if !needle.is_empty() && !row.name.to_lowercase().contains(&needle) && !row.exe.to_lowercase().contains(&needle)
        {
            continue;
        }
        if row.audio {
            playing.push(row);
        } else if advanced || !row.background {
            other.push(row);
        }
    }
    playing.sort_by(|a, b| b.level.total_cmp(&a.level).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    other.sort_by_key(|r| r.name.to_lowercase());
    (playing, other)
}

fn row_key(exe: &str) -> String {
    exe.to_ascii_lowercase()
}

/// Every running process, with windows, audio sessions and descriptions
/// (empty on other platforms). Never panics; anything unreadable is skipped.
#[cfg(windows)]
pub fn snapshot() -> Vec<Proc> {
    win::snapshot()
}

#[cfg(not(windows))]
pub fn snapshot() -> Vec<Proc> {
    Vec::new()
}

/// Which processes have an active audio session, and their peak (cheap:
/// sessions only).
#[cfg(windows)]
pub fn audio_levels() -> Vec<(u32, bool, f32)> {
    win::audio_levels()
}

#[cfg(not(windows))]
pub fn audio_levels() -> Vec<(u32, bool, f32)> {
    Vec::new()
}

/// Reads the running apps on a worker thread while it lives: everything
/// every 2 s, the audio levels every 100 ms.
pub struct AppLister {
    latest: std::sync::Arc<std::sync::Mutex<Vec<Proc>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AppLister {
    pub fn open() -> AppLister {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let latest = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (out, quit) = (latest.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("confluence-app-list".into()).spawn(move || {
            #[cfg(windows)]
            win::com_init();
            let mut full_at: Option<Instant> = None;
            while !quit.load(Ordering::Relaxed) {
                if full_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                    let procs = snapshot();
                    full_at = Some(Instant::now());
                    if let Ok(mut l) = out.lock() {
                        *l = procs;
                    }
                } else {
                    let levels = audio_levels();
                    if let Ok(mut l) = out.lock() {
                        for p in l.iter_mut() {
                            let found = levels.iter().find(|(pid, _, _)| *pid == p.pid);
                            p.audio = found.is_some_and(|f| f.1);
                            p.level = found.map_or(0.0, |f| f.2);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        AppLister { latest, stop }
    }

    /// The latest list (empty until the first read lands).
    pub fn latest(&self) -> Vec<Proc> {
        self.latest.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl Drop for AppLister {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use windows::core::{Interface, BOOL, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
        GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
    };

    use super::Proc;

    /// Joins the multithreaded COM apartment (once per thread; harmless if
    /// already joined).
    pub fn com_init() {
        // SAFETY: plain COM initialisation for this thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    pub fn snapshot() -> Vec<Proc> {
        com_init();
        let mut procs = processes();
        let windows = window_pids();
        let audio = audio_levels();
        for p in procs.iter_mut() {
            p.window = windows.contains(&p.pid);
            if let Some((_, active, level)) = audio.iter().find(|(pid, _, _)| *pid == p.pid) {
                p.audio = *active;
                p.level = *level;
            }
            if p.window || p.audio {
                p.path = crate::app_icon::image_path(p.pid);
                p.description = p.path.as_deref().and_then(description);
            }
        }
        procs
    }

    fn processes() -> Vec<Proc> {
        let mut out = Vec::new();
        // SAFETY: a process snapshot, closed below.
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        // SAFETY: `e` is sized for the call.
        let mut ok = unsafe { Process32FirstW(snap, &mut e) }.is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(Proc {
                pid: e.th32ProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..len]),
                ..Proc::default()
            });
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snap, &mut e) }.is_ok();
        }
        // SAFETY: the snapshot handle from above.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(snap);
        }
        out
    }

    /// Processes owning a visible, titled, unowned top-level window that is
    /// not a tool window.
    fn window_pids() -> Vec<u32> {
        unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: `lparam` is the Vec passed to EnumWindows below, alive for the call.
            let pids = unsafe { &mut *(lparam.0 as *mut Vec<u32>) };
            // SAFETY: window queries on a handle EnumWindows gave us.
            unsafe {
                let tool = (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) != 0;
                let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid());
                if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 && !tool && !owned {
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid != 0 && !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
            BOOL(1)
        }
        let mut pids: Vec<u32> = Vec::new();
        // SAFETY: the callback only touches `pids` during the call.
        let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut pids as *mut Vec<u32> as isize)) };
        pids
    }

    /// (pid, session active, peak) for every audio session on every active
    /// playback device.
    pub fn audio_levels() -> Vec<(u32, bool, f32)> {
        let mut out: Vec<(u32, bool, f32)> = Vec::new();
        // SAFETY: COM calls on objects we hold for the duration; every failure skips.
        unsafe {
            let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
                return out;
            };
            let Ok(devices) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
            let n = devices.GetCount().unwrap_or(0);
            for d in 0..n {
                let Ok(dev) = devices.Item(d) else { continue };
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(sessions) = mgr.GetSessionEnumerator() else { continue };
                for s in 0..sessions.GetCount().unwrap_or(0) {
                    let Ok(ctl) = sessions.GetSession(s) else { continue };
                    let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                    let Ok(pid) = ctl2.GetProcessId() else { continue };
                    if pid == 0 {
                        continue;
                    }
                    let active = ctl.GetState().is_ok_and(|st| st == AudioSessionStateActive);
                    let peak = ctl.cast::<IAudioMeterInformation>().and_then(|m| m.GetPeakValue()).unwrap_or(0.0);
                    match out.iter_mut().find(|(p, _, _)| *p == pid) {
                        Some(e) => {
                            e.1 |= active;
                            e.2 = e.2.max(peak);
                        }
                        None => out.push((pid, active, peak)),
                    }
                }
            }
        }
        out
    }

    /// An executable's FileDescription, cached by path.
    fn description(path: &str) -> Option<String> {
        static CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);
        if let Ok(mut c) = CACHE.lock() {
            if let Some(hit) = c.get_or_insert_with(HashMap::new).get(path) {
                return hit.clone();
            }
        }
        let found = read_description(path);
        if let Ok(mut c) = CACHE.lock() {
            c.get_or_insert_with(HashMap::new).insert(path.to_string(), found.clone());
        }
        found
    }

    fn read_description(path: &str) -> Option<String> {
        let file = HSTRING::from(path);
        // SAFETY: version-info calls into a buffer of the size they report.
        unsafe {
            let size = GetFileVersionInfoSizeW(&file, None);
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            GetFileVersionInfoW(&file, None, size, buf.as_mut_ptr().cast()).ok()?;
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            let trans = HSTRING::from("\\VarFileInfo\\Translation");
            let lang = if VerQueryValueW(buf.as_ptr().cast(), PCWSTR(trans.as_ptr()), &mut ptr, &mut len).as_bool()
                && len >= 4
            {
                let pair = ptr as *const u16;
                format!("{:04x}{:04x}", *pair, *pair.add(1))
            } else {
                "040904b0".to_string()
            };
            let key = HSTRING::from(format!("\\StringFileInfo\\{lang}\\FileDescription"));
            if !VerQueryValueW(buf.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool() || len == 0 {
                return None;
            }
            let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            let s = String::from_utf16_lossy(&text[..end]).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, exe: &str, window: bool, audio: bool, level: f32) -> Proc {
        Proc { pid, exe: exe.into(), window, audio, level, ..Proc::default() }
    }

    fn names(rows: &[AppRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn apps_group_by_exe_and_playing_comes_first() {
        let mut procs = vec![
            p(10, "chrome.exe", true, false, 0.0),
            p(11, "chrome.exe", false, true, 0.4),
            p(12, "Chrome.exe", false, false, 0.0),
            p(20, "discord.exe", true, true, 0.7),
            p(30, "explorer.exe", true, false, 0.0),
            p(40, "svchost.exe", false, false, 0.0),
        ];
        procs[3].description = Some("Discord".into());
        let (playing, other) = rows(&procs, "", false, &[]);
        assert_eq!(names(&playing), ["Discord", "chrome"], "loudest first");
        assert_eq!(playing[1].pids, [10, 11, 12], "one row per executable");
        assert!((playing[1].level - 0.4).abs() < 1e-6);
        assert_eq!(names(&other), ["explorer"]);
        let (_, other) = rows(&procs, "", true, &[]);
        assert_eq!(names(&other), ["explorer", "svchost"], "background processes with advanced options");
        assert!(other[1].background);
    }

    #[test]
    fn the_filter_matches_name_or_exe_case_insensitively() {
        let mut procs = vec![p(1, "Discord.exe", true, false, 0.0), p(2, "obs64.exe", true, false, 0.0)];
        procs[1].description = Some("OBS Studio".into());
        assert_eq!(names(&rows(&procs, "DISC", false, &[]).1), ["Discord"]);
        assert_eq!(names(&rows(&procs, "studio", false, &[]).1), ["OBS Studio"]);
        assert_eq!(names(&rows(&procs, "obs64", false, &[]).1), ["OBS Studio"]);
        assert!(rows(&procs, "zzz", false, &[]).1.is_empty());
    }

    #[test]
    fn windows_shell_hosts_count_as_background() {
        let procs =
            vec![p(1, "ApplicationFrameHost.exe", true, false, 0.0), p(2, "TextInputHost.exe", true, false, 0.0)];
        assert!(rows(&procs, "", false, &[]).1.is_empty());
        assert_eq!(rows(&procs, "", true, &[]).1.len(), 2, "still there with advanced options");
    }

    #[test]
    fn own_processes_are_left_out() {
        let procs = vec![p(1, "confluence.exe", true, false, 0.0), p(2, "confluence-engine.exe", false, true, 0.1)];
        let (playing, other) = rows(&procs, "", true, &[1, 2]);
        assert!(playing.is_empty() && other.is_empty());
    }

    #[test]
    fn a_name_falls_back_to_the_exe_stem() {
        let procs = vec![p(1, "obs64.exe", true, false, 0.0)];
        assert_eq!(rows(&procs, "", false, &[]).1[0].name, "obs64");
        assert_eq!(rows(&procs, "", false, &[]).1[0].exe, "obs64.exe");
    }

    #[cfg(windows)]
    #[test]
    fn the_snapshot_lists_this_test_process() {
        let me = std::process::id();
        let procs = snapshot();
        let mine = procs.iter().find(|p| p.pid == me).expect("this process is listed");
        assert!(mine.exe.to_ascii_lowercase().ends_with(".exe"), "{}", mine.exe);
    }
}
