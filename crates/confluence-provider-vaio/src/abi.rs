//! The layout shared with the VAIO driver. It is written independently of the
//! driver's `confluence_vaio_abi.h` (the driver is a separate MIT/MS-PL
//! program); both sides pin the same offsets in tests.

use std::sync::atomic::{AtomicU32, AtomicU64};

pub const MAGIC: u32 = 0x4F49_4156; // "VAIO", little-endian
pub const VERSION: u32 = 1;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// The endpoint's format: 48 kHz, two channels of 32-bit signed PCM
/// (Windows does not create an endpoint for a float-only device format).
pub const BYTES_PER_FRAME: usize = 8;
/// The ring starts this many bytes into the region.
pub const HEADER_BYTES: usize = 4096;
pub const MIN_CAPACITY: u32 = 1024;
pub const MAX_CAPACITY: u32 = 65_536;
pub const MIN_TARGET: u32 = 64;
/// `CTL_CODE(FILE_DEVICE_SOUND, 0x900, METHOD_OUT_DIRECT, FILE_READ_ACCESS | FILE_WRITE_ACCESS)`.
pub const IOCTL_ATTACH: u32 = 0x001D_E402;
/// Path the engine opens.
pub const USER_PATH: &str = r"\\.\ConfluenceVaio";
/// DOS device name, for checking that the driver is installed.
pub const DOS_NAME: &str = "ConfluenceVaio";

/// Start of the shared region. Counters are atomics: the driver reads and
/// writes them through its own mapping of the same pages.
#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub capacity_frames: u32,
    pub target_frames: u32,
    pub attached: AtomicU32,
    pub streaming: AtomicU32,
    pub write_frames: AtomicU64,
    pub read_frames: AtomicU64,
    pub engine_heartbeat: AtomicU64,
    pub driver_ticks: AtomicU64,
    pub freerun_frames: AtomicU64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// The same offsets are pinned by static_asserts in confluence_vaio_abi.h.
    #[test]
    fn the_header_matches_the_drivers_layout() {
        assert_eq!(offset_of!(Header, magic), 0);
        assert_eq!(offset_of!(Header, version), 4);
        assert_eq!(offset_of!(Header, capacity_frames), 8);
        assert_eq!(offset_of!(Header, target_frames), 12);
        assert_eq!(offset_of!(Header, attached), 16);
        assert_eq!(offset_of!(Header, streaming), 20);
        assert_eq!(offset_of!(Header, write_frames), 24);
        assert_eq!(offset_of!(Header, read_frames), 32);
        assert_eq!(offset_of!(Header, engine_heartbeat), 40);
        assert_eq!(offset_of!(Header, driver_ticks), 48);
        assert_eq!(offset_of!(Header, freerun_frames), 56);
        assert_eq!(size_of::<Header>(), 64);
        assert_eq!(align_of::<Header>(), 8);
        assert!(size_of::<Header>() <= HEADER_BYTES);
    }

    #[test]
    fn the_ioctl_code_is_ctl_code_sound_0x900_out_direct_read_write() {
        // CTL_CODE(FILE_DEVICE_SOUND = 0x1D, 0x900, METHOD_OUT_DIRECT = 2, FILE_READ_ACCESS | FILE_WRITE_ACCESS = 3)
        assert_eq!(IOCTL_ATTACH, (0x1D << 16) | (3 << 14) | (0x900 << 2) | 2);
        assert_eq!(IOCTL_ATTACH, 0x001D_E402);
        assert_eq!(BYTES_PER_FRAME, CHANNELS * 4);
    }

    /// The driver's symbolic link is \DosDevices\Global\ConfluenceVaio, which a
    /// process opens as \\.\ConfluenceVaio (two leading backslashes).
    #[test]
    fn the_control_device_path_is_the_dos_device_namespace() {
        assert_eq!(USER_PATH, "\\\\.\\ConfluenceVaio");
        assert_eq!(USER_PATH.strip_prefix("\\\\.\\"), Some(DOS_NAME));
    }
}
