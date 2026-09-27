//! Event-driven shared-mode streams: endpoint render/capture and per-app
//! capture (process loopback). Samples are 32-bit float, interleaved.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;

use confluence_rt::{enable_flush_denormals, now_seconds, ComApartment, ProAudioThread};
use windows::core::{implement, Interface, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoTaskMemFree, IAgileObject, IAgileObject_Impl, BLOB, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use crate::endpoints::{device_by_id, Direction};
use crate::{Context, WasapiError};

/// What to open.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Endpoint {
        id: String,
        direction: Direction,
    },
    /// Capture everything an application (and its child processes) plays.
    App {
        pid: u32,
    },
}

/// The stream's negotiated format (known before streaming starts).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamFormat {
    pub direction: Direction,
    pub channels: usize,
    pub sample_rate: f64,
    /// Typical frames per callback (the device period).
    pub period_frames: usize,
}

/// Per-callback audio handler, run on the stream's MMCSS thread. Must be
/// real-time safe. Buffers are interleaved `f32`.
pub enum Handler {
    Render(RenderFn),
    Capture(CaptureFn),
}

/// Fills an interleaved output buffer; the second argument is the engine time.
pub type RenderFn = Box<dyn FnMut(&mut [f32], f64) + Send>;
/// Consumes an interleaved input buffer; the second argument is the engine time.
pub type CaptureFn = Box<dyn FnMut(&[f32], f64) + Send>;

#[derive(Default, Debug)]
pub struct StreamHealth {
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
    /// Set when the device disappeared (unplugged, disabled, format change).
    pub lost: AtomicBool,
    /// Callbacks whose handler panicked (a render buffer is then silent).
    pub faults: AtomicU64,
}

/// Runs a render handler; a panic plays silence and counts a fault instead of
/// ending the stream thread with the WASAPI buffer still held.
fn render_guarded(handler: &mut RenderFn, buf: &mut [f32], now: f64, health: &StreamHealth) {
    if catch_unwind(AssertUnwindSafe(|| handler(buf, now))).is_err() {
        buf.fill(0.0);
        health.faults.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs a capture handler; a panic counts a fault.
fn capture_guarded(handler: &mut CaptureFn, buf: &[f32], now: f64, health: &StreamHealth) {
    if catch_unwind(AssertUnwindSafe(|| handler(buf, now))).is_err() {
        health.faults.fetch_add(1, Ordering::Relaxed);
    }
}

/// A WASAPI stream on its own thread. Dropping it stops the stream.
pub struct WasapiStream {
    format: StreamFormat,
    start: Option<SyncSender<Handler>>,
    stop: Arc<AtomicBool>,
    wake: Event,
    health: Arc<StreamHealth>,
    thread: Option<JoinHandle<()>>,
}

/// An auto-reset event handle shared with the stream thread.
#[derive(Clone, Copy)]
struct Event(HANDLE);

// SAFETY: kernel event handles may be signalled and waited on from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl WasapiStream {
    /// Opens and initialises the stream (not yet running).
    pub fn open(target: Target) -> Result<Self, WasapiError> {
        // SAFETY: plain event creation; closed in Drop.
        let wake = Event(unsafe { CreateEventW(None, false, false, None) }.call("CreateEventW")?);
        let (fmt_tx, fmt_rx) = sync_channel(1);
        let (start_tx, start_rx) = sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(StreamHealth::default());
        let thread = {
            let (stop, health) = (stop.clone(), health.clone());
            std::thread::Builder::new()
                .name("confluence-wasapi".into())
                .spawn(move || stream_thread(target, fmt_tx, start_rx, stop, wake, health))
                .map_err(|_| WasapiError::Gone)?
        };
        let mut s = Self {
            format: StreamFormat::placeholder(),
            start: Some(start_tx),
            stop,
            wake,
            health,
            thread: Some(thread),
        };
        match fmt_rx.recv() {
            Ok(Ok(format)) => {
                s.format = format;
                Ok(s)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(WasapiError::Gone),
        }
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }

    pub fn health(&self) -> Arc<StreamHealth> {
        self.health.clone()
    }

    /// Starts streaming with `handler` (which must match the stream direction).
    pub fn start(&mut self, handler: Handler) -> Result<(), WasapiError> {
        let tx = self.start.take().ok_or(WasapiError::AlreadyStarted)?;
        tx.send(handler).map_err(|_| WasapiError::Gone)
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.start.take();
        // SAFETY: valid event.
        let _ = unsafe { SetEvent(self.wake.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the thread has exited; nothing else uses the handle.
        let _ = unsafe { CloseHandle(self.wake.0) };
    }
}

impl StreamFormat {
    fn placeholder() -> Self {
        Self { direction: Direction::Render, channels: 0, sample_rate: 0.0, period_frames: 0 }
    }
}

fn float_format(channels: u16, rate: u32, mask: u32) -> WAVEFORMATEXTENSIBLE {
    let block_align = channels * 4;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// An event handle closed when the last owner drops it.
struct OwnedEvent(HANDLE);

// SAFETY: kernel event handles may be signalled and waited on from any thread.
unsafe impl Send for OwnedEvent {}
unsafe impl Sync for OwnedEvent {}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        // SAFETY: created by us and closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Completion handler for process-loopback activation. Must be agile. It
/// shares the event with the waiting thread, so a completion that arrives
/// after the wait has timed out still signals a live handle.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(Arc<OwnedEvent>);

impl IAgileObject_Impl for Activated_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: the event lives as long as this handler holds it.
        unsafe { SetEvent(self.0 .0) }
    }
}

/// Process-loopback activation (asynchronous by API design; we wait for it).
///
/// The activation parameters (the `PROPVARIANT` and the blob it points to) are
/// deliberately leaked, 36 bytes per app capture opened. Measured on Windows 11
/// with the opt-in loopback tests: freeing them right after activation, or
/// only after the stream thread and its COM apartment are gone, corrupts the
/// heap (STATUS_HEAP_CORRUPTION, 5 runs out of 5 each), while leaking them is
/// clean (5 of 5). Windows evidently keeps or frees that memory itself; which,
/// is undocumented, so we never free or reuse it.
fn activate_app(pid: u32) -> Result<IAudioClient, WasapiError> {
    let params: &'static mut AUDIOCLIENT_ACTIVATION_PARAMS = Box::leak(Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    }));
    let pv: &'static mut PROPVARIANT = Box::leak(Box::new(PROPVARIANT::default()));
    // SAFETY: the blob points at the leaked (never freed) params.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
        };
    }
    // SAFETY: plain event creation, closed below.
    let done = Arc::new(OwnedEvent(unsafe { CreateEventW(None, false, false, None) }.call("CreateEventW")?));
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done.clone()).into();
    (|| {
        // SAFETY: all arguments stay valid for the duration of the call and beyond.
        let op = unsafe {
            ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)
        }
        .call("ActivateAudioInterfaceAsync")?;
        // SAFETY: valid event.
        if unsafe { WaitForSingleObject(done.0, 5_000) } != WAIT_OBJECT_0 {
            return Err(WasapiError::Timeout);
        }
        let mut hr = HRESULT(0);
        let mut unknown = None;
        // SAFETY: valid out-pointers.
        unsafe { op.GetActivateResult(&mut hr, &mut unknown) }.call("GetActivateResult")?;
        hr.ok().call("process loopback activation")?;
        unknown.ok_or(WasapiError::Gone)?.cast::<IAudioClient>().call("IUnknown::cast<IAudioClient>")
    })()
}

fn open_client(target: &Target) -> Result<(IAudioClient, StreamFormat), WasapiError> {
    const BUFFER_HNS: i64 = 200_000; // 20 ms
    match target {
        Target::Endpoint { id, direction } => {
            let device = device_by_id(id)?;
            // SAFETY: valid COM objects; the mix format is freed here.
            unsafe {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).call("IMMDevice::Activate")?;
                let mix = client.GetMixFormat().call("IAudioClient::GetMixFormat")?;
                let (channels, rate) = ((*mix).nChannels, (*mix).nSamplesPerSec);
                let mask = if (*mix).wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
                    (*mix.cast::<WAVEFORMATEXTENSIBLE>()).dwChannelMask
                } else {
                    0
                };
                CoTaskMemFree(Some(mix as *const _));
                let fmt = float_format(channels, rate, mask);
                let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &fmt.Format, None)
                    .call("IAudioClient::Initialize")?;
                let mut period = 0i64;
                client.GetDevicePeriod(Some(&mut period), None).call("IAudioClient::GetDevicePeriod")?;
                let period_frames = ((period as f64 * rate as f64 / 1e7).round() as usize).max(1);
                Ok((
                    client,
                    StreamFormat {
                        direction: *direction,
                        channels: channels as usize,
                        sample_rate: rate as f64,
                        period_frames,
                    },
                ))
            }
        }
        Target::App { pid } => {
            let client = activate_app(*pid)?;
            let (channels, rate) = (2u16, 48_000u32);
            let fmt = float_format(channels, rate, 0x3);
            let flags =
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
            // SAFETY: valid client and format.
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &fmt.Format, None) }
                .call("IAudioClient::Initialize(loopback)")?;
            Ok((
                client,
                StreamFormat { direction: Direction::Capture, channels: 2, sample_rate: 48_000.0, period_frames: 480 },
            ))
        }
    }
}

fn stream_thread(
    target: Target,
    fmt_tx: SyncSender<Result<StreamFormat, WasapiError>>,
    start_rx: Receiver<Handler>,
    stop: Arc<AtomicBool>,
    wake: Event,
    health: Arc<StreamHealth>,
) {
    let _com = match ComApartment::multi_threaded().call("CoInitializeEx") {
        Ok(c) => c,
        Err(e) => {
            let _ = fmt_tx.send(Err(e));
            return;
        }
    };
    let opened = open_client(&target).and_then(|(client, format)| {
        // SAFETY: valid client and event.
        unsafe { client.SetEventHandle(wake.0) }.call("IAudioClient::SetEventHandle")?;
        Ok((client, format))
    });
    let (client, format) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = fmt_tx.send(Err(e));
            return;
        }
    };
    if fmt_tx.send(Ok(format)).is_err() {
        return;
    }
    let Ok(handler) = start_rx.recv() else { return };
    let _mmcss = ProAudioThread::enter().ok();
    enable_flush_denormals();
    let result = match handler {
        Handler::Render(h) => run_render(&client, format, h, &stop, wake, &health),
        Handler::Capture(h) => run_capture(&client, format, h, &stop, wake, &health),
    };
    if let Err(e) = result {
        if e == AUDCLNT_E_DEVICE_INVALIDATED {
            health.lost.store(true, Ordering::Release);
        }
    }
    // SAFETY: valid client.
    let _ = unsafe { client.Stop() };
}

fn run_render(
    client: &IAudioClient,
    format: StreamFormat,
    mut handler: RenderFn,
    stop: &AtomicBool,
    wake: Event,
    health: &StreamHealth,
) -> Result<(), HRESULT> {
    let ch = format.channels;
    // SAFETY (whole function): valid COM objects; buffers are used only between
    // GetBuffer and ReleaseBuffer and sized by the frame counts WASAPI returns.
    unsafe {
        let render: IAudioRenderClient = client.GetService().map_err(|e| e.code())?;
        let size = client.GetBufferSize().map_err(|e| e.code())?;
        let p = render.GetBuffer(size).map_err(|e| e.code())?;
        std::ptr::write_bytes(p, 0, size as usize * ch * 4);
        render.ReleaseBuffer(size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(|e| e.code())?;
        client.Start().map_err(|e| e.code())?;
        while !stop.load(Ordering::Acquire) {
            WaitForSingleObject(wake.0, 2_000);
            if stop.load(Ordering::Acquire) {
                break;
            }
            let padding = client.GetCurrentPadding().map_err(|e| e.code())?;
            let avail = size.saturating_sub(padding);
            if avail == 0 {
                continue;
            }
            let p = render.GetBuffer(avail).map_err(|e| e.code())?;
            let buf = std::slice::from_raw_parts_mut(p.cast::<f32>(), avail as usize * ch);
            render_guarded(&mut handler, buf, now_seconds(), health);
            render.ReleaseBuffer(avail, 0).map_err(|e| e.code())?;
            health.callbacks.fetch_add(1, Ordering::Relaxed);
            health.frames.fetch_add(avail as u64, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn run_capture(
    client: &IAudioClient,
    format: StreamFormat,
    mut handler: CaptureFn,
    stop: &AtomicBool,
    wake: Event,
    health: &StreamHealth,
) -> Result<(), HRESULT> {
    let ch = format.channels;
    // SAFETY: as in run_render.
    unsafe {
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.code())?;
        let size = client.GetBufferSize().map_err(|e| e.code())?;
        let silence = vec![0f32; size.max(4096) as usize * ch];
        client.Start().map_err(|e| e.code())?;
        while !stop.load(Ordering::Acquire) {
            WaitForSingleObject(wake.0, 2_000);
            if stop.load(Ordering::Acquire) {
                break;
            }
            loop {
                let next = capture.GetNextPacketSize().map_err(|e| e.code())?;
                if next == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| e.code())?;
                let n = frames as usize * ch;
                // Arrival time, not capture time: the bridge's continuous fill
                // model needs to know when frames became available to it.
                let now = now_seconds();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    capture_guarded(&mut handler, &silence[..n.min(silence.len())], now, health);
                } else {
                    capture_guarded(&mut handler, std::slice::from_raw_parts(data.cast::<f32>(), n), now, health);
                }
                capture.ReleaseBuffer(frames).map_err(|e| e.code())?;
                health.callbacks.fetch_add(1, Ordering::Relaxed);
                health.frames.fetch_add(frames as u64, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_format_is_consistent() {
        let f = float_format(2, 48_000, 3);
        // Copy fields out: the struct is packed, so no references to its fields.
        let (align, avg, extra, sub) = (f.Format.nBlockAlign, f.Format.nAvgBytesPerSec, f.Format.cbSize, f.SubFormat);
        assert_eq!(align, 8);
        assert_eq!(avg, 384_000);
        assert_eq!(extra, 22);
        assert_eq!(sub, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }

    #[test]
    fn a_panicking_handler_is_contained_silenced_and_counted() {
        let health = StreamHealth::default();
        let mut render: RenderFn = Box::new(|buf: &mut [f32], _| {
            buf.fill(0.9);
            panic!("handler bug");
        });
        let mut buf = vec![0.5f32; 8];
        render_guarded(&mut render, &mut buf, 0.0, &health);
        assert!(buf.iter().all(|&s| s == 0.0), "a faulting render plays silence, not half a block");
        let mut capture: CaptureFn = Box::new(|_: &[f32], _| panic!("handler bug"));
        capture_guarded(&mut capture, &buf, 0.0, &health);
        assert_eq!(health.faults.load(Ordering::Relaxed), 2);
        // A healthy handler is untouched.
        let mut ok: RenderFn = Box::new(|buf: &mut [f32], _| buf.fill(0.25));
        render_guarded(&mut ok, &mut buf, 0.0, &health);
        assert!(buf.iter().all(|&s| s == 0.25));
        assert_eq!(health.faults.load(Ordering::Relaxed), 2);
    }
}
