//! Real-time plumbing for Windows shared by the engine and device providers:
//! the engine time base, MMCSS registration, FTZ/DAZ, COM apartments and
//! opting the process out of power throttling (spec §3.1, §5.4).
#![cfg(windows)]

use std::sync::OnceLock;

use std::marker::PhantomData;

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess, ProcessPowerThrottling,
    SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};

/// (QPC value at the first call, QPC frequency).
static EPOCH: OnceLock<(i64, f64)> = OnceLock::new();

fn qpc() -> i64 {
    let mut v = 0i64;
    // SAFETY: valid out-pointer; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}

fn epoch() -> (i64, f64) {
    *EPOCH.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out-pointer.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        (qpc(), f.max(1) as f64)
    })
}

/// Seconds since the first call, on the QueryPerformanceCounter clock. Device
/// timestamps and engine block times must all be on this time base.
pub fn now_seconds() -> f64 {
    let (start, freq) = epoch();
    (qpc() - start) as f64 / freq
}

/// Converts a QPC position in 100 ns units (as WASAPI reports) to the
/// [`now_seconds`] time base.
pub fn qpc_100ns_to_seconds(qpc_100ns: u64) -> f64 {
    let (start, freq) = epoch();
    qpc_100ns as f64 * 1e-7 - start as f64 / freq
}

/// Opts the whole process out of EcoQoS execution-speed throttling and of
/// timer-resolution throttling when windowless or minimized.
pub fn disable_power_throttling() -> windows::core::Result<()> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: `state` is a valid PROCESS_POWER_THROTTLING_STATE of the size passed.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
}

/// Sets flush-to-zero and denormals-are-zero for the calling thread, so tiny
/// values in decaying signals never hit slow denormal arithmetic (spec §5.4).
#[cfg(target_arch = "x86_64")]
pub fn enable_flush_denormals() {
    const FTZ: u32 = 1 << 15;
    const DAZ: u32 = 1 << 6;
    let mut csr: u32 = 0;
    // SAFETY: reads and writes only this thread's MXCSR register via a valid pointer.
    unsafe {
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
        csr |= FTZ | DAZ;
        std::arch::asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn enable_flush_denormals() {}

/// COM initialized on the calling thread until dropped. Not `Send`: COM
/// apartments belong to the thread that entered them.
pub struct ComApartment(PhantomData<*const ()>);

impl ComApartment {
    /// Single-threaded apartment: required for ASIO driver control calls.
    pub fn single_threaded() -> windows::core::Result<Self> {
        Self::enter(COINIT_APARTMENTTHREADED)
    }

    /// Multi-threaded apartment: used for WASAPI streams.
    pub fn multi_threaded() -> windows::core::Result<Self> {
        Self::enter(COINIT_MULTITHREADED)
    }

    fn enter(mode: COINIT) -> windows::core::Result<Self> {
        // SAFETY: balanced by CoUninitialize in Drop on the same thread.
        unsafe { CoInitializeEx(None, mode).ok()? };
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: this thread entered COM successfully in `enter`.
        unsafe { CoUninitialize() };
    }
}

/// Registers the current thread with MMCSS "Pro Audio" until dropped.
pub struct ProAudioThread(HANDLE);

impl ProAudioThread {
    pub fn enter() -> windows::core::Result<Self> {
        let mut task_index = 0u32;
        // SAFETY: valid task name and out-pointer; the handle is reverted in Drop.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index)? };
        Ok(Self(handle))
    }
}

impl Drop for ProAudioThread {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_and_tracks_wall_time() {
        let a = now_seconds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let b = now_seconds();
        assert!(b - a > 0.045 && b - a < 0.2, "{}", b - a);
    }

    #[test]
    fn wasapi_qpc_positions_map_onto_the_same_time_base() {
        let (_, freq) = epoch();
        let now = now_seconds();
        let raw_100ns = (qpc() as f64 / freq * 1e7) as u64;
        assert!((qpc_100ns_to_seconds(raw_100ns) - now).abs() < 0.01);
    }

    #[test]
    fn process_can_opt_out_of_throttling() {
        disable_power_throttling().unwrap();
    }

    #[test]
    fn denormals_flush_to_zero_on_the_calling_thread() {
        std::thread::spawn(|| {
            enable_flush_denormals();
            let tiny = std::hint::black_box(f32::MIN_POSITIVE);
            assert_eq!(tiny * 0.5, 0.0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn threads_can_enter_either_com_apartment() {
        std::thread::spawn(|| drop(ComApartment::single_threaded().unwrap())).join().unwrap();
        std::thread::spawn(|| drop(ComApartment::multi_threaded().unwrap())).join().unwrap();
    }

    #[test]
    fn a_thread_cannot_switch_apartment_mode() {
        std::thread::spawn(|| {
            let _sta = ComApartment::single_threaded().unwrap();
            assert!(ComApartment::multi_threaded().is_err());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn thread_can_join_mmcss() {
        let guard = ProAudioThread::enter().unwrap();
        drop(guard);
    }
}
