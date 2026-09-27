//! The `IASIO` object a DAW talks to. It is a *strict* driver (spec §7.4):
//! it offers exactly the engine's sample rate and block size and refuses
//! anything else. Control calls come from the DAW's thread; audio callbacks
//! come from this driver's own stream thread (see `stream`).

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use confluence_provider_asio::sys::*;
use confluence_provider_vasio::config::{self, InstanceConfig};
use confluence_shm::Client;
use windows::core::GUID;

use crate::stream::{self, Position, Stream};
use crate::{driver_name, LIVE};

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

/// The SDK's `ASIOClockSource`.
#[repr(C, packed(4))]
struct AsioClockSource {
    index: i32,
    associated_channel: i32,
    associated_group: i32,
    is_current_source: AsioBool,
    name: [u8; 32],
}

// `future` selectors (kAsio*).
const K_ASIO_CAN_TIME_INFO: i32 = 10;

#[repr(C)]
struct Driver {
    base: IAsio,
    refs: AtomicU32,
    instance: u32,
    /// Outside the lock: hosts read it from inside `bufferSwitch`.
    position: Arc<Position>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Fixed by `init` until the host re-initialises us.
    cfg: Option<InstanceConfig>,
    error: String,
    /// One `2 * block` buffer per channel the host asked for: (is_input, channel, samples).
    buffers: Vec<(bool, usize, Box<[f32]>)>,
    callbacks: Option<AsioCallbacks>,
    stream: Option<Stream>,
    /// A stream stopped from its own thread: signalled, joined later from another thread.
    stopping: Option<Stream>,
}

/// A new driver object with one reference.
pub(crate) fn create(instance: u32) -> *mut IAsio {
    LIVE.fetch_add(1, Ordering::AcqRel);
    let d = Box::new(Driver {
        base: IAsio { vtbl: &VTBL },
        refs: AtomicU32::new(1),
        instance,
        position: Arc::default(),
        inner: Mutex::new(Inner::default()),
    });
    Box::into_raw(d).cast()
}

fn me<'a>(this: *mut IAsio) -> &'a Driver {
    // SAFETY: only reached through VTBL, whose objects are `Driver`s.
    unsafe { &*this.cast::<Driver>() }
}

fn inner(this: *mut IAsio) -> MutexGuard<'static, Inner> {
    let d: &'static Driver = me(this);
    // A poisoned lock only means an earlier call panicked; the state is still usable.
    d.inner.lock().unwrap_or_else(|p| p.into_inner())
}

/// Stops the stream thread without holding the driver lock while waiting for
/// it: a host may call into the driver from inside `bufferSwitch`, and that
/// call must not wait on a thread that is waiting on it. Returns false if
/// called from the stream thread itself: the thread is only signalled, and is
/// joined later from another thread, so the buffers must stay alive for now.
fn halt(this: *mut IAsio) -> bool {
    let (running, stopping) = {
        let mut g = inner(this);
        (g.stream.take(), g.stopping.take())
    };
    let mut done = true;
    for s in [running, stopping].into_iter().flatten() {
        if s.is_current_thread() {
            s.signal();
            inner(this).stopping = Some(s);
            done = false;
        } else {
            s.stop();
        }
    }
    done
}

/// Runs an entry point body; a panic becomes `on_panic` instead of unwinding into the host.
fn guard<T>(on_panic: T, body: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(on_panic)
}

/// The shape to offer: the running engine's, else the last one it served, else defaults.
fn current_config(instance: u32) -> InstanceConfig {
    let name = confluence_provider_vasio::stream_name(instance);
    if let Ok(Some(c)) = Client::connect(&name) {
        return InstanceConfig::from_layout(&c.header().layout());
    }
    config::load(instance).unwrap_or_default()
}

fn write_cstr(dst: *mut u8, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    // SAFETY: the SDK guarantees `cap` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
}

unsafe extern "system" fn query_interface(this: *mut IAsio, riid: *const c_void, ppv: *mut *mut c_void) -> i32 {
    guard(0x8000_4003u32 as i32, || {
        if riid.is_null() || ppv.is_null() {
            return 0x8000_4003u32 as i32; // E_POINTER
        }
        // SAFETY: COM passes valid pointers.
        unsafe {
            let riid = &*riid.cast::<GUID>();
            if *riid == IID_IUNKNOWN || *riid == crate::clsid(me(this).instance) {
                me(this).refs.fetch_add(1, Ordering::AcqRel);
                *ppv = this.cast();
                0
            } else {
                *ppv = std::ptr::null_mut();
                0x8000_4002u32 as i32 // E_NOINTERFACE
            }
        }
    })
}

unsafe extern "system" fn add_ref(this: *mut IAsio) -> u32 {
    me(this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut IAsio) -> u32 {
    let left = me(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // Stop the stream thread before its buffers go away. Released from the
        // stream thread itself (a very odd host), the object is leaked instead.
        if guard(false, || halt(this)) {
            // SAFETY: last reference; created by Box::into_raw in `create`.
            drop(unsafe { Box::from_raw(this.cast::<Driver>()) });
        }
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
    left
}

unsafe extern "system" fn init(this: *mut IAsio, _sys_handle: *mut c_void) -> AsioBool {
    guard(ASIO_FALSE, || {
        // A second init without disposeBuffers: the old buffers may not fit
        // the new shape, so they go (the host must create buffers again).
        if !halt(this) {
            return ASIO_FALSE;
        }
        let cfg = current_config(me(this).instance);
        let mut g = inner(this);
        g.buffers.clear();
        g.callbacks = None;
        g.cfg = Some(cfg);
        g.error.clear();
        ASIO_TRUE
    })
}

unsafe extern "system" fn get_driver_name(this: *mut IAsio, name: *mut u8) {
    guard((), || write_cstr(name, 32, &driver_name(me(this).instance)))
}

unsafe extern "system" fn get_driver_version(_: *mut IAsio) -> i32 {
    1
}

unsafe extern "system" fn get_error_message(this: *mut IAsio, msg: *mut u8) {
    guard((), || write_cstr(msg, 124, &inner(this).error))
}

unsafe extern "system" fn start(this: *mut IAsio) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        if inner(this).stream.is_some() {
            return ASE_OK;
        }
        // Reap a stream stopped from its own callback before starting a new one.
        if !halt(this) {
            return ASE_INVALID_MODE;
        }
        let mut g = inner(this);
        let (Some(cfg), Some(callbacks)) = (g.cfg, g.callbacks) else { return ASE_INVALID_MODE };
        let block = cfg.block as usize;
        let mut inputs = vec![None; cfg.daw_inputs as usize];
        let mut outputs = vec![None; cfg.daw_outputs as usize];
        for (is_input, ch, buf) in g.buffers.iter_mut() {
            let slot = if *is_input { &mut inputs[*ch] } else { &mut outputs[*ch] };
            *slot = Some(stream::Buffer(buf.as_mut_ptr()));
        }
        let params = stream::Params {
            instance: me(this).instance,
            cfg,
            block,
            callbacks,
            inputs,
            outputs,
            position: me(this).position.clone(),
        };
        match Stream::start(params) {
            Ok(s) => {
                g.stream = Some(s);
                ASE_OK
            }
            Err(e) => {
                g.error = format!("could not start the stream thread: {e}");
                ASE_HW_MALFUNCTION
            }
        }
    })
}

unsafe extern "system" fn stop(this: *mut IAsio) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        halt(this);
        ASE_OK
    })
}

unsafe extern "system" fn get_channels(this: *mut IAsio, inputs: *mut i32, outputs: *mut i32) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let Some(cfg) = inner(this).cfg else { return ASE_NOT_PRESENT };
        // SAFETY: the host passes valid out-pointers.
        unsafe {
            *inputs = cfg.daw_inputs as i32;
            *outputs = cfg.daw_outputs as i32;
        }
        ASE_OK
    })
}

unsafe extern "system" fn get_latencies(this: *mut IAsio, input: *mut i32, output: *mut i32) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let Some(cfg) = inner(this).cfg else { return ASE_NOT_PRESENT };
        // One block each way through the engine (spec §7.4: two blocks round trip).
        // SAFETY: valid out-pointers.
        unsafe {
            *input = cfg.block as i32;
            *output = cfg.block as i32;
        }
        ASE_OK
    })
}

unsafe extern "system" fn get_buffer_size(
    this: *mut IAsio,
    min: *mut i32,
    max: *mut i32,
    preferred: *mut i32,
    granularity: *mut i32,
) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let Some(cfg) = inner(this).cfg else { return ASE_NOT_PRESENT };
        // Strict: the engine's block and nothing else.
        // SAFETY: valid out-pointers.
        unsafe {
            *min = cfg.block as i32;
            *max = cfg.block as i32;
            *preferred = cfg.block as i32;
            *granularity = 0;
        }
        ASE_OK
    })
}

fn rate_ok(this: *mut IAsio, rate: f64) -> AsioError {
    match inner(this).cfg {
        Some(cfg) if (rate - cfg.sample_rate as f64).abs() < 0.5 => ASE_OK,
        Some(_) => ASE_NO_CLOCK,
        None => ASE_NOT_PRESENT,
    }
}

unsafe extern "system" fn can_sample_rate(this: *mut IAsio, rate: f64) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || rate_ok(this, rate))
}

unsafe extern "system" fn get_sample_rate(this: *mut IAsio, rate: *mut f64) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let Some(cfg) = inner(this).cfg else { return ASE_NOT_PRESENT };
        // SAFETY: valid out-pointer.
        unsafe { *rate = cfg.sample_rate as f64 };
        ASE_OK
    })
}

unsafe extern "system" fn set_sample_rate(this: *mut IAsio, rate: f64) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || rate_ok(this, rate))
}

unsafe extern "system" fn get_clock_sources(_: *mut IAsio, clocks: *mut c_void, count: *mut i32) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        // SAFETY: the host passes an array of `*count` entries.
        unsafe {
            if *count < 1 {
                return ASE_INVALID_PARAMETER;
            }
            let c = clocks.cast::<AsioClockSource>();
            (*c).index = 0;
            (*c).associated_channel = -1;
            (*c).associated_group = -1;
            (*c).is_current_source = ASIO_TRUE;
            let mut name = [0u8; 32];
            name[..17].copy_from_slice(b"Confluence engine");
            (*c).name = name;
            *count = 1;
        }
        ASE_OK
    })
}

unsafe extern "system" fn set_clock_source(_: *mut IAsio, reference: i32) -> AsioError {
    if reference == 0 {
        ASE_OK
    } else {
        ASE_INVALID_PARAMETER
    }
}

unsafe extern "system" fn get_sample_position(
    this: *mut IAsio,
    pos: *mut AsioSamples,
    stamp: *mut AsioTimeStamp,
) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let p = &me(this).position;
        // SAFETY: valid out-pointers.
        unsafe {
            *pos = AsioSamples::from_value(p.samples.load(Ordering::Acquire));
            *stamp = AsioTimeStamp::from_value(p.nanos.load(Ordering::Acquire));
        }
        // The SDK: no advancing position while the stream is not running.
        if p.running.load(Ordering::Acquire) {
            ASE_OK
        } else {
            ASE_SP_NOT_ADVANCING
        }
    })
}

unsafe extern "system" fn get_channel_info(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let g = inner(this);
        let Some(cfg) = g.cfg else { return ASE_NOT_PRESENT };
        // SAFETY: the host passes a valid struct with `channel` and `is_input` set.
        let info = unsafe { &mut *info };
        let (ch, is_input) = (info.channel, info.is_input != ASIO_FALSE);
        let count = if is_input { cfg.daw_inputs } else { cfg.daw_outputs } as i32;
        if ch < 0 || ch >= count {
            return ASE_INVALID_PARAMETER;
        }
        let active = g.buffers.iter().any(|(i, c, _)| *i == is_input && *c == ch as usize);
        info.is_active = active as AsioBool;
        info.channel_group = 0;
        info.sample_type = ST_FLOAT32_LSB;
        let mut name = [0u8; 32];
        let label = format!("{} {}", if is_input { "In" } else { "Out" }, ch + 1);
        name[..label.len()].copy_from_slice(label.as_bytes());
        info.name = name;
        ASE_OK
    })
}

unsafe extern "system" fn create_buffers(
    this: *mut IAsio,
    infos: *mut AsioBufferInfo,
    count: i32,
    block: i32,
    callbacks: *const AsioCallbacks,
) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        if inner(this).stream.is_some() || !halt(this) {
            return ASE_INVALID_MODE;
        }
        let mut g = inner(this);
        let Some(cfg) = g.cfg else { return ASE_NOT_PRESENT };
        if block != cfg.block as i32 {
            g.error = format!("Confluence runs at a block of {} frames", cfg.block);
            return ASE_INVALID_MODE;
        }
        if count < 0 || infos.is_null() || callbacks.is_null() {
            return ASE_INVALID_PARAMETER;
        }
        // SAFETY: the host passes `count` entries and a callbacks struct.
        let (infos, callbacks) = unsafe { (std::slice::from_raw_parts_mut(infos, count as usize), *callbacks) };
        let mut buffers = Vec::with_capacity(infos.len());
        for info in infos.iter_mut() {
            let is_input = info.is_input != ASIO_FALSE;
            let ch = info.channel_num;
            let limit = if is_input { cfg.daw_inputs } else { cfg.daw_outputs } as i32;
            if ch < 0 || ch >= limit || buffers.iter().any(|(i, c, _)| *i == is_input && *c == ch as usize) {
                return ASE_INVALID_PARAMETER;
            }
            let mut buf = vec![0.0f32; 2 * block as usize].into_boxed_slice();
            let p = buf.as_mut_ptr();
            // SAFETY: both halves lie inside `buf`, which lives until disposeBuffers.
            info.buffers = unsafe { [p.cast(), p.add(block as usize).cast()] };
            buffers.push((is_input, ch as usize, buf));
        }
        g.buffers = buffers;
        g.callbacks = Some(callbacks);
        ASE_OK
    })
}

unsafe extern "system" fn dispose_buffers(this: *mut IAsio) -> AsioError {
    guard(ASE_HW_MALFUNCTION, || {
        let stopped = halt(this);
        let mut g = inner(this);
        g.callbacks = None;
        // Called from the stream thread itself, the buffers are still in use
        // until that thread is joined; they go with the next createBuffers or Release.
        if stopped {
            g.buffers.clear();
        }
        ASE_OK
    })
}

unsafe extern "system" fn control_panel(_: *mut IAsio) -> AsioError {
    // Configured from the Confluence app, not a driver panel.
    ASE_NOT_PRESENT
}

unsafe extern "system" fn future(_: *mut IAsio, selector: i32, _opt: *mut c_void) -> AsioError {
    if selector == K_ASIO_CAN_TIME_INFO {
        ASE_SUCCESS
    } else {
        ASE_INVALID_PARAMETER
    }
}

unsafe extern "system" fn output_ready(_: *mut IAsio) -> AsioError {
    // Outputs are taken when bufferSwitch returns.
    ASE_NOT_PRESENT
}

static VTBL: IAsioVtbl = IAsioVtbl {
    query_interface,
    add_ref,
    release,
    init,
    get_driver_name,
    get_driver_version,
    get_error_message,
    start,
    stop,
    get_channels,
    get_latencies,
    get_buffer_size,
    can_sample_rate,
    get_sample_rate,
    set_sample_rate,
    get_clock_sources,
    set_clock_source,
    get_sample_position,
    get_channel_info,
    create_buffers,
    dispose_buffers,
    control_panel,
    future,
    output_ready,
};
