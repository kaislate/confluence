//! Installed ASIO drivers, from `HKLM\SOFTWARE\ASIO` (each subkey holds a
//! `CLSID` and a `Description`).

use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows::Win32::System::Com::CLSIDFromString;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
};

#[derive(Clone, Debug, PartialEq)]
pub struct DriverEntry {
    /// Registry key name: the name users see in host applications.
    pub name: String,
    pub clsid: GUID,
    pub description: String,
}

/// Lists installed ASIO drivers. Entries with a missing or malformed CLSID are skipped.
pub fn installed_drivers() -> std::io::Result<Vec<DriverEntry>> {
    let mut key = HKEY::default();
    // SAFETY: valid key path and out-pointer; closed below.
    let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, &HSTRING::from("SOFTWARE\\ASIO"), None, KEY_READ, &mut key) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(Vec::new());
    }
    if r != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(r.0 as i32));
    }
    let mut out = Vec::new();
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` UTF-16 units.
        let r = unsafe { RegEnumKeyExW(key, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        if r != ERROR_SUCCESS {
            continue;
        }
        let sub = &name[..len as usize + 1];
        let Some(clsid_text) = read_string(key, sub, "CLSID") else { continue };
        // SAFETY: NUL-terminated wide string.
        let Ok(clsid) = (unsafe { CLSIDFromString(&HSTRING::from(clsid_text.as_str())) }) else { continue };
        let description = read_string(key, sub, "Description").unwrap_or_default();
        out.push(DriverEntry { name: String::from_utf16_lossy(&name[..len as usize]), clsid, description });
    }
    // SAFETY: opened above.
    unsafe {
        let _ = RegCloseKey(key);
    }
    Ok(out)
}

/// Finds an installed driver by exact name.
pub fn find_driver(name: &str) -> std::io::Result<Option<DriverEntry>> {
    Ok(installed_drivers()?.into_iter().find(|d| d.name == name))
}

fn read_string(key: HKEY, subkey_nul: &[u16], value: &str) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut bytes = (buf.len() * 2) as u32;
    let value = HSTRING::from(value);
    // SAFETY: `subkey_nul` is NUL-terminated; `buf` holds `bytes` bytes.
    let r = unsafe {
        RegGetValueW(
            key,
            PCWSTR(subkey_nul.as_ptr()),
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let units = (bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..units]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_succeeds_and_entries_are_well_formed() {
        // Works with or without drivers installed (CI machines have none).
        for d in installed_drivers().unwrap() {
            assert!(!d.name.is_empty());
            assert_ne!(d.clsid, GUID::zeroed(), "{}", d.name);
        }
        assert_eq!(find_driver("no such driver, surely").unwrap(), None);
    }
}
