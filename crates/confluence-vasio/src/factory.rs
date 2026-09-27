//! `IClassFactory` for the driver classes. The factories are static objects,
//! so their reference counting is a no-op; `LockServer` counts toward
//! `DllCanUnloadNow`.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::core::GUID;

use crate::{driver, E_NOINTERFACE, E_POINTER, INSTANCES, LIVE, S_OK};

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110u32 as i32;

#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut Factory, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Factory) -> u32,
    release: unsafe extern "system" fn(*mut Factory) -> u32,
    create_instance: unsafe extern "system" fn(*mut Factory, *mut c_void, *const GUID, *mut *mut c_void) -> i32,
    lock_server: unsafe extern "system" fn(*mut Factory, i32) -> i32,
}

#[repr(C)]
struct Factory {
    vtbl: &'static Vtbl,
    instance: u32,
}

// SAFETY: immutable after construction.
unsafe impl Sync for Factory {}

static VTBL: Vtbl = Vtbl { query_interface, add_ref, release, create_instance, lock_server };

static FACTORIES: [Factory; INSTANCES as usize] = [
    Factory { vtbl: &VTBL, instance: 1 },
    Factory { vtbl: &VTBL, instance: 2 },
    Factory { vtbl: &VTBL, instance: 3 },
    Factory { vtbl: &VTBL, instance: 4 },
    Factory { vtbl: &VTBL, instance: 5 },
    Factory { vtbl: &VTBL, instance: 6 },
    Factory { vtbl: &VTBL, instance: 7 },
    Factory { vtbl: &VTBL, instance: 8 },
];

/// The factory for `instance` as interface `riid`, if supported.
pub(crate) fn get(instance: u32, riid: &GUID) -> Option<*mut c_void> {
    let f = FACTORIES.get(instance.checked_sub(1)? as usize)?;
    (*riid == IID_IUNKNOWN || *riid == IID_ICLASS_FACTORY).then_some(f as *const Factory as *mut c_void)
}

unsafe extern "system" fn query_interface(this: *mut Factory, riid: *const GUID, ppv: *mut *mut c_void) -> i32 {
    if riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM passes valid pointers; `this` is one of FACTORIES.
    unsafe {
        let ok = *riid == IID_IUNKNOWN || *riid == IID_ICLASS_FACTORY;
        *ppv = if ok { this.cast() } else { std::ptr::null_mut() };
        if ok {
            S_OK
        } else {
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn add_ref(_: *mut Factory) -> u32 {
    2
}

unsafe extern "system" fn release(_: *mut Factory) -> u32 {
    1
}

unsafe extern "system" fn create_instance(
    this: *mut Factory,
    outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> i32 {
    std::panic::catch_unwind(|| {
        if riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM passes valid pointers; `this` is one of FACTORIES.
        unsafe {
            *ppv = std::ptr::null_mut();
            if !outer.is_null() {
                return CLASS_E_NOAGGREGATION;
            }
            let instance = (*this).instance;
            // ASIO hosts ask for the driver's own CLSID as the interface id.
            if *riid != IID_IUNKNOWN && *riid != crate::clsid(instance) {
                return E_NOINTERFACE;
            }
            *ppv = driver::create(instance).cast();
        }
        S_OK
    })
    .unwrap_or(E_POINTER)
}

unsafe extern "system" fn lock_server(_: *mut Factory, lock: i32) -> i32 {
    if lock != 0 {
        LIVE.fetch_add(1, Ordering::AcqRel);
    } else {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
    S_OK
}
