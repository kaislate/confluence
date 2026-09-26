//! Audio endpoint enumeration.

use confluence_rt::ComApartment;
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::{Context, WasapiError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Render,
    Capture,
}

impl Direction {
    pub(crate) fn flow(self) -> EDataFlow {
        match self {
            Direction::Render => eRender,
            Direction::Capture => eCapture,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Direction::Render => "playback",
            Direction::Capture => "recording",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// Stable device id (`{0.0.0.00000000}.{guid}`).
    pub id: String,
    pub name: String,
    pub direction: Direction,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Runs `f` on a fresh MTA thread (callers may be in an STA or have no COM).
fn on_mta<T: Send + 'static>(f: impl FnOnce() -> Result<T, WasapiError> + Send + 'static) -> Result<T, WasapiError> {
    std::thread::spawn(move || {
        let _com = ComApartment::multi_threaded().call("CoInitializeEx")?;
        f()
    })
    .join()
    .map_err(|_| WasapiError::Gone)?
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, WasapiError> {
    // SAFETY: COM is initialised on this thread by the caller.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.call("CoCreateInstance(MMDeviceEnumerator)")
}

pub(crate) fn describe(device: &IMMDevice, direction: Direction) -> Result<Endpoint, WasapiError> {
    // SAFETY: valid COM objects; returned strings and formats are freed here.
    unsafe {
        let id_ptr = device.GetId().call("IMMDevice::GetId")?;
        let id = id_ptr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id_ptr.0 as *const _));

        let store = device.OpenPropertyStore(STGM_READ).call("IMMDevice::OpenPropertyStore")?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).call("IPropertyStore::GetValue")?;
        let inner = &value.Anonymous.Anonymous;
        let name =
            if inner.vt == VT_LPWSTR { inner.Anonymous.pwszVal.to_string().unwrap_or_default() } else { id.clone() };

        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).call("IMMDevice::Activate")?;
        let fmt = client.GetMixFormat().call("IAudioClient::GetMixFormat")?;
        let (channels, sample_rate) = ((*fmt).nChannels, (*fmt).nSamplesPerSec);
        CoTaskMemFree(Some(fmt as *const _));
        Ok(Endpoint { id, name, direction, channels, sample_rate })
    }
}

/// Active endpoints in one direction.
pub fn endpoints(direction: Direction) -> Result<Vec<Endpoint>, WasapiError> {
    on_mta(move || {
        let e = enumerator()?;
        // SAFETY: valid enumerator.
        unsafe {
            let all = e.EnumAudioEndpoints(direction.flow(), DEVICE_STATE_ACTIVE).call("EnumAudioEndpoints")?;
            let count = all.GetCount().call("IMMDeviceCollection::GetCount")?;
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let device = all.Item(i).call("IMMDeviceCollection::Item")?;
                // An endpoint that disappears mid-enumeration is simply skipped.
                if let Ok(ep) = describe(&device, direction) {
                    out.push(ep);
                }
            }
            Ok(out)
        }
    })
}

/// The system default endpoint in one direction.
pub fn default_endpoint(direction: Direction) -> Result<Endpoint, WasapiError> {
    on_mta(move || {
        let e = enumerator()?;
        // SAFETY: valid enumerator.
        let device =
            unsafe { e.GetDefaultAudioEndpoint(direction.flow(), eConsole) }.call("GetDefaultAudioEndpoint")?;
        describe(&device, direction)
    })
}

/// Finds an active endpoint by exact friendly name or id.
pub fn find_endpoint(direction: Direction, name_or_id: &str) -> Result<Endpoint, WasapiError> {
    endpoints(direction)?
        .into_iter()
        .find(|e| e.name == name_or_id || e.id == name_or_id)
        .ok_or_else(|| WasapiError::NoSuchEndpoint(direction.label(), name_or_id.to_string()))
}

/// Opens an endpoint by id on the calling (COM-initialised) thread.
pub(crate) fn device_by_id(id: &str) -> Result<IMMDevice, WasapiError> {
    let e = enumerator()?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated id.
    unsafe { e.GetDevice(PCWSTR(wide.as_ptr())) }.call("IMMDeviceEnumerator::GetDevice")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_works_with_or_without_devices() {
        for dir in [Direction::Render, Direction::Capture] {
            for ep in endpoints(dir).unwrap() {
                assert!(!ep.id.is_empty());
                assert!(ep.channels > 0 && ep.sample_rate > 0, "{ep:?}");
                assert_eq!(ep.direction, dir);
            }
        }
        assert!(matches!(
            find_endpoint(Direction::Render, "no such endpoint name"),
            Err(WasapiError::NoSuchEndpoint("playback", _))
        ));
    }
}
