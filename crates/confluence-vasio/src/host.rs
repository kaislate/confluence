//! The name of the program that loaded the driver (the DAW), as the engine
//! shows it on the VASIO position: the exe's FileDescription, else its file
//! name.

use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

/// The host program's name, worked out once per process.
pub fn host_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let Some(exe) = exe_path() else { return String::new() };
        description(&exe).unwrap_or_else(|| {
            std::path::Path::new(&exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        })
    })
}

fn exe_path() -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: `buf` is writable for its length; no module handle means the exe.
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// The exe's version-resource FileDescription, if it has one.
fn description(exe: &str) -> Option<String> {
    let path = HSTRING::from(exe);
    // SAFETY: valid wide string.
    let size = unsafe { GetFileVersionInfoSizeW(&path, None) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` holds `size` bytes.
    unsafe { GetFileVersionInfoW(&path, None, size, data.as_mut_ptr().cast()) }.ok()?;
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `data` is the version block just read; the out-pointers are valid.
    let found = unsafe {
        VerQueryValueW(data.as_ptr().cast(), &HSTRING::from("\\VarFileInfo\\Translation"), &mut ptr, &mut len)
    };
    let (lang, page) = if found.as_bool() && len >= 4 && !ptr.is_null() {
        // SAFETY: the translation table holds at least one (language, code page) pair of u16s.
        let pair = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), 2) };
        (pair[0], pair[1])
    } else {
        (0x0409, 0x04B0)
    };
    let key = HSTRING::from(format!("\\StringFileInfo\\{lang:04x}{page:04x}\\FileDescription"));
    // SAFETY: as above.
    let found = unsafe { VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len) };
    if !found.as_bool() || len == 0 || ptr.is_null() {
        return None;
    }
    // SAFETY: the value is `len` UTF-16 units (its NUL included) inside `data`.
    let units = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize) };
    let text = String::from_utf16_lossy(units).trim_end_matches('\0').trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_without_a_description_is_named_by_its_file() {
        let stem = std::env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(host_name(), stem);
    }
}
