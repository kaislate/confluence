//! `confluence-vasio.dll`: the virtual ASIO driver (spec §7.4). An in-proc
//! COM server with eight driver classes, "Confluence VASIO 1" … "8". A DAW
//! that loads one exchanges audio with the engine through `confluence-shm`.
//!
//! The DAW must never hang or crash because of Confluence: every entry point
//! catches panics, nothing waits on the engine without a timeout, and while
//! the engine is absent the driver keeps the DAW running on silence.
#![cfg(windows)]
#![allow(non_snake_case)] // the COM exports have fixed names

mod driver;
mod factory;
pub mod register;
mod stream;

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::GUID;

pub use confluence_provider_vasio::INSTANCES;

const S_OK: i32 = 0;
const S_FALSE: i32 = 1;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const CLASS_E_CLASSNOTAVAILABLE: i32 = 0x8004_0111u32 as i32;
const SELFREG_E_CLASS: i32 = 0x8004_0201u32 as i32;

/// Live driver objects plus `LockServer` locks, for `DllCanUnloadNow`.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The COM class id of instance `n` (1-based).
pub fn clsid(instance: u32) -> GUID {
    GUID::from_values(0x5E2A_7C31, 0x9B4D, 0x4F6A, [0x8C, 0x1E, 0x3D, 0x7B, 0x9A, 0x0F, 0x21, instance as u8])
}

/// The instance whose class id is `id`.
pub fn instance_of(id: &GUID) -> Option<u32> {
    (1..=INSTANCES).find(|&n| clsid(n) == *id)
}

/// Driver name shown in DAWs.
pub fn driver_name(instance: u32) -> String {
    format!("Confluence VASIO {instance}")
}

/// COM entry point: the class factory for one of our driver classes.
///
/// # Safety
/// Called by COM (or a host) with valid GUID pointers and out-pointer.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> i32 {
    std::panic::catch_unwind(|| {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: checked non-null; the caller guarantees validity.
        let (rclsid, riid) = unsafe { (&*rclsid, &*riid) };
        // SAFETY: as above.
        unsafe { *ppv = std::ptr::null_mut() };
        let Some(instance) = instance_of(rclsid) else { return CLASS_E_CLASSNOTAVAILABLE };
        match factory::get(instance, riid) {
            Some(p) => {
                // SAFETY: as above.
                unsafe { *ppv = p };
                S_OK
            }
            None => E_NOINTERFACE,
        }
    })
    .unwrap_or(E_POINTER)
}

/// COM entry point: whether the DLL may be unloaded.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    if LIVE.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

/// `regsvr32 confluence-vasio.dll` (as administrator): registers the eight
/// driver classes and their `HKLM\SOFTWARE\ASIO` entries.
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> i32 {
    std::panic::catch_unwind(|| match register::module_path() {
        Some(path) if register::register(&path).is_ok() => S_OK,
        _ => SELFREG_E_CLASS,
    })
    .unwrap_or(SELFREG_E_CLASS)
}

/// `regsvr32 /u confluence-vasio.dll`: removes what `DllRegisterServer` wrote.
#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> i32 {
    std::panic::catch_unwind(|| if register::unregister().is_ok() { S_OK } else { SELFREG_E_CLASS })
        .unwrap_or(SELFREG_E_CLASS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_distinct_classes_that_map_back_to_their_instance() {
        let ids: Vec<GUID> = (1..=INSTANCES).map(clsid).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(instance_of(id), Some(i as u32 + 1));
            assert_eq!(ids.iter().filter(|x| *x == id).count(), 1);
        }
        assert_eq!(instance_of(&clsid(9)), None);
        assert_eq!(driver_name(3), "Confluence VASIO 3");
    }

    #[test]
    fn unknown_classes_and_interfaces_are_refused() {
        let mut p = std::ptr::null_mut();
        let bogus = GUID::from_u128(0x1234);
        // SAFETY: valid pointers.
        assert_eq!(unsafe { DllGetClassObject(&bogus, &bogus, &mut p) }, CLASS_E_CLASSNOTAVAILABLE);
        // SAFETY: valid pointers.
        assert_eq!(unsafe { DllGetClassObject(&clsid(1), &bogus, &mut p) }, E_NOINTERFACE);
        assert!(p.is_null());
        // SAFETY: null pointers are rejected, not dereferenced.
        assert_eq!(unsafe { DllGetClassObject(std::ptr::null(), &bogus, &mut p) }, E_POINTER);
    }
}
