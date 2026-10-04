//! Putting this test process in a kill-on-close job object, as some terminals
//! and IDEs do with what they start. Each test that uses it lives in its own
//! test binary: joining a job affects the whole process.
#![allow(dead_code)]

use std::ffi::c_void;
use std::path::PathBuf;

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// Joins a new kill-on-close job. The handle is never closed: closing it
/// would kill this process too.
pub fn join_kill_on_close_job(allow_breakaway: bool) -> HANDLE {
    // SAFETY: plain Win32 calls with valid arguments; `info` outlives the call.
    unsafe {
        let job = CreateJobObjectW(None, None).unwrap();
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let breakaway = if allow_breakaway { JOB_OBJECT_LIMIT_BREAKAWAY_OK } else { JOB_OBJECT_LIMIT(0) };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | breakaway;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .unwrap();
        AssignProcessToJobObject(job, GetCurrentProcess()).unwrap();
        job
    }
}

/// Whether process `pid` is in `job`.
pub fn in_job(pid: u32, job: HANDLE) -> bool {
    // SAFETY: the handle is opened and closed here.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap();
        let mut result = BOOL(0);
        IsProcessInJob(process, Some(job), &mut result).unwrap();
        let _ = CloseHandle(process);
        result.as_bool()
    }
}

pub fn kill(pid: u32) {
    // SAFETY: the handle is opened and closed here.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(process, 0);
            let _ = CloseHandle(process);
        }
    }
}

/// A harmless process that runs for a while.
pub fn long_runner() -> (PathBuf, Vec<String>) {
    (PathBuf::from(r"C:\Windows\System32\PING.EXE"), vec!["-n".into(), "30".into(), "127.0.0.1".into()])
}
