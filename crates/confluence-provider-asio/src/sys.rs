//! Raw ASIO 2.3 ABI for 64-bit Windows, transcribed from the Steinberg ASIO SDK
//! (`common/asio.h`, `common/iasiodrv.h`, used under the SDK's GPLv3 option).
//!
//! The SDK packs every struct to 4-byte alignment (`#pragma pack(push,4)`), and
//! on Windows `NATIVE_INT64` is 0, so 64-bit sample positions and timestamps are
//! `{hi, lo}` pairs of `u32`. Getting either wrong corrupts driver memory, so the
//! layouts are pinned by tests below.

use std::ffi::c_void;

pub type AsioBool = i32;
pub type AsioError = i32;
pub type AsioSampleType = i32;

pub const ASIO_FALSE: AsioBool = 0;
pub const ASIO_TRUE: AsioBool = 1;

pub const ASE_OK: AsioError = 0;
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_SP_NOT_ADVANCING: AsioError = -996;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

// Sample types (ASIOST*LSB; little-endian ones are the only ones found on x64 Windows).
pub const ST_INT16_LSB: AsioSampleType = 16;
pub const ST_INT24_LSB: AsioSampleType = 17;
pub const ST_INT32_LSB: AsioSampleType = 18;
pub const ST_FLOAT32_LSB: AsioSampleType = 19;
pub const ST_FLOAT64_LSB: AsioSampleType = 20;
pub const ST_INT32_LSB16: AsioSampleType = 24;
pub const ST_INT32_LSB18: AsioSampleType = 25;
pub const ST_INT32_LSB20: AsioSampleType = 26;
pub const ST_INT32_LSB24: AsioSampleType = 27;

// asioMessage selectors (kAsio*).
pub const K_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ENGINE_VERSION: i32 = 2;
pub const K_RESET_REQUEST: i32 = 3;
pub const K_BUFFER_SIZE_CHANGE: i32 = 4;
pub const K_RESYNC_REQUEST: i32 = 5;
pub const K_LATENCIES_CHANGED: i32 = 6;
pub const K_SUPPORTS_TIME_INFO: i32 = 7;
pub const K_SUPPORTS_TIME_CODE: i32 = 8;
pub const K_OVERLOAD: i32 = 15;

// AsioTimeInfo flags.
pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 1 << 1;

/// 64-bit value as two 32-bit halves, most significant first.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AsioInt64 {
    pub hi: u32,
    pub lo: u32,
}

impl AsioInt64 {
    pub fn value(self) -> i64 {
        (((self.hi as u64) << 32) | self.lo as u64) as i64
    }

    pub fn from_value(v: i64) -> Self {
        let u = v as u64;
        Self { hi: (u >> 32) as u32, lo: u as u32 }
    }
}

pub type AsioSamples = AsioInt64;
pub type AsioTimeStamp = AsioInt64;

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeInfo {
    pub speed: f64,
    /// Nanoseconds, driver-defined epoch.
    pub system_time: AsioTimeStamp,
    pub sample_position: AsioSamples,
    pub sample_rate: f64,
    pub flags: u32,
    pub reserved: [u8; 12],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioSamples,
    pub flags: u32,
    pub future: [u8; 64],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: i32,
    pub sample_type: AsioSampleType,
    pub name: [u8; 32],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    pub is_input: AsioBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

/// Host callbacks handed to `createBuffers`. Plain C function pointers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    pub buffer_switch: extern "C" fn(double_buffer_index: i32, direct_process: AsioBool),
    pub sample_rate_did_change: extern "C" fn(rate: f64),
    pub asio_message: extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    pub buffer_switch_time_info:
        extern "C" fn(params: *mut AsioTime, double_buffer_index: i32, direct_process: AsioBool) -> *mut AsioTime,
}

/// A driver object: a C++ object whose first word is its vtable pointer.
#[repr(C)]
pub struct IAsio {
    pub vtbl: *const IAsioVtbl,
}

/// `IUnknown` followed by the 21 `IASIO` virtual methods, in declaration order.
/// On x64 every C++ member function uses the single Windows x64 convention.
#[repr(C)]
pub struct IAsioVtbl {
    pub query_interface: unsafe extern "system" fn(*mut IAsio, *const c_void, *mut *mut c_void) -> i32,
    pub add_ref: unsafe extern "system" fn(*mut IAsio) -> u32,
    pub release: unsafe extern "system" fn(*mut IAsio) -> u32,
    pub init: unsafe extern "system" fn(*mut IAsio, sys_handle: *mut c_void) -> AsioBool,
    pub get_driver_name: unsafe extern "system" fn(*mut IAsio, name: *mut u8),
    pub get_driver_version: unsafe extern "system" fn(*mut IAsio) -> i32,
    pub get_error_message: unsafe extern "system" fn(*mut IAsio, msg: *mut u8),
    pub start: unsafe extern "system" fn(*mut IAsio) -> AsioError,
    pub stop: unsafe extern "system" fn(*mut IAsio) -> AsioError,
    pub get_channels: unsafe extern "system" fn(*mut IAsio, inputs: *mut i32, outputs: *mut i32) -> AsioError,
    pub get_latencies: unsafe extern "system" fn(*mut IAsio, input: *mut i32, output: *mut i32) -> AsioError,
    pub get_buffer_size: unsafe extern "system" fn(
        *mut IAsio,
        min: *mut i32,
        max: *mut i32,
        preferred: *mut i32,
        granularity: *mut i32,
    ) -> AsioError,
    pub can_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: f64) -> AsioError,
    pub get_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: *mut f64) -> AsioError,
    pub set_sample_rate: unsafe extern "system" fn(*mut IAsio, rate: f64) -> AsioError,
    pub get_clock_sources: unsafe extern "system" fn(*mut IAsio, clocks: *mut c_void, count: *mut i32) -> AsioError,
    pub set_clock_source: unsafe extern "system" fn(*mut IAsio, reference: i32) -> AsioError,
    pub get_sample_position:
        unsafe extern "system" fn(*mut IAsio, pos: *mut AsioSamples, stamp: *mut AsioTimeStamp) -> AsioError,
    pub get_channel_info: unsafe extern "system" fn(*mut IAsio, info: *mut AsioChannelInfo) -> AsioError,
    pub create_buffers: unsafe extern "system" fn(
        *mut IAsio,
        infos: *mut AsioBufferInfo,
        count: i32,
        block: i32,
        callbacks: *const AsioCallbacks,
    ) -> AsioError,
    pub dispose_buffers: unsafe extern "system" fn(*mut IAsio) -> AsioError,
    pub control_panel: unsafe extern "system" fn(*mut IAsio) -> AsioError,
    pub future: unsafe extern "system" fn(*mut IAsio, selector: i32, opt: *mut c_void) -> AsioError,
    pub output_ready: unsafe extern "system" fn(*mut IAsio) -> AsioError,
}

/// Human-readable name for an `AsioError`.
pub fn error_name(e: AsioError) -> &'static str {
    match e {
        ASE_OK => "OK",
        ASE_SUCCESS => "SUCCESS",
        ASE_NOT_PRESENT => "NotPresent",
        ASE_HW_MALFUNCTION => "HWMalfunction",
        ASE_INVALID_PARAMETER => "InvalidParameter",
        ASE_INVALID_MODE => "InvalidMode",
        ASE_SP_NOT_ADVANCING => "SPNotAdvancing",
        ASE_NO_CLOCK => "NoClock",
        ASE_NO_MEMORY => "NoMemory",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn struct_layouts_match_the_sdk_with_pack_4() {
        assert_eq!(size_of::<AsioInt64>(), 8);
        assert_eq!(size_of::<AsioTimeInfo>(), 48);
        assert_eq!(offset_of!(AsioTimeInfo, system_time), 8);
        assert_eq!(offset_of!(AsioTimeInfo, sample_position), 16);
        assert_eq!(offset_of!(AsioTimeInfo, sample_rate), 24);
        assert_eq!(offset_of!(AsioTimeInfo, flags), 32);
        assert_eq!(size_of::<AsioTimeCode>(), 84);
        assert_eq!(size_of::<AsioTime>(), 148);
        assert_eq!(offset_of!(AsioTime, time_info), 16);
        assert_eq!(offset_of!(AsioTime, time_code), 64);
        assert_eq!(size_of::<AsioChannelInfo>(), 52);
        assert_eq!(offset_of!(AsioChannelInfo, sample_type), 16);
        assert_eq!(size_of::<AsioBufferInfo>(), 24);
        assert_eq!(offset_of!(AsioBufferInfo, buffers), 8);
        assert_eq!(size_of::<AsioCallbacks>(), 32);
        assert_eq!(size_of::<IAsioVtbl>(), 24 * size_of::<usize>(), "IUnknown (3) + IASIO (21)");
    }

    #[test]
    fn int64_halves_round_trip() {
        for v in [0i64, 1, 0xFFFF_FFFF, 0x1_0000_0000, 123_456_789_012_345, -1] {
            assert_eq!(AsioInt64::from_value(v).value(), v);
        }
        assert_eq!(AsioInt64 { hi: 1, lo: 2 }.value(), (1i64 << 32) + 2);
    }
}
