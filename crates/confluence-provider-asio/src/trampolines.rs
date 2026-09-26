//! Per-slot ASIO callback entry points.
//!
//! `ASIOCallbacks` carry no context pointer, so each hosted driver gets its own
//! set of four C functions, monomorphised per slot index `N` (spec §7.1: a fixed
//! table of 16 trampoline sets). A slot's state is published through an
//! `AtomicPtr` before `createBuffers` and withdrawn after `stop`.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::io::{AsioIo, Channel};
use crate::sys::*;
use crate::AsioCallback;

pub const MAX_DRIVERS: usize = 16;

/// Stream counters, readable from any thread.
#[derive(Default, Debug)]
pub struct AsioHealth {
    pub callbacks: AtomicU64,
    /// Callbacks whose handler panicked (outputs were silenced instead).
    pub faults: AtomicU64,
    /// Callbacks that followed a skipped buffer (sample position jumped).
    pub gaps: AtomicU64,
    pub reset_requests: AtomicU64,
    pub resync_requests: AtomicU64,
    pub rate_changes: AtomicU64,
    pub overloads: AtomicU64,
}

/// Everything a callback needs, owned by the control thread and shared with the
/// driver's callback thread through [`SLOTS`].
pub(crate) struct SlotState {
    pub driver: *mut IAsio,
    pub block: usize,
    pub inputs: UnsafeCell<Vec<Channel>>,
    pub outputs: UnsafeCell<Vec<Channel>>,
    pub callback: UnsafeCell<Box<dyn AsioCallback>>,
    pub post_output: AtomicBool,
    /// Set after buffers are recorded, cleared before they are disposed.
    pub ready: AtomicBool,
    pub in_flight: AtomicU32,
    pub last_position: AtomicI64,
    pub clock: fn() -> f64,
    pub health: Arc<AsioHealth>,
}

// SAFETY: the raw driver pointer and UnsafeCells are only used under the
// protocol documented on each access: the control thread writes before `ready`
// is set (and after it is cleared and in-flight callbacks drained); the
// driver's single callback thread reads/uses them while `ready` is set.
unsafe impl Send for SlotState {}
unsafe impl Sync for SlotState {}

pub(crate) struct Slot {
    busy: AtomicBool,
    pub state: AtomicPtr<SlotState>,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot = Slot { busy: AtomicBool::new(false), state: AtomicPtr::new(std::ptr::null_mut()) };
pub(crate) static SLOTS: [Slot; MAX_DRIVERS] = [EMPTY; MAX_DRIVERS];

/// Claims a free slot index.
pub(crate) fn claim() -> Option<usize> {
    SLOTS.iter().position(|s| s.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
}

/// Releases a slot claimed with [`claim`]. Its state must already be withdrawn.
pub(crate) fn release(index: usize) {
    SLOTS[index].state.store(std::ptr::null_mut(), Ordering::Release);
    SLOTS[index].busy.store(false, Ordering::Release);
}

extern "C" fn buffer_switch<const N: usize>(index: i32, _direct: AsioBool) {
    dispatch(N, index, None);
}

extern "C" fn buffer_switch_time_info<const N: usize>(
    params: *mut AsioTime,
    index: i32,
    _direct: AsioBool,
) -> *mut AsioTime {
    dispatch(N, index, Some(params));
    params
}

extern "C" fn sample_rate_did_change<const N: usize>(_rate: f64) {
    if let Some(st) = state(N) {
        st.health.rate_changes.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn asio_message<const N: usize>(selector: i32, value: i32, _message: *mut c_void, _opt: *mut f64) -> i32 {
    let count = |f: fn(&AsioHealth) -> &AtomicU64| {
        if let Some(st) = state(N) {
            f(&st.health).fetch_add(1, Ordering::Relaxed);
        }
    };
    match selector {
        K_SELECTOR_SUPPORTED => matches!(
            value,
            K_RESET_REQUEST
                | K_ENGINE_VERSION
                | K_RESYNC_REQUEST
                | K_LATENCIES_CHANGED
                | K_SUPPORTS_TIME_INFO
                | K_OVERLOAD
        ) as i32,
        K_ENGINE_VERSION => 2,
        K_RESET_REQUEST => {
            count(|h| &h.reset_requests);
            1
        }
        K_RESYNC_REQUEST => {
            count(|h| &h.resync_requests);
            1
        }
        K_OVERLOAD => {
            count(|h| &h.overloads);
            1
        }
        K_LATENCIES_CHANGED | K_SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

const fn callbacks<const N: usize>() -> AsioCallbacks {
    AsioCallbacks {
        buffer_switch: buffer_switch::<N>,
        sample_rate_did_change: sample_rate_did_change::<N>,
        asio_message: asio_message::<N>,
        buffer_switch_time_info: buffer_switch_time_info::<N>,
    }
}

pub(crate) static CALLBACKS: [AsioCallbacks; MAX_DRIVERS] = [
    callbacks::<0>(),
    callbacks::<1>(),
    callbacks::<2>(),
    callbacks::<3>(),
    callbacks::<4>(),
    callbacks::<5>(),
    callbacks::<6>(),
    callbacks::<7>(),
    callbacks::<8>(),
    callbacks::<9>(),
    callbacks::<10>(),
    callbacks::<11>(),
    callbacks::<12>(),
    callbacks::<13>(),
    callbacks::<14>(),
    callbacks::<15>(),
];

fn state(n: usize) -> Option<&'static SlotState> {
    let p = SLOTS[n].state.load(Ordering::Acquire);
    // SAFETY: a published state stays alive until withdrawn, and the control
    // thread waits for `in_flight` to drain before freeing it.
    (!p.is_null()).then(|| unsafe { &*p })
}

fn dispatch(n: usize, half: i32, time: Option<*mut AsioTime>) {
    let Some(st) = state(n) else { return };
    st.in_flight.fetch_add(1, Ordering::AcqRel);
    if st.ready.load(Ordering::Acquire) {
        // A panic must never unwind into the driver (that aborts the process):
        // count it as a fault instead.
        if catch_unwind(AssertUnwindSafe(|| run(st, (half & 1) as usize, time))).is_err() {
            st.health.faults.fetch_add(1, Ordering::Relaxed);
        }
    }
    st.in_flight.fetch_sub(1, Ordering::Release);
}

fn run(st: &SlotState, half: usize, time: Option<*mut AsioTime>) {
    let now = (st.clock)();
    let position = time
        .filter(|p| !p.is_null())
        // SAFETY: the driver passes a valid ASIOTime for the duration of the call.
        .map(|p| unsafe { (*p).time_info })
        .filter(|ti| ti.flags & K_SAMPLE_POSITION_VALID != 0)
        .map(|ti| ti.sample_position.value())
        .or_else(|| {
            let (mut pos, mut stamp) = (AsioSamples::default(), AsioTimeStamp::default());
            // SAFETY: calling the driver from its own callback thread is permitted by the SDK.
            let r = unsafe { ((*(*st.driver).vtbl).get_sample_position)(st.driver, &mut pos, &mut stamp) };
            (r == ASE_OK).then(|| pos.value())
        });
    let block = st.block as i64;
    let frames_since_last = match position {
        Some(pos) => {
            let last = st.last_position.swap(pos, Ordering::Relaxed);
            let delta = pos.wrapping_sub(last);
            if last == i64::MIN || delta <= 0 || delta > 64 * block {
                block
            } else {
                if delta > block {
                    st.health.gaps.fetch_add(1, Ordering::Relaxed);
                }
                delta
            }
        }
        None => block,
    } as u32;

    // SAFETY: buffers were recorded before `ready`; the callback is only ever
    // entered from the driver's single callback thread.
    let (inputs, outputs, callback) = unsafe { (&*st.inputs.get(), &*st.outputs.get(), &mut *st.callback.get()) };
    let mut io = AsioIo {
        frames: st.block,
        now,
        sample_position: position.unwrap_or(0),
        frames_since_last,
        inputs,
        outputs,
        half,
    };
    if catch_unwind(AssertUnwindSafe(|| callback.process(&mut io))).is_err() {
        st.health.faults.fetch_add(1, Ordering::Relaxed);
        io.silence_outputs();
    }
    if st.post_output.load(Ordering::Relaxed) {
        // SAFETY: as for get_sample_position.
        unsafe { ((*(*st.driver).vtbl).output_ready)(st.driver) };
    }
    st.health.callbacks.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_slot_has_distinct_entry_points() {
        let a = CALLBACKS[0].buffer_switch_time_info as usize;
        let b = CALLBACKS[1].buffer_switch_time_info as usize;
        let c = CALLBACKS[15].buffer_switch_time_info as usize;
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn host_advertises_time_info_and_engine_version_2() {
        let msg = CALLBACKS[0].asio_message;
        assert_eq!(msg(K_SELECTOR_SUPPORTED, K_SUPPORTS_TIME_INFO, std::ptr::null_mut(), std::ptr::null_mut()), 1);
        assert_eq!(msg(K_SELECTOR_SUPPORTED, K_SUPPORTS_TIME_CODE, std::ptr::null_mut(), std::ptr::null_mut()), 0);
        assert_eq!(msg(K_ENGINE_VERSION, 0, std::ptr::null_mut(), std::ptr::null_mut()), 2);
        assert_eq!(msg(K_SUPPORTS_TIME_INFO, 0, std::ptr::null_mut(), std::ptr::null_mut()), 1);
        assert_eq!(msg(999, 0, std::ptr::null_mut(), std::ptr::null_mut()), 0);
    }
}
