//! COM and ASIO registration. Real registration writes HKLM (administrator);
//! the `_at` variants take any hive and key prefix so tests can use HKCU.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Registry::{RegDeleteTreeW, RegSetKeyValueW, HKEY, HKEY_LOCAL_MACHINE, REG_SZ};

use crate::{clsid, driver_name, INSTANCES};

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` for instance `n`.
pub fn clsid_string(instance: u32) -> String {
    let g = clsid(instance);
    let d = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

/// The key paths (relative to the hive) one instance writes.
pub fn keys(prefix: &str, instance: u32) -> (String, String) {
    (
        format!("{prefix}SOFTWARE\\Classes\\CLSID\\{}", clsid_string(instance)),
        format!("{prefix}SOFTWARE\\ASIO\\{}", driver_name(instance)),
    )
}

fn set(hive: HKEY, key: &str, name: &str, value: &str) -> std::io::Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let name = if name.is_empty() { None } else { Some(HSTRING::from(name)) };
    // SAFETY: `wide` is a NUL-terminated UTF-16 string of the length given.
    let r = unsafe {
        RegSetKeyValueW(
            hive,
            &HSTRING::from(key),
            name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(r.0 as i32))
    }
}

/// Registers all instances, pointing COM at `dll_path`.
pub fn register_at(hive: HKEY, prefix: &str, dll_path: &str) -> std::io::Result<()> {
    for n in 1..=INSTANCES {
        let (class_key, asio_key) = keys(prefix, n);
        let name = driver_name(n);
        set(hive, &class_key, "", &name)?;
        set(hive, &format!("{class_key}\\InprocServer32"), "", dll_path)?;
        set(hive, &format!("{class_key}\\InprocServer32"), "ThreadingModel", "Apartment")?;
        set(hive, &asio_key, "CLSID", &clsid_string(n))?;
        set(hive, &asio_key, "Description", &name)?;
    }
    Ok(())
}

/// Removes every key `register_at` wrote.
pub fn unregister_at(hive: HKEY, prefix: &str) -> std::io::Result<()> {
    for n in 1..=INSTANCES {
        let (class_key, asio_key) = keys(prefix, n);
        for key in [class_key, asio_key] {
            // SAFETY: valid wide string. A missing key is fine.
            unsafe {
                let _ = RegDeleteTreeW(hive, &HSTRING::from(key));
            }
        }
    }
    Ok(())
}

pub fn register(dll_path: &str) -> std::io::Result<()> {
    register_at(HKEY_LOCAL_MACHINE, "", dll_path)
}

pub fn unregister() -> std::io::Result<()> {
    unregister_at(HKEY_LOCAL_MACHINE, "")
}

/// Full path of the module containing this code (the DLL, once built).
pub fn module_path() -> Option<String> {
    let mut module = HMODULE::default();
    let anchor = module_path as *const () as *const u16;
    // SAFETY: the address lies inside this module; the handle is not ref-counted.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = [0u16; 1024];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    fn get(key: &str, name: &str) -> Option<String> {
        let mut buf = [0u16; 512];
        let mut bytes = (buf.len() * 2) as u32;
        let name = HSTRING::from(name);
        // SAFETY: `buf` holds `bytes` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(bytes as usize / 2).saturating_sub(1)]))
    }

    #[test]
    fn registration_writes_the_com_and_asio_keys_and_removes_them() {
        let prefix = format!("Software\\ConfluenceTest\\reg.{}\\", std::process::id());
        register_at(HKEY_CURRENT_USER, &prefix, "C:\\x\\confluence_vasio.dll").unwrap();
        for n in [1, 8] {
            let (class_key, asio_key) = keys(&prefix, n);
            assert_eq!(
                get(&format!("{class_key}\\InprocServer32"), "").as_deref(),
                Some("C:\\x\\confluence_vasio.dll")
            );
            assert_eq!(get(&format!("{class_key}\\InprocServer32"), "ThreadingModel").as_deref(), Some("Apartment"));
            assert_eq!(get(&asio_key, "CLSID"), Some(clsid_string(n)));
            assert_eq!(get(&asio_key, "Description"), Some(driver_name(n)));
        }
        unregister_at(HKEY_CURRENT_USER, &prefix).unwrap();
        let (class_key, asio_key) = keys(&prefix, 1);
        assert_eq!(get(&asio_key, "CLSID"), None);
        assert_eq!(get(&format!("{class_key}\\InprocServer32"), ""), None);
        // SAFETY: valid wide string.
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(prefix.trim_end_matches('\\')));
        }
    }

    #[test]
    fn clsid_strings_are_registry_formatted() {
        assert_eq!(clsid_string(1), "{5E2A7C31-9B4D-4F6A-8C1E-3D7B9A0F2101}");
    }
}
