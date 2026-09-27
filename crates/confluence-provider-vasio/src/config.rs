//! The last shape the engine served for each instance, remembered in
//! `HKCU\Software\Confluence\VASIO\<n>`. A DAW that opens a VASIO driver
//! while the engine is not running gets this shape (and runs on silence)
//! instead of a guess that would change once the engine starts.

use windows::core::HSTRING;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD,
};

use confluence_shm::Layout;

/// Registry root under HKEY_CURRENT_USER.
pub const ROOT: &str = "Software\\Confluence\\VASIO";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstanceConfig {
    pub daw_inputs: u32,
    pub daw_outputs: u32,
    pub sample_rate: u32,
    pub block: u32,
}

impl Default for InstanceConfig {
    /// Used before the engine has ever served the instance.
    fn default() -> Self {
        InstanceConfig { daw_inputs: 2, daw_outputs: 2, sample_rate: 48_000, block: 256 }
    }
}

impl InstanceConfig {
    pub fn from_layout(l: &Layout) -> Self {
        InstanceConfig {
            daw_inputs: l.to_client_channels,
            daw_outputs: l.from_client_channels,
            sample_rate: l.sample_rate.round() as u32,
            block: l.block,
        }
    }
}

const VALUES: [&str; 4] = ["Inputs", "Outputs", "SampleRate", "Block"];

fn key(root: &str, instance: u32) -> HSTRING {
    HSTRING::from(format!("{root}\\{instance}"))
}

/// Saves `cfg` for `instance` under `root`.
pub fn save_at(root: &str, instance: u32, cfg: &InstanceConfig) -> std::io::Result<()> {
    let key = key(root, instance);
    for (name, v) in VALUES.iter().zip([cfg.daw_inputs, cfg.daw_outputs, cfg.sample_rate, cfg.block]) {
        // SAFETY: `v` is a 4-byte DWORD for the duration of the call.
        let r = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(*name),
                REG_DWORD.0,
                Some((&v as *const u32).cast()),
                4,
            )
        };
        if r != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r.0 as i32));
        }
    }
    Ok(())
}

/// Loads the config saved for `instance` under `root`, if complete and sane.
pub fn load_at(root: &str, instance: u32) -> Option<InstanceConfig> {
    let key = key(root, instance);
    let mut v = [0u32; 4];
    for (name, slot) in VALUES.iter().zip(v.iter_mut()) {
        let mut size = 4u32;
        // SAFETY: `slot` holds 4 bytes, as `size` says.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(*name),
                RRF_RT_REG_DWORD,
                None,
                Some((slot as *mut u32).cast()),
                Some(&mut size),
            )
        };
        if r != ERROR_SUCCESS {
            return None;
        }
    }
    let cfg = InstanceConfig { daw_inputs: v[0], daw_outputs: v[1], sample_rate: v[2], block: v[3] };
    let ok = (2..=128).contains(&cfg.daw_inputs)
        && (2..=128).contains(&cfg.daw_outputs)
        && (8_000..=768_000).contains(&cfg.sample_rate)
        && (16..=8192).contains(&cfg.block);
    ok.then_some(cfg)
}

pub fn save(instance: u32, cfg: &InstanceConfig) -> std::io::Result<()> {
    save_at(ROOT, instance, cfg)
}

pub fn load(instance: u32) -> Option<InstanceConfig> {
    load_at(ROOT, instance)
}

/// Removes everything under `root` (tests and uninstall).
pub fn delete_root(root: &str) {
    // SAFETY: valid wide string.
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_shape_comes_back_and_nonsense_is_ignored() {
        let root = format!("Software\\ConfluenceTest\\VASIO.{}", std::process::id());
        assert_eq!(load_at(&root, 1), None, "nothing saved yet");
        let cfg = InstanceConfig { daw_inputs: 16, daw_outputs: 8, sample_rate: 96_000, block: 128 };
        save_at(&root, 1, &cfg).unwrap();
        assert_eq!(load_at(&root, 1), Some(cfg));
        save_at(&root, 2, &InstanceConfig { block: 0, ..cfg }).unwrap();
        assert_eq!(load_at(&root, 2), None, "an unusable saved shape is ignored");
        delete_root(&root);
        assert_eq!(load_at(&root, 1), None);
    }
}
