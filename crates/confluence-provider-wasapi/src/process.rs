//! Finding the process to capture for per-application capture.

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

use crate::WasapiError;

/// A process as seen in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub exe: String,
}

fn snapshot() -> Vec<Proc> {
    let mut out = Vec::new();
    // SAFETY: standard ToolHelp enumeration; the handle is closed below.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(Proc {
                pid: e.th32ProcessID,
                parent: e.th32ParentProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Picks the root of an application's process tree among `procs` matching `name`
/// (with or without `.exe`, case-insensitive). Capture is requested for the
/// whole tree, so the root is the right target for multi-process apps.
pub(crate) fn pick(procs: &[Proc], name: &str) -> Option<u32> {
    let want = name.trim().to_ascii_lowercase();
    let is_match = |p: &Proc| {
        let exe = p.exe.to_ascii_lowercase();
        exe == want || exe == format!("{want}.exe")
    };
    let matches: Vec<&Proc> = procs.iter().filter(|p| is_match(p)).collect();
    matches
        .iter()
        .filter(|p| !matches.iter().any(|q| q.pid == p.parent))
        .map(|p| p.pid)
        .min()
        .or_else(|| matches.iter().map(|p| p.pid).min())
}

/// True while process `pid` runs. A process we may not open (elevated,
/// protected) counts as running: only "no such process" or an exit code
/// says it is gone.
pub fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: query-only access; the handle is closed below.
    let h = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(e) => return e.code() == ERROR_ACCESS_DENIED.to_hresult(),
    };
    let mut code = 0u32;
    // SAFETY: `h` is a process handle we own; `code` receives the exit code.
    let ok = unsafe { GetExitCodeProcess(h, &mut code) }.is_ok();
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = CloseHandle(h);
    }
    !ok || code == STILL_ACTIVE.0 as u32
}

/// Resolves a process name (e.g. `Discord`, `obs64.exe`) or a numeric PID.
pub fn find_process(name_or_pid: &str) -> Result<u32, WasapiError> {
    if let Ok(pid) = name_or_pid.trim().parse::<u32>() {
        return Ok(pid);
    }
    pick(&snapshot(), name_or_pid).ok_or_else(|| WasapiError::NoSuchProcess(name_or_pid.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_process_is_alive_and_an_exited_one_is_not() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("cmd").args(["/c", "exit"]).spawn().expect("cmd runs");
        let pid = child.id();
        child.wait().unwrap();
        // The handle `child` held is closed with it, so the PID is gone.
        drop(child);
        assert!(!process_alive(pid), "{pid} exited");
    }

    fn p(pid: u32, parent: u32, exe: &str) -> Proc {
        Proc { pid, parent, exe: exe.into() }
    }

    #[test]
    fn picks_the_root_of_a_multi_process_app() {
        let procs = [
            p(900, 4, "explorer.exe"),
            p(1200, 900, "Discord.exe"),
            p(1300, 1200, "Discord.exe"),
            p(1100, 1200, "Discord.exe"),
        ];
        assert_eq!(pick(&procs, "discord"), Some(1200));
        assert_eq!(pick(&procs, "Discord.exe"), Some(1200));
        assert_eq!(pick(&procs, "obs64"), None);
    }

    #[test]
    fn pids_pass_through_and_our_own_process_is_found() {
        assert_eq!(find_process("4242").unwrap(), 4242);
        let me = std::env::current_exe().unwrap();
        let exe = me.file_name().unwrap().to_string_lossy().to_string();
        assert!(find_process(&exe).is_ok(), "{exe}");
        assert!(matches!(find_process("surely-not-running-xyz"), Err(WasapiError::NoSuchProcess(_))));
    }
}
