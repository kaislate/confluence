//! Attaching to the driver: a dedicated thread opens the control device and
//! keeps the attach request pending. The request must not be issued from a
//! short-lived thread, because Windows cancels a thread's pending I/O when the
//! thread exits. Dropping the Attachment cancels the request and waits until
//! it completes, and only then can the region be freed.

use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PATH_NOT_FOUND, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use crate::abi::{IOCTL_ATTACH, USER_PATH};
use crate::region::Region;
use crate::{VaioError, VaioStats};

/// How long to wait for the driver to accept the region.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for SendHandle {}

pub struct Attachment {
    device: SendHandle,
    thread: Option<JoinHandle<()>>,
}

impl Attachment {
    pub fn start(region: Arc<Region>, stats: Arc<VaioStats>) -> Result<Attachment, VaioError> {
        // SAFETY: valid path; the handle is closed by the thread.
        let device = unsafe {
            CreateFileW(
                &HSTRING::from(USER_PATH),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| match e.code() {
            c if c == ERROR_FILE_NOT_FOUND.to_hresult() || c == ERROR_PATH_NOT_FOUND.to_hresult() => {
                VaioError::NotInstalled
            }
            c if c == ERROR_ACCESS_DENIED.to_hresult() => VaioError::InUse,
            _ => VaioError::Io(e.to_string()),
        })?;
        let device = SendHandle(device);
        let (tx, rx) = sync_channel::<Result<(), VaioError>>(1);
        let thread = std::thread::Builder::new()
            .name("confluence-vaio-attach".into())
            .spawn(move || run(device, region, stats, tx))
            .map_err(|e| {
                // SAFETY: we opened it and nothing else has it.
                let _ = unsafe { CloseHandle(device.0) };
                VaioError::Io(e.to_string())
            })?;
        let attachment = Attachment { device, thread: Some(thread) };
        match rx.recv_timeout(ATTACH_TIMEOUT) {
            Ok(Ok(())) => Ok(attachment),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(VaioError::Io("the VAIO driver did not answer".into())),
        }
    }
}

fn run(
    device: SendHandle,
    region: Arc<Region>,
    stats: Arc<VaioStats>,
    tx: std::sync::mpsc::SyncSender<Result<(), VaioError>>,
) {
    let result = (|| -> Result<(), VaioError> {
        // SAFETY: plain event creation.
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| VaioError::Io(e.to_string()))?;
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        let len = u32::try_from(region.len()).map_err(|_| VaioError::Io("ring too large".into()))?;
        // SAFETY: the region outlives the request: this thread holds an Arc
        // until GetOverlappedResult below has returned.
        let issued = unsafe {
            DeviceIoControl(
                device.0,
                IOCTL_ATTACH,
                None,
                0,
                Some(region.as_mut_ptr().cast()),
                len,
                None,
                Some(&mut ov as *mut OVERLAPPED),
            )
        };
        match issued {
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(e) => return Err(VaioError::Io(e.to_string())),
            Ok(()) => return Err(VaioError::Rejected), // completed at once: refused
        }
        if region.header().attached.load(Ordering::Acquire) != 1 {
            return Err(VaioError::Rejected);
        }
        stats.attached.store(true, Ordering::Relaxed);
        let _ = tx.send(Ok(()));
        let mut n = 0u32;
        // Blocks until the driver lets go: cancelled by Drop, device removal, …
        // SAFETY: `ov` and the region are still alive.
        let _ = unsafe { GetOverlappedResult(device.0, &ov, &mut n, true) };
        stats.attached.store(false, Ordering::Relaxed);
        // SAFETY: created above.
        let _ = unsafe { CloseHandle(event) };
        Ok(())
    })();
    if let Err(e) = result {
        let _ = tx.send(Err(e));
    }
    // The device handle is closed by `Drop`, after the join: closing it here
    // would let `Drop`'s CancelIoEx hit an unrelated handle that reused the value.
    drop(region);
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // SAFETY: the handle stays open until after the join below (the thread
        // never closes it). If the request already completed (e.g. the device
        // was removed), the cancel finds nothing and the join returns at once.
        let _ = unsafe { CancelIoEx(self.device.0, None) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: opened in `start`; the thread has exited.
        let _ = unsafe { CloseHandle(self.device.0) };
    }
}
