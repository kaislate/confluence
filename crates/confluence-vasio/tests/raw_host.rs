//! VASIO under a bare-bones host that talks to the `IASIO` vtable directly,
//! the way real DAWs do, including calling back into the driver from inside
//! `bufferSwitch`. None of this may hang or corrupt the host.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use confluence_provider_asio::sys::*;
use windows::core::GUID;

#[repr(C)]
struct ClassFactoryVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create_instance: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> i32,
    lock_server: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);

/// The driver object the callbacks below talk to, and what they should do.
static DRIVER: AtomicPtr<IAsio> = AtomicPtr::new(std::ptr::null_mut());
static CALLBACKS: AtomicU64 = AtomicU64::new(0);
/// 0 = query the driver from bufferSwitch; 1 = also call stop() from inside it once.
static MODE: AtomicU32 = AtomicU32::new(0);
/// Largest |getSamplePosition timestamp - timeGetTime()| seen in a callback, in ms.
static STAMP_ERROR_MS: AtomicI64 = AtomicI64::new(0);
/// Largest amount the timestamp fell behind timeGetTime(), in ms. timeGetTime
/// is coarse (up to 15.6 ms ticks) but never ahead of true time, so a correctly
/// anchored systemTime is never behind it.
static STAMP_BEHIND_MS: AtomicI64 = AtomicI64::new(0);
/// kAsioResetRequest messages received (this host does not support them).
static RESETS: AtomicU64 = AtomicU64::new(0);

fn vt(d: *mut IAsio) -> &'static IAsioVtbl {
    // SAFETY: a live driver object starts with its vtable pointer.
    unsafe { &*(*d).vtbl }
}

extern "C" fn buffer_switch(_half: i32, _direct: AsioBool) {
    let d = DRIVER.load(Ordering::Acquire);
    CALLBACKS.fetch_add(1, Ordering::AcqRel);
    // Hosts commonly ask the driver for its position (and other things) here.
    let (mut pos, mut stamp) = (AsioSamples::default(), AsioTimeStamp::default());
    let (mut i, mut o) = (0, 0);
    // SAFETY: live driver; valid out-pointers.
    let r = unsafe {
        let r = (vt(d).get_sample_position)(d, &mut pos, &mut stamp);
        (vt(d).get_latencies)(d, &mut i, &mut o);
        r
    };
    if r == ASE_OK {
        // The SDK defines systemTime on the timeGetTime() basis, in nanoseconds.
        // SAFETY: plain query.
        let tgt = i64::from(unsafe { windows::Win32::Media::timeGetTime() });
        let err = (stamp.value() / 1_000_000 - tgt).abs();
        STAMP_ERROR_MS.fetch_max(err, Ordering::AcqRel);
        STAMP_BEHIND_MS.fetch_max(tgt - stamp.value() / 1_000_000, Ordering::AcqRel);
    }
    if MODE.load(Ordering::Acquire) == 1 && CALLBACKS.load(Ordering::Acquire) == 20 {
        // Some hosts stop the driver from its own callback (e.g. on an error).
        // SAFETY: live driver.
        unsafe { (vt(d).stop)(d) };
    }
}

extern "C" fn rate_changed(_: f64) {}

extern "C" fn message(selector: i32, _value: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    if selector == K_RESET_REQUEST {
        RESETS.fetch_add(1, Ordering::AcqRel);
    }
    0 // supports nothing: no time info (plain bufferSwitch), no reset requests
}

extern "C" fn switch_time_info(p: *mut AsioTime, half: i32, direct: AsioBool) -> *mut AsioTime {
    buffer_switch(half, direct);
    p
}

static HOST_CALLBACKS: AsioCallbacks = AsioCallbacks {
    buffer_switch,
    sample_rate_did_change: rate_changed,
    asio_message: message,
    buffer_switch_time_info: switch_time_info,
};

fn create(instance: u32) -> *mut IAsio {
    let clsid = confluence_vasio::clsid(instance);
    let mut factory = std::ptr::null_mut();
    // SAFETY: valid pointers.
    assert_eq!(unsafe { confluence_vasio::DllGetClassObject(&clsid, &IID_ICLASS_FACTORY, &mut factory) }, 0);
    // SAFETY: an IClassFactory starts with its vtable pointer.
    let fvt = unsafe { &**factory.cast::<*const ClassFactoryVtbl>() };
    let mut p = std::ptr::null_mut();
    // SAFETY: live factory; valid pointers.
    assert_eq!(unsafe { (fvt.create_instance)(factory, std::ptr::null_mut(), &clsid, &mut p) }, 0);
    p.cast()
}

/// init + createBuffers (2 in, 2 out) at the driver's preferred block.
fn prepare(d: *mut IAsio) -> Vec<AsioBufferInfo> {
    // SAFETY: live driver; valid pointers.
    unsafe {
        assert_eq!((vt(d).init)(d, std::ptr::null_mut()), ASIO_TRUE);
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        (vt(d).get_buffer_size)(d, &mut min, &mut max, &mut pref, &mut gran);
        let mut infos: Vec<AsioBufferInfo> = [(ASIO_TRUE, 0), (ASIO_TRUE, 1), (ASIO_FALSE, 0), (ASIO_FALSE, 1)]
            .iter()
            .map(|&(is_input, ch)| AsioBufferInfo { is_input, channel_num: ch, buffers: [std::ptr::null_mut(); 2] })
            .collect();
        assert_eq!((vt(d).create_buffers)(d, infos.as_mut_ptr(), 4, pref, &HOST_CALLBACKS), ASE_OK);
        infos
    }
}

/// Runs `f` on another thread; fails the test if it has not returned in 3 s.
fn must_return(what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    assert!(rx.recv_timeout(Duration::from_secs(3)).is_ok(), "{what} hung the host");
}

#[derive(Clone, Copy)]
struct SendPtr(*mut IAsio);
// SAFETY: the driver may be called from any host thread.
unsafe impl Send for SendPtr {}

/// All scenarios share the host statics, so they run in sequence in one test.
#[test]
fn a_host_that_calls_back_into_the_driver_never_hangs() {
    confluence_provider_vasio::isolate_for_tests();

    // 1. The host queries the driver from bufferSwitch while another thread stops it.
    let d = create(6);
    DRIVER.store(d, Ordering::Release);
    MODE.store(0, Ordering::Release);
    let _infos = prepare(d);
    let (mut pos, mut stamp) = (AsioSamples::default(), AsioTimeStamp::default());
    // SAFETY: live driver; valid out-pointers.
    let r = unsafe { (vt(d).get_sample_position)(d, &mut pos, &mut stamp) };
    assert_eq!(r, ASE_SP_NOT_ADVANCING, "no position while not running (SDK)");
    // SAFETY: live driver.
    assert_eq!(unsafe { (vt(d).start)(d) }, ASE_OK);
    std::thread::sleep(Duration::from_millis(300));
    assert!(CALLBACKS.load(Ordering::Acquire) > 10);
    let err = STAMP_ERROR_MS.load(Ordering::Acquire);
    assert!(err <= 20, "systemTime is on the timeGetTime() basis: off by {err} ms");
    let behind = STAMP_BEHIND_MS.load(Ordering::Acquire);
    assert!(behind <= 1, "systemTime is anchored on a timeGetTime() tick, never behind it: {behind} ms behind");
    let p = SendPtr(d);
    must_return("stop() while the callback queries the driver", move || {
        let p = p;
        // SAFETY: live driver.
        unsafe { (vt(p.0).stop)(p.0) };
    });
    must_return("disposeBuffers() + Release()", move || {
        let p = p;
        // SAFETY: live driver, last reference.
        unsafe {
            (vt(p.0).dispose_buffers)(p.0);
            (vt(p.0).release)(p.0);
        }
    });

    // 2. The host stops the driver from inside its own callback.
    let d = create(7);
    DRIVER.store(d, Ordering::Release);
    CALLBACKS.store(0, Ordering::Release);
    MODE.store(1, Ordering::Release);
    let _infos = prepare(d);
    // SAFETY: live driver.
    assert_eq!(unsafe { (vt(d).start)(d) }, ASE_OK);
    std::thread::sleep(Duration::from_millis(500));
    let after_stop = CALLBACKS.load(Ordering::Acquire);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(CALLBACKS.load(Ordering::Acquire), after_stop, "stop() from the callback really stopped it");
    let p = SendPtr(d);
    must_return("disposeBuffers() + Release() after a stop from the callback", move || {
        let p = p;
        // SAFETY: live driver, last reference.
        unsafe {
            (vt(p.0).dispose_buffers)(p.0);
            (vt(p.0).release)(p.0);
        }
    });
    // 3. The engine appears with another shape: a host that does not support
    //    reset requests must not be sent one.
    let d = create(8);
    DRIVER.store(d, Ordering::Release);
    MODE.store(0, Ordering::Release);
    let _infos = prepare(d);
    let block = {
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        // SAFETY: live driver.
        unsafe { (vt(d).get_buffer_size)(d, &mut min, &mut max, &mut pref, &mut gran) };
        pref as usize
    };
    // SAFETY: live driver.
    assert_eq!(unsafe { (vt(d).start)(d) }, ASE_OK);
    // The engine comes up with another shape: a host that supports resets would get one.
    let _engine = confluence_provider_vasio::VasioSlot::open(8, 2, 2, 48_000.0, if block == 128 { 256 } else { 128 });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(RESETS.load(Ordering::Acquire), 0, "unsupported messages are not sent");
    // SAFETY: live driver, last reference.
    unsafe {
        (vt(d).stop)(d);
        (vt(d).dispose_buffers)(d);
        (vt(d).release)(d);
    }
}

#[test]
fn a_second_init_without_dispose_never_starts_on_mismatched_buffers() {
    confluence_provider_vasio::isolate_for_tests();
    let d = create(5);
    let _infos = prepare(d);
    // The engine appears with a different block size between the two inits.
    let block = {
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        // SAFETY: live driver.
        unsafe { (vt(d).get_buffer_size)(d, &mut min, &mut max, &mut pref, &mut gran) };
        pref as usize
    };
    let other = if block == 1024 { 512 } else { block * 2 };
    let _engine = confluence_provider_vasio::VasioSlot::open(5, 2, 2, 48_000.0, other).unwrap();
    // SAFETY: live driver.
    unsafe {
        assert_eq!((vt(d).init)(d, std::ptr::null_mut()), ASIO_TRUE);
        assert_ne!((vt(d).start)(d), ASE_OK, "buffers made for {block} frames must not stream {other}-frame blocks");
        (vt(d).dispose_buffers)(d);
        (vt(d).release)(d);
    }
}
