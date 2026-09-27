//! Per-slot ASIO callback entry points.
//!
//! `ASIOCallbacks` carry no context pointer, so each hosted driver gets its own
//! set of four C functions, monomorphised per slot index `N` (spec §7.1: a fixed
//! table of 16 trampoline sets). A slot's state is published through an
//! `AtomicPtr` before `createBuffers` and withdrawn after `stop`.
//!
//! Teardown protocol: every entry point first increments its slot's `active`
//! counter, *then* loads the state pointer, and holds a guard while it uses
//! the state. Teardown nulls the pointer, then waits for `active` to reach 0.
//! Both sides use SeqCst, so either the callback's increment is seen by the
//! teardown (which waits for it) or the callback sees the null pointer. A
//! callback that never returns makes teardown fail, and the state is leaked
//! rather than freed.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::io::{AsioIo, Channel};
use crate::sys::*;
use crate::AsioCallback;

pub const MAX_DRIVERS: usize = 16;

/// A handler that panics this many blocks in a row is no longer called: its
/// outputs stay silent and every block counts as a fault. This bounds how
/// often a panic (and its hook) runs on the driver's real-time thread.
pub const MAX_CONSECUTIVE_FAULTS: u32 = 8;

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
    pub last_position: AtomicI64,
    /// Handler panics in a row (see [`MAX_CONSECUTIVE_FAULTS`]).
    pub consecutive_faults: AtomicU32,
    pub clock: fn() -> f64,
    pub health: Arc<AsioHealth>,
}

// SAFETY: the raw driver pointer and UnsafeCells are only used under the
// protocol documented on each access: the control thread writes before `ready`
// is set (and after it is cleared and active callbacks drained); the
// driver's single callback thread reads/uses them while `ready` is set.
unsafe impl Send for SlotState {}
unsafe impl Sync for SlotState {}

pub(crate) struct Slot {
    busy: AtomicBool,
    state: AtomicPtr<SlotState>,
    /// Entry points currently inside this slot (see the module docs).
    active: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot =
    Slot { busy: AtomicBool::new(false), state: AtomicPtr::new(std::ptr::null_mut()), active: AtomicU32::new(0) };
pub(crate) static SLOTS: [Slot; MAX_DRIVERS] = [EMPTY; MAX_DRIVERS];

/// Claims a free slot index.
pub(crate) fn claim() -> Option<usize> {
    SLOTS.iter().position(|s| s.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
}

/// Releases a slot claimed with [`claim`]. Its state must already be
/// withdrawn and drained ([`withdraw`] returned true).
pub(crate) fn release(index: usize) {
    SLOTS[index].state.store(std::ptr::null_mut(), Ordering::SeqCst);
    SLOTS[index].busy.store(false, Ordering::Release);
}

/// Makes `state` visible to the slot's entry points.
pub(crate) fn publish(index: usize, state: *mut SlotState) {
    SLOTS[index].state.store(state, Ordering::SeqCst);
}

/// Waits until no entry point is inside the slot. False on timeout.
pub(crate) fn wait_idle(index: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while SLOTS[index].active.load(Ordering::SeqCst) != 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// Hides the slot's state from entry points and waits for those still inside
/// to leave. True when the state may be freed; false means a callback is
/// stuck inside it, and the caller must leak the state and keep the slot.
pub(crate) fn withdraw(index: usize, timeout: Duration) -> bool {
    SLOTS[index].state.store(std::ptr::null_mut(), Ordering::SeqCst);
    wait_idle(index, timeout)
}

/// Proof that an entry point is inside a slot; the state stays alive while it exists.
pub(crate) struct Entered(usize);

impl Drop for Entered {
    fn drop(&mut self) {
        SLOTS[self.0].active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Enters slot `n`: counts in first, then loads the state (never the other way round).
pub(crate) fn enter(n: usize) -> Option<(Entered, &'static SlotState)> {
    SLOTS[n].active.fetch_add(1, Ordering::SeqCst);
    let guard = Entered(n);
    let p = SLOTS[n].state.load(Ordering::SeqCst);
    // SAFETY: a published state is freed only after `withdraw` saw `active`
    // reach 0, which cannot happen while `guard` is alive.
    (!p.is_null()).then(|| (guard, unsafe { &*p }))
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
    // Every entry point is panic-guarded (spec §7.1): nothing may unwind into the driver.
    let _ = catch_unwind(|| {
        if let Some((_inside, st)) = enter(N) {
            st.health.rate_changes.fetch_add(1, Ordering::Relaxed);
        }
    });
}

extern "C" fn asio_message<const N: usize>(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32 {
    catch_unwind(|| handle_message::<N>(selector, value, message, opt)).unwrap_or(0)
}

fn handle_message<const N: usize>(selector: i32, value: i32, _message: *mut c_void, _opt: *mut f64) -> i32 {
    let count = |f: fn(&AsioHealth) -> &AtomicU64| {
        if let Some((_inside, st)) = enter(N) {
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

fn dispatch(n: usize, half: i32, time: Option<*mut AsioTime>) {
    let Some((_inside, st)) = enter(n) else { return };
    // SeqCst pairs with the control thread's `ready.store(false)` + `wait_idle`.
    if st.ready.load(Ordering::SeqCst) {
        // A panic must never unwind into the driver (that aborts the process):
        // count it as a fault instead.
        if catch_unwind(AssertUnwindSafe(|| run(st, (half & 1) as usize, time))).is_err() {
            st.health.faults.fetch_add(1, Ordering::Relaxed);
        }
    }
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
    let faults_in_a_row = st.consecutive_faults.load(Ordering::Relaxed);
    let ok = faults_in_a_row < MAX_CONSECUTIVE_FAULTS
        && catch_unwind(AssertUnwindSafe(|| callback.process(&mut io))).is_ok();
    if ok {
        st.consecutive_faults.store(0, Ordering::Relaxed);
    } else {
        st.consecutive_faults.store(faults_in_a_row.saturating_add(1), Ordering::Relaxed);
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

    fn dummy_state() -> *mut SlotState {
        Box::into_raw(Box::new(SlotState {
            driver: std::ptr::null_mut(),
            block: 64,
            inputs: UnsafeCell::new(Vec::new()),
            outputs: UnsafeCell::new(Vec::new()),
            callback: UnsafeCell::new(Box::new(|_: &mut AsioIo<'_>| {})),
            post_output: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            last_position: AtomicI64::new(i64::MIN),
            consecutive_faults: AtomicU32::new(0),
            clock: || 0.0,
            health: Arc::default(),
        }))
    }

    #[test]
    fn teardown_waits_for_a_callback_that_is_inside_the_slot() {
        use std::time::Duration;
        let n = claim().unwrap();
        let st = dummy_state();
        publish(n, st);
        let guard = enter(n).expect("a published slot can be entered");
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || tx.send(withdraw(n, Duration::from_secs(5))).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err(), "teardown must wait while a callback holds the state");
        assert!(enter(n).is_none(), "callbacks arriving after withdrawal see nothing");
        drop(guard);
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "drained once the callback left");
        t.join().unwrap();
        release(n);
        // SAFETY: withdrawn and drained, so nothing else can reach it.
        drop(unsafe { Box::from_raw(st) });
    }

    #[test]
    fn a_callback_that_never_returns_makes_teardown_report_failure() {
        use std::time::Duration;
        let n = claim().unwrap();
        publish(n, dummy_state());
        let stuck = enter(n).unwrap();
        assert!(!withdraw(n, Duration::from_millis(50)), "the caller must leak, not free, the state");
        // The stuck callback never leaves; the slot (and its state) stay leaked.
        std::mem::forget(stuck);
    }
}
