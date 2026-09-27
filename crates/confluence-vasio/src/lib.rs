//! `confluence-vasio.dll`: the virtual ASIO driver (spec §7.4). An in-proc
//! COM server with eight driver classes, "Confluence VASIO 1" … "8". A DAW
//! that loads one exchanges audio with the engine through `confluence-shm`.
//!
//! The DAW must never hang or crash because of Confluence: every entry point
//! catches panics, nothing waits on the engine without a timeout, and while
//! the engine is absent the driver keeps the DAW running on silence.
#![cfg(windows)]
#![allow(non_snake_case)] // the COM exports have fixed names

pub mod register;

use windows::core::GUID;

pub use confluence_provider_vasio::INSTANCES;

const S_OK: i32 = 0;
const SELFREG_E_CLASS: i32 = 0x8004_0201u32 as i32;

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
}
