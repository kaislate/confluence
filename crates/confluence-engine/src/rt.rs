//! Real-time plumbing for Windows: the engine time base, MMCSS registration
//! and opting the process out of power throttling (spec §3.1).

use std::sync::OnceLock;
use std::time::Instant;

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess, ProcessPowerThrottling,
    SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Seconds since the first call, on the QPC-backed monotonic clock. Device
/// timestamps and engine block times must all come from this function.
pub fn now_seconds() -> f64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
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
    fn clock_is_monotonic() {
        let a = now_seconds();
        let b = now_seconds();
        assert!(b >= a);
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
    fn thread_can_join_mmcss() {
        let guard = ProAudioThread::enter().unwrap();
        drop(guard);
    }
}
