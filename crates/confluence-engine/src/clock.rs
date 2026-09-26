//! The internal clock: a synthetic master driven by a high-resolution
//! waitable timer on an MMCSS "Pro Audio" thread (spec §6.1, §7.6).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    TIMER_ALL_ACCESS,
};

use crate::audio::AudioEngine;
use crate::rt::{enable_flush_denormals, now_seconds, ProAudioThread};

/// If the clock falls this many blocks behind (e.g. the machine slept), it
/// re-anchors instead of bursting to catch up.
const MAX_LAG_BLOCKS: f64 = 4.0;

pub struct InternalClock {
    stop: Arc<AtomicBool>,
    late: Arc<AtomicU64>,
    thread: Option<JoinHandle<AudioEngine>>,
}

struct Timer(HANDLE);

impl Timer {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain object creation; closed in Drop.
        let h =
            unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)? };
        Ok(Self(h))
    }

    /// Blocks until `seconds` from now (returns immediately if ≤ 0).
    fn sleep(&self, seconds: f64) {
        if seconds <= 0.0 {
            return;
        }
        // Negative = relative, in 100 ns units.
        let due = -((seconds * 1e7) as i64).max(1);
        // SAFETY: valid timer handle and due-time pointer; no completion routine.
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                let r = WaitForSingleObject(self.0, 1000);
                debug_assert_eq!(r, WAIT_OBJECT_0);
            }
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: handle owned by this struct.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl InternalClock {
    /// Starts calling `audio.process_block` every `block / sample_rate` seconds.
    pub fn start(mut audio: AudioEngine, sample_rate: f64) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicU64::new(0));
        let period = audio.block() as f64 / sample_rate;
        let thread = {
            let (stop, late) = (stop.clone(), late.clone());
            std::thread::Builder::new().name("confluence-internal-clock".into()).spawn(move || {
                let _mmcss = ProAudioThread::enter().ok();
                enable_flush_denormals();
                let Ok(timer) = Timer::new() else { return audio };
                let mut anchor = now_seconds();
                let mut k = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let due = anchor + (k + 1) as f64 * period;
                    let now = now_seconds();
                    if now - due > MAX_LAG_BLOCKS * period {
                        late.fetch_add(1, Ordering::Relaxed);
                        anchor = now;
                        k = 0;
                        continue;
                    }
                    timer.sleep(due - now);
                    audio.process_block(due);
                    k += 1;
                }
                audio
            })?
        };
        Ok(Self { stop, late, thread: Some(thread) })
    }

    /// Times the clock had to re-anchor because it fell behind.
    pub fn late_events(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    /// Stops the clock and returns the audio engine.
    pub fn stop(mut self) -> Option<AudioEngine> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().and_then(|t| t.join().ok())
    }
}

impl Drop for InternalClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
