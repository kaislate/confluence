//! One hosted ASIO driver: a dedicated STA control thread owns the driver
//! object and makes every control call; audio callbacks arrive on the driver's
//! own thread through the trampolines (spec §7.1).

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use confluence_rt::ComApartment;
use windows::core::{GUID, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT,
};

use crate::convert::SampleFormat;
use crate::io::Channel;
use crate::registry::{find_driver, DriverEntry};
use crate::sys::*;
use crate::trampolines::{self, AsioHealth, SlotState, CALLBACKS, MAX_DRIVERS};
use crate::AsioCallback;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AsioHostError {
    #[error("ASIO driver '{0}' is not installed")]
    NotInstalled(String),
    #[error("could not create the driver object: {0}")]
    Create(String),
    #[error("the driver failed to initialise: {0}")]
    Init(String),
    #[error("{call} failed: {code}")]
    Call { call: &'static str, code: &'static str },
    #[error("sample rate {0} Hz is not supported by the driver")]
    Rate(f64),
    #[error("a block of {block} frames is not supported (min {min}, max {max}, granularity {granularity})")]
    Block { block: usize, min: i32, max: i32, granularity: i32 },
    #[error("channel {channel} uses unsupported ASIO sample type {sample_type}")]
    Format { channel: String, sample_type: i32 },
    #[error("all {0} ASIO driver slots are in use")]
    TooManyDrivers(usize),
    #[error("the stream is already running")]
    AlreadyRunning,
    #[error("the driver's control thread has stopped")]
    Gone,
    #[error("registry: {0}")]
    Registry(String),
}

/// What the driver reports after `init`.
#[derive(Clone, Debug, PartialEq)]
pub struct DriverInfo {
    pub name: String,
    pub version: i32,
    pub input_names: Vec<String>,
    pub output_names: Vec<String>,
    /// Per channel; `None` = a format this host does not support.
    pub input_formats: Vec<Option<SampleFormat>>,
    pub output_formats: Vec<Option<SampleFormat>>,
    /// Per channel, the ASIO sample type the driver reported (-1 if it did not say).
    pub input_sample_types: Vec<AsioSampleType>,
    pub output_sample_types: Vec<AsioSampleType>,
    pub min_block: i32,
    pub max_block: i32,
    pub preferred_block: i32,
    pub granularity: i32,
    pub sample_rate: f64,
}

impl DriverInfo {
    pub fn inputs(&self) -> usize {
        self.input_names.len()
    }

    pub fn outputs(&self) -> usize {
        self.output_names.len()
    }
}

/// Requested stream parameters; `None` keeps the driver's current setting.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamConfig {
    pub sample_rate: Option<f64>,
    pub block: Option<usize>,
}

/// The running stream's actual parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamInfo {
    pub sample_rate: f64,
    pub block: usize,
    pub input_latency: i32,
    pub output_latency: i32,
    /// True when the driver supports `outputReady` (lower output latency).
    pub post_output: bool,
}

/// Where the driver object comes from.
pub enum DriverSource {
    Installed(DriverEntry),
    /// An in-process fake driver (tests without hardware).
    Fake(crate::fake::FakeConfig),
    /// A driver's COM class factory called directly: the same path a host
    /// takes through `CoCreateInstance`, minus the registry lookup (tests of
    /// our own drivers, e.g. VASIO, without registering them).
    ClassFactory {
        get_class_object: GetClassObject,
        clsid: GUID,
        name: String,
    },
}

/// Signature of an in-proc COM server's `DllGetClassObject`.
pub type GetClassObject = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;

/// `IClassFactory`'s vtable (after `IUnknown`).
#[repr(C)]
struct ClassFactoryVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create_instance: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> i32,
    lock_server: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);

enum Ctl {
    Start(StreamConfig, Box<dyn AsioCallback>, fn() -> f64, SyncSender<Result<StreamInfo, AsioHostError>>),
    Stop(SyncSender<()>),
    Close,
}

/// An auto-reset event that wakes the control thread when a request is sent.
struct Wake(HANDLE);

// SAFETY: kernel event handles may be signalled and waited on from any thread.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: created by us, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A hosted ASIO driver. Dropping it stops the stream and releases the driver.
pub struct AsioDevice {
    tx: Sender<Ctl>,
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
    info: DriverInfo,
    health: Arc<AsioHealth>,
    running: bool,
}

impl AsioDevice {
    /// Loads and initialises a driver on a new STA control thread.
    pub fn open(source: DriverSource) -> Result<Self, AsioHostError> {
        let (init_tx, init_rx) = sync_channel(1);
        let (tx, rx) = channel();
        // SAFETY: plain unnamed auto-reset event.
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| AsioHostError::Create(format!("CreateEvent: {}", e.message())))?;
        let wake = Arc::new(Wake(event));
        let thread_wake = wake.clone();
        let health = Arc::new(AsioHealth::default());
        let thread_health = health.clone();
        let thread = std::thread::Builder::new()
            .name("confluence-asio-control".into())
            .spawn(move || control(source, init_tx, rx, thread_wake, thread_health))
            .map_err(|e| AsioHostError::Create(e.to_string()))?;
        match init_rx.recv() {
            Ok(Ok(info)) => Ok(Self { tx, wake, thread: Some(thread), info, health, running: false }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(AsioHostError::Gone)
            }
        }
    }

    /// Opens an installed driver by its registry name (e.g. "MOTU Gen 5").
    pub fn open_installed(name: &str) -> Result<Self, AsioHostError> {
        let entry = find_driver(name)
            .map_err(|e| AsioHostError::Registry(e.to_string()))?
            .ok_or_else(|| AsioHostError::NotInstalled(name.to_string()))?;
        Self::open(DriverSource::Installed(entry))
    }

    pub fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// Stream counters (callbacks, faults, gaps, driver requests).
    pub fn health(&self) -> Arc<AsioHealth> {
        self.health.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Starts streaming; `callback` runs on the driver's thread once per block,
    /// with timestamps from `confluence_rt::now_seconds`.
    pub fn start(&mut self, cfg: StreamConfig, callback: Box<dyn AsioCallback>) -> Result<StreamInfo, AsioHostError> {
        self.start_with_clock(cfg, callback, confluence_rt::now_seconds)
    }

    /// As [`start`](Self::start) with an explicit time base (tests).
    pub fn start_with_clock(
        &mut self,
        cfg: StreamConfig,
        callback: Box<dyn AsioCallback>,
        clock: fn() -> f64,
    ) -> Result<StreamInfo, AsioHostError> {
        let (reply, result) = sync_channel(1);
        self.send(Ctl::Start(cfg, callback, clock, reply)).map_err(|_| AsioHostError::Gone)?;
        let info = result.recv().map_err(|_| AsioHostError::Gone)??;
        self.running = true;
        Ok(info)
    }

    /// Hands a request to the control thread and wakes it.
    fn send(&self, ctl: Ctl) -> Result<(), ()> {
        self.tx.send(ctl).map_err(|_| ())?;
        // SAFETY: live event.
        let _ = unsafe { SetEvent(self.wake.0) };
        Ok(())
    }

    /// Stops streaming and disposes the driver's buffers; the callback is dropped here.
    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        let (reply, done) = sync_channel(1);
        if self.send(Ctl::Stop(reply)).is_ok() {
            let _ = done.recv();
        }
        self.running = false;
    }
}

impl Drop for AsioDevice {
    fn drop(&mut self) {
        self.stop();
        let _ = self.send(Ctl::Close);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Owned reference to a driver object.
struct Driver(*mut IAsio);

impl Driver {
    fn vt(&self) -> &IAsioVtbl {
        // SAFETY: a live driver object always starts with its vtable pointer.
        unsafe { &*(*self.0).vtbl }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: we hold one reference from creation.
        unsafe { (self.vt().release)(self.0) };
    }
}

struct Stream {
    slot: usize,
    state: *mut SlotState,
}

#[link(name = "ole32")]
extern "system" {
    fn CoCreateInstance(
        rclsid: *const GUID,
        outer: *mut c_void,
        context: u32,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT;
}
const CLSCTX_INPROC_SERVER: u32 = 1;

fn create(source: &DriverSource) -> Result<Driver, AsioHostError> {
    match source {
        DriverSource::Installed(entry) => {
            let mut p: *mut c_void = std::ptr::null_mut();
            // ASIO convention: the interface id requested is the driver's own CLSID.
            // SAFETY: valid GUID pointers and out-pointer.
            let hr = unsafe {
                CoCreateInstance(&entry.clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &entry.clsid, &mut p)
            };
            if hr.is_err() || p.is_null() {
                return Err(AsioHostError::Create(format!("{} ({})", entry.name, hr.message())));
            }
            Ok(Driver(p.cast()))
        }
        DriverSource::Fake(cfg) => Ok(Driver(crate::fake::create(cfg.clone()))),
        DriverSource::ClassFactory { get_class_object, clsid, name } => {
            let failed = |what: &str, hr: i32| AsioHostError::Create(format!("{name}: {what} failed (0x{hr:08x})"));
            let mut factory: *mut c_void = std::ptr::null_mut();
            // SAFETY: valid GUID pointers and out-pointer, as DllGetClassObject requires.
            let hr = unsafe { get_class_object(clsid, &IID_ICLASS_FACTORY, &mut factory) };
            if hr < 0 || factory.is_null() {
                return Err(failed("DllGetClassObject", hr));
            }
            // SAFETY: a COM object starts with its vtable pointer; this one is an IClassFactory.
            let vt = unsafe { &**factory.cast::<*const ClassFactoryVtbl>() };
            let mut p: *mut c_void = std::ptr::null_mut();
            // ASIO convention: the interface id requested is the driver's own CLSID.
            // SAFETY: live factory; valid pointers.
            let hr = unsafe { (vt.create_instance)(factory, std::ptr::null_mut(), clsid, &mut p) };
            // SAFETY: we own the reference DllGetClassObject returned.
            unsafe { (vt.release)(factory) };
            if hr < 0 || p.is_null() {
                return Err(failed("CreateInstance", hr));
            }
            Ok(Driver(p.cast()))
        }
    }
}

fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn check(call: &'static str, r: AsioError) -> Result<(), AsioHostError> {
    if r == ASE_OK {
        Ok(())
    } else {
        Err(AsioHostError::Call { call, code: error_name(r) })
    }
}

fn init_and_query(d: &Driver) -> Result<DriverInfo, AsioHostError> {
    let vt = d.vt();
    // SAFETY (whole block): every call passes the live driver and valid out-pointers.
    unsafe {
        // The SDK specifies an application window as the system reference on Windows.
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow();
        if (vt.init)(d.0, hwnd.0) == ASIO_FALSE {
            let mut msg = [0u8; 124];
            (vt.get_error_message)(d.0, msg.as_mut_ptr());
            let reason = cstr(&msg);
            let reason = if reason.trim().is_empty() {
                "the driver gave no reason; is the hardware connected and not held by another application?".to_string()
            } else {
                reason
            };
            return Err(AsioHostError::Init(reason));
        }
        let mut name = [0u8; 32];
        (vt.get_driver_name)(d.0, name.as_mut_ptr());
        let version = (vt.get_driver_version)(d.0);
        let (mut ins, mut outs) = (0i32, 0i32);
        check("getChannels", (vt.get_channels)(d.0, &mut ins, &mut outs))?;
        let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
        check("getBufferSize", (vt.get_buffer_size)(d.0, &mut min, &mut max, &mut pref, &mut gran))?;
        let mut rate = 0f64;
        check("getSampleRate", (vt.get_sample_rate)(d.0, &mut rate))?;
        let channels = |is_input: bool, count: i32| {
            (0..count)
                .map(|ch| {
                    let mut ci = AsioChannelInfo {
                        channel: ch,
                        is_input: is_input as AsioBool,
                        is_active: 0,
                        channel_group: 0,
                        sample_type: -1,
                        name: [0; 32],
                    };
                    let r = (vt.get_channel_info)(d.0, &mut ci);
                    let fmt = if r == ASE_OK { SampleFormat::from_asio(ci.sample_type) } else { None };
                    let label = cstr(&ci.name);
                    let label = if label.is_empty() {
                        format!("{} {}", if is_input { "In" } else { "Out" }, ch + 1)
                    } else {
                        label
                    };
                    (label, fmt, ci.sample_type)
                })
                .collect::<Vec<_>>()
        };
        let inputs = channels(true, ins);
        let outputs = channels(false, outs);
        Ok(DriverInfo {
            name: cstr(&name),
            version,
            input_names: inputs.iter().map(|c| c.0.clone()).collect(),
            output_names: outputs.iter().map(|c| c.0.clone()).collect(),
            input_formats: inputs.iter().map(|c| c.1).collect(),
            output_formats: outputs.iter().map(|c| c.1).collect(),
            input_sample_types: inputs.iter().map(|c| c.2).collect(),
            output_sample_types: outputs.iter().map(|c| c.2).collect(),
            min_block: min,
            max_block: max,
            preferred_block: pref,
            granularity: gran,
            sample_rate: rate,
        })
    }
}

fn block_ok(block: usize, info: &DriverInfo) -> bool {
    let b = block as i32;
    if b < info.min_block || b > info.max_block {
        return false;
    }
    match info.granularity {
        -1 => block.is_power_of_two(),
        g if g > 0 => (b - info.min_block) % g == 0,
        _ => b == info.preferred_block || info.min_block == info.max_block,
    }
}

fn start_stream(
    d: &Driver,
    info: &DriverInfo,
    cfg: StreamConfig,
    callback: Box<dyn AsioCallback>,
    clock: fn() -> f64,
    health: &Arc<AsioHealth>,
) -> Result<(Stream, StreamInfo), AsioHostError> {
    let vt = d.vt();
    if let Some(rate) = cfg.sample_rate {
        if (rate - info.sample_rate).abs() > 0.5 {
            // SAFETY: live driver.
            unsafe {
                if (vt.can_sample_rate)(d.0, rate) != ASE_OK {
                    return Err(AsioHostError::Rate(rate));
                }
                check("setSampleRate", (vt.set_sample_rate)(d.0, rate))?;
            }
        }
    }
    let block = cfg.block.unwrap_or(info.preferred_block.max(1) as usize);
    if !block_ok(block, info) {
        return Err(AsioHostError::Block {
            block,
            min: info.min_block,
            max: info.max_block,
            granularity: info.granularity,
        });
    }
    let formats = |names: &[String], fmts: &[Option<SampleFormat>], types: &[AsioSampleType]| {
        names
            .iter()
            .zip(fmts)
            .zip(types)
            .map(|((n, f), &t)| f.ok_or_else(|| AsioHostError::Format { channel: n.clone(), sample_type: t }))
            .collect::<Result<Vec<SampleFormat>, AsioHostError>>()
    };
    let in_formats = formats(&info.input_names, &info.input_formats, &info.input_sample_types)?;
    let out_formats = formats(&info.output_names, &info.output_formats, &info.output_sample_types)?;

    let slot = trampolines::claim().ok_or(AsioHostError::TooManyDrivers(MAX_DRIVERS))?;
    let null = [std::ptr::null_mut(); 2];
    let state = Box::into_raw(Box::new(SlotState {
        driver: d.0,
        block,
        inputs: UnsafeCell::new(in_formats.iter().map(|&format| Channel { buffers: null, format }).collect()),
        outputs: UnsafeCell::new(out_formats.iter().map(|&format| Channel { buffers: null, format }).collect()),
        callback: UnsafeCell::new(callback),
        post_output: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        last_position: AtomicI64::new(i64::MIN),
        consecutive_faults: AtomicU32::new(0),
        clock,
        health: health.clone(),
    }));
    // Published before createBuffers: drivers may send asioMessage from inside it.
    trampolines::publish(slot, state);
    let abandon = |state: *mut SlotState| retire(slot, state);

    let mut infos: Vec<AsioBufferInfo> = (0..in_formats.len())
        .map(|ch| AsioBufferInfo {
            is_input: ASIO_TRUE,
            channel_num: ch as i32,
            buffers: null.map(|p| p as *mut c_void),
        })
        .chain((0..out_formats.len()).map(|ch| AsioBufferInfo {
            is_input: ASIO_FALSE,
            channel_num: ch as i32,
            buffers: null.map(|p| p as *mut c_void),
        }))
        .collect();
    // SAFETY: `infos` holds `len` entries; the callbacks table is 'static.
    let r = unsafe { (vt.create_buffers)(d.0, infos.as_mut_ptr(), infos.len() as i32, block as i32, &CALLBACKS[slot]) };
    if let Err(e) = check("createBuffers", r) {
        abandon(state);
        return Err(e);
    }
    // SAFETY: not ready yet, so only this thread touches the buffer lists.
    unsafe {
        let (ins, outs) = (&mut *(*state).inputs.get(), &mut *(*state).outputs.get());
        for (c, bi) in ins.iter_mut().chain(outs.iter_mut()).zip(&infos) {
            c.buffers = bi.buffers.map(|p| p.cast());
        }
    }
    // SDK: probe outputReady once; ASE_OK means the host should call it after each switch.
    // SAFETY: live driver; `state` is ours until `ready`.
    let (post, (in_lat, out_lat)) = unsafe {
        let post = (vt.output_ready)(d.0) == ASE_OK;
        let (mut il, mut ol) = (0i32, 0i32);
        let lat = if (vt.get_latencies)(d.0, &mut il, &mut ol) == ASE_OK { (il, ol) } else { (0, 0) };
        (*state).post_output.store(post, Ordering::Relaxed);
        (*state).ready.store(true, Ordering::Release);
        (post, lat)
    };
    // SAFETY: live driver.
    if let Err(e) = check("start", unsafe { (vt.start)(d.0) }) {
        // SAFETY: as in stop_stream.
        unsafe { (*state).ready.store(false, Ordering::SeqCst) };
        trampolines::wait_idle(slot, DRAIN_TIMEOUT);
        // SAFETY: live driver.
        unsafe { (vt.dispose_buffers)(d.0) };
        abandon(state);
        return Err(e);
    }
    let mut rate = 0f64;
    // SAFETY: live driver.
    unsafe { (vt.get_sample_rate)(d.0, &mut rate) };
    let info =
        StreamInfo { sample_rate: rate, block, input_latency: in_lat, output_latency: out_lat, post_output: post };
    Ok((Stream { slot, state }, info))
}

/// How long teardown waits for callbacks still inside a slot.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Withdraws a slot's state and frees it once no callback can reach it. If a
/// callback is stuck inside, the state and the slot are leaked instead: a
/// bounded leak beats a use-after-free on the driver's thread.
fn retire(slot: usize, state: *mut SlotState) {
    if trampolines::withdraw(slot, DRAIN_TIMEOUT) {
        trampolines::release(slot);
        // SAFETY: withdrawn and drained, so no entry point can reach it.
        drop(unsafe { Box::from_raw(state) });
    }
}

fn stop_stream(d: &Driver, s: Stream) {
    let vt = d.vt();
    // SAFETY: live driver. Callbacks entering after `ready` is cleared (SeqCst,
    // paired with `dispatch`) skip the buffers, so they may be disposed once
    // those already inside have left.
    unsafe {
        (vt.stop)(d.0);
        (*s.state).ready.store(false, Ordering::SeqCst);
        trampolines::wait_idle(s.slot, DRAIN_TIMEOUT);
        (vt.dispose_buffers)(d.0);
    }
    retire(s.slot, s.state);
}

/// Waits for the next request while dispatching window messages, as an STA
/// thread must: some drivers post messages or set timers on the thread that
/// created them and stall if nobody pumps. Returns `None` once the device is gone.
fn next_request(rx: &Receiver<Ctl>, wake: &Wake) -> Option<Ctl> {
    loop {
        match rx.try_recv() {
            Ok(msg) => return Some(msg),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => {}
        }
        // SAFETY: a live event handle; a timeout bounds the wait even if a wake is missed.
        unsafe { MsgWaitForMultipleObjects(Some(&[wake.0]), false, 1_000, QS_ALLINPUT) };
        let mut msg = MSG::default();
        // SAFETY: standard message pump for this thread's queue.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn control(
    source: DriverSource,
    init: SyncSender<Result<DriverInfo, AsioHostError>>,
    rx: Receiver<Ctl>,
    wake: Arc<Wake>,
    health: Arc<AsioHealth>,
) {
    let _com = match ComApartment::single_threaded() {
        Ok(c) => c,
        Err(e) => {
            let _ = init.send(Err(AsioHostError::Create(format!("COM: {e}"))));
            return;
        }
    };
    let driver = match create(&source) {
        Ok(d) => d,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let info = match init_and_query(&driver) {
        Ok(i) => i,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    if init.send(Ok(info.clone())).is_err() {
        return;
    }
    let mut stream: Option<Stream> = None;
    while let Some(msg) = next_request(&rx, &wake) {
        match msg {
            Ctl::Start(cfg, cb, clock, reply) => {
                let result = if stream.is_some() {
                    Err(AsioHostError::AlreadyRunning)
                } else {
                    start_stream(&driver, &info, cfg, cb, clock, &health).map(|(s, i)| {
                        stream = Some(s);
                        i
                    })
                };
                let _ = reply.send(result);
            }
            Ctl::Stop(reply) => {
                if let Some(s) = stream.take() {
                    stop_stream(&driver, s);
                }
                let _ = reply.send(());
            }
            Ctl::Close => break,
        }
    }
    if let Some(s) = stream.take() {
        stop_stream(&driver, s);
    }
}
