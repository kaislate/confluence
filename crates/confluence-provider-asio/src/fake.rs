//! An in-process fake ASIO driver implementing the real `IASIO` vtable, so the
//! host (trampolines, control thread, buffer handling) is tested without
//! hardware. It streams on its own real-time thread like a driver would.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::SetTimer;

use crate::convert::{decode, encode, SampleFormat};
use crate::sys::*;

/// Behaviour of a fake driver.
#[derive(Clone, Debug)]
pub struct FakeConfig {
    pub name: String,
    pub inputs: usize,
    pub outputs: usize,
    pub sample_rate: f64,
    pub block: usize,
    pub sample_type: AsioSampleType,
    /// Constant value written to every input sample.
    pub input_value: f32,
    pub supports_output_ready: bool,
    /// Use `bufferSwitchTimeInfo` when the host supports it.
    pub use_time_info: bool,
    pub fail_init: bool,
    /// Skip delivering every Nth buffer (the position still advances).
    pub skip_every: Option<u64>,
    /// Send `kAsioResetRequest` after this many callbacks.
    pub reset_after: Option<u64>,
    /// Like some real drivers, set a thread timer during `init`: it only fires
    /// if the host pumps window messages on the driver's thread.
    pub posts_timer: bool,
    pub probe: Arc<FakeProbe>,
}

impl FakeConfig {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            inputs: 2,
            outputs: 2,
            sample_rate: 48_000.0,
            block: 128,
            sample_type: ST_INT32_LSB,
            input_value: 0.25,
            supports_output_ready: true,
            use_time_info: true,
            fail_init: false,
            skip_every: None,
            reset_after: None,
            posts_timer: false,
            probe: Arc::new(FakeProbe::default()),
        }
    }
}

/// What the fake observed, for test assertions.
#[derive(Default, Debug)]
pub struct FakeProbe {
    pub switches: AtomicU64,
    pub output_ready_calls: AtomicU64,
    /// Output channel 0 of the most recent block, decoded.
    pub last_output: Mutex<Vec<f32>>,
    pub released: AtomicBool,
    /// Timer messages dispatched on the driver's thread (see `posts_timer`).
    pub timer_ticks: AtomicU64,
}

thread_local! {
    /// The probe of the fake whose timer fires on this thread.
    static TIMER_PROBE: std::cell::RefCell<Option<Arc<FakeProbe>>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn timer_tick(_: HWND, _: u32, _: usize, _: u32) {
    TIMER_PROBE.with(|p| {
        if let Some(p) = p.borrow().as_ref() {
            p.timer_ticks.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[repr(C)]
struct Fake {
    base: IAsio,
    refs: AtomicU32,
    cfg: FakeConfig,
    position: Arc<AtomicI64>,
    run: Arc<AtomicBool>,
    /// Changed by control calls; the callback thread never touches it.
    state: Mutex<FakeState>,
}

struct FakeState {
    rate: f64,
    callbacks: Option<AsioCallbacks>,
    /// Inputs then outputs, two halves each.
    buffers: Vec<[Vec<u8>; 2]>,
    block: usize,
    thread: Option<JoinHandle<()>>,
}

/// Creates a fake driver object with one reference.
pub(crate) fn create(cfg: FakeConfig) -> *mut IAsio {
    let rate = cfg.sample_rate;
    let fake = Box::new(Fake {
        base: IAsio { vtbl: &FAKE_VTBL },
        refs: AtomicU32::new(1),
        cfg,
        position: Arc::new(AtomicI64::new(0)),
        run: Arc::new(AtomicBool::new(false)),
        state: Mutex::new(FakeState { rate, callbacks: None, buffers: Vec::new(), block: 0, thread: None }),
    });
    Box::into_raw(fake).cast()
}

fn me<'a>(this: *mut IAsio) -> &'a Fake {
    // SAFETY: only called through FAKE_VTBL, whose objects are `Fake`s. Shared
    // access only: the host calls in from its control and callback threads.
    unsafe { &*this.cast::<Fake>() }
}

fn state(this: *mut IAsio) -> std::sync::MutexGuard<'static, FakeState> {
    let f: &'static Fake = me(this);
    f.state.lock().unwrap_or_else(|p| p.into_inner())
}

fn format(cfg: &FakeConfig) -> SampleFormat {
    SampleFormat::from_asio(cfg.sample_type).unwrap_or(SampleFormat::F32)
}

unsafe extern "system" fn qi(_: *mut IAsio, _: *const c_void, out: *mut *mut c_void) -> i32 {
    // SAFETY: caller passes a valid out-pointer.
    unsafe { *out = std::ptr::null_mut() };
    0x8000_4002u32 as i32 // E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut IAsio) -> u32 {
    me(this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut IAsio) -> u32 {
    let left = me(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: last reference; created by Box::into_raw in `create`.
        let fake = unsafe { Box::from_raw(this.cast::<Fake>()) };
        fake.cfg.probe.released.store(true, Ordering::Release);
    }
    left
}

unsafe extern "system" fn init(this: *mut IAsio, _: *mut c_void) -> AsioBool {
    let f = me(this);
    if f.cfg.posts_timer {
        TIMER_PROBE.with(|p| *p.borrow_mut() = Some(f.cfg.probe.clone()));
        // SAFETY: a thread timer with a valid callback; it lives as long as this thread.
        unsafe { SetTimer(None, 0, 20, Some(timer_tick)) };
    }
    (!f.cfg.fail_init) as AsioBool
}

unsafe extern "system" fn get_driver_name(this: *mut IAsio, name: *mut u8) {
    let src = me(this).cfg.name.as_bytes();
    let n = src.len().min(31);
    // SAFETY: the SDK guarantees a 32-byte buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), name, n);
        *name.add(n) = 0;
    }
}

unsafe extern "system" fn get_driver_version(_: *mut IAsio) -> i32 {
    1
}

unsafe extern "system" fn get_error_message(_: *mut IAsio, msg: *mut u8) {
    let text = b"fake driver asked to fail\0";
    // SAFETY: the SDK guarantees a 124-byte buffer.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), msg, text.len()) };
}

unsafe extern "system" fn start(this: *mut IAsio) -> AsioError {
    let f = me(this);
    let mut st = state(this);
    let Some(cb) = st.callbacks else { return ASE_INVALID_MODE };
    let fmt = format(&f.cfg);
    let bps = fmt.bytes_per_sample();
    let ptrs: Vec<[usize; 2]> = st.buffers.iter().map(|b| [b[0].as_ptr() as usize, b[1].as_ptr() as usize]).collect();
    let (block, rate, cfg) = (st.block, st.rate, f.cfg.clone());
    let (run, position) = (f.run.clone(), f.position.clone());
    run.store(true, Ordering::Release);
    let time_info = cfg.use_time_info
        && (cb.asio_message)(K_SELECTOR_SUPPORTED, K_SUPPORTS_TIME_INFO, std::ptr::null_mut(), std::ptr::null_mut())
            == 1;
    st.thread = Some(std::thread::spawn(move || {
        let period = Duration::from_secs_f64(block as f64 / rate);
        let mut next = Instant::now() + period;
        let (mut half, mut n) = (0usize, 0u64);
        let input = vec![cfg.input_value; block];
        let mut out = vec![0f32; block];
        while run.load(Ordering::Acquire) {
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            next += period;
            n += 1;
            if cfg.skip_every.is_some_and(|k| n % k == 0) {
                position.fetch_add(block as i64, Ordering::AcqRel);
                continue;
            }
            for p in &ptrs[..cfg.inputs] {
                // SAFETY: buffers live until disposeBuffers, which follows stop (which joins us).
                let bytes = unsafe { std::slice::from_raw_parts_mut(p[half] as *mut u8, block * bps) };
                encode(fmt, &input, bytes);
            }
            let pos = position.load(Ordering::Acquire);
            if time_info {
                // SAFETY: plain-old-data struct.
                let mut t: AsioTime = unsafe { std::mem::zeroed() };
                t.time_info.sample_position = AsioInt64::from_value(pos);
                t.time_info.sample_rate = rate;
                t.time_info.flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID;
                (cb.buffer_switch_time_info)(&mut t, half as i32, ASIO_TRUE);
            } else {
                (cb.buffer_switch)(half as i32, ASIO_TRUE);
            }
            cfg.probe.switches.fetch_add(1, Ordering::Relaxed);
            if let Some(p) = ptrs.get(cfg.inputs) {
                // SAFETY: as above.
                let bytes = unsafe { std::slice::from_raw_parts(p[half] as *const u8, block * bps) };
                decode(fmt, bytes, &mut out);
                if let Ok(mut last) = cfg.probe.last_output.lock() {
                    last.clear();
                    last.extend_from_slice(&out);
                }
            }
            if cfg.reset_after == Some(n) {
                (cb.asio_message)(K_RESET_REQUEST, 0, std::ptr::null_mut(), std::ptr::null_mut());
            }
            position.fetch_add(block as i64, Ordering::AcqRel);
            half ^= 1;
        }
    }));
    ASE_OK
}

unsafe extern "system" fn stop(this: *mut IAsio) -> AsioError {
    me(this).run.store(false, Ordering::Release);
    let thread = state(this).thread.take();
    if let Some(t) = thread {
        let _ = t.join();
    }
    ASE_OK
}

unsafe extern "system" fn get_channels(this: *mut IAsio, i: *mut i32, o: *mut i32) -> AsioError {
    let f = me(this);
    // SAFETY: valid out-pointers.
    unsafe {
        *i = f.cfg.inputs as i32;
        *o = f.cfg.outputs as i32;
    }
    ASE_OK
}

unsafe extern "system" fn get_latencies(this: *mut IAsio, i: *mut i32, o: *mut i32) -> AsioError {
    let b = me(this).cfg.block as i32;
    // SAFETY: valid out-pointers.
    unsafe {
        *i = b;
        *o = b;
    }
    ASE_OK
}

unsafe extern "system" fn get_buffer_size(
    this: *mut IAsio,
    min: *mut i32,
    max: *mut i32,
    pref: *mut i32,
    gran: *mut i32,
) -> AsioError {
    // SAFETY: valid out-pointers.
    unsafe {
        *min = 32;
        *max = 2048;
        *pref = me(this).cfg.block as i32;
        *gran = -1;
    }
    ASE_OK
}

unsafe extern "system" fn can_sample_rate(_: *mut IAsio, rate: f64) -> AsioError {
    if [44_100.0, 48_000.0, 96_000.0].contains(&rate) {
        ASE_OK
    } else {
        ASE_NO_CLOCK
    }
}

unsafe extern "system" fn get_sample_rate(this: *mut IAsio, rate: *mut f64) -> AsioError {
    // SAFETY: valid out-pointer.
    unsafe { *rate = state(this).rate };
    ASE_OK
}

unsafe extern "system" fn set_sample_rate(this: *mut IAsio, rate: f64) -> AsioError {
    state(this).rate = rate;
    ASE_OK
}

unsafe extern "system" fn get_clock_sources(_: *mut IAsio, _: *mut c_void, count: *mut i32) -> AsioError {
    // SAFETY: valid out-pointer.
    unsafe { *count = 0 };
    ASE_OK
}

unsafe extern "system" fn set_clock_source(_: *mut IAsio, _: i32) -> AsioError {
    ASE_OK
}

unsafe extern "system" fn get_sample_position(
    this: *mut IAsio,
    pos: *mut AsioSamples,
    stamp: *mut AsioTimeStamp,
) -> AsioError {
    // SAFETY: valid out-pointers.
    unsafe {
        *pos = AsioInt64::from_value(me(this).position.load(Ordering::Acquire));
        *stamp = AsioInt64::default();
    }
    ASE_OK
}

unsafe extern "system" fn get_channel_info(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioError {
    let f = me(this);
    // SAFETY: valid in/out pointer.
    let ci = unsafe { &mut *info };
    let count = if ci.is_input != 0 { f.cfg.inputs } else { f.cfg.outputs };
    if ci.channel < 0 || ci.channel as usize >= count {
        return ASE_INVALID_PARAMETER;
    }
    ci.is_active = ASIO_FALSE;
    ci.channel_group = 0;
    ci.sample_type = f.cfg.sample_type;
    let label = format!("Fake {} {}", if ci.is_input != 0 { "In" } else { "Out" }, ci.channel + 1);
    ci.name = [0; 32];
    ci.name[..label.len().min(31)].copy_from_slice(&label.as_bytes()[..label.len().min(31)]);
    ASE_OK
}

unsafe extern "system" fn create_buffers(
    this: *mut IAsio,
    infos: *mut AsioBufferInfo,
    count: i32,
    block: i32,
    callbacks: *const AsioCallbacks,
) -> AsioError {
    let bytes = block as usize * format(&me(this).cfg).bytes_per_sample();
    // SAFETY: the host passes `count` infos and a valid callbacks table.
    let (infos, cb) = unsafe { (std::slice::from_raw_parts_mut(infos, count as usize), *callbacks) };
    let mut st = state(this);
    st.callbacks = Some(cb);
    st.block = block as usize;
    st.buffers = infos.iter().map(|_| [vec![0u8; bytes], vec![0u8; bytes]]).collect();
    for (bi, b) in infos.iter_mut().zip(st.buffers.iter_mut()) {
        bi.buffers = [b[0].as_mut_ptr().cast(), b[1].as_mut_ptr().cast()];
    }
    ASE_OK
}

unsafe extern "system" fn dispose_buffers(this: *mut IAsio) -> AsioError {
    let mut st = state(this);
    st.buffers.clear();
    st.callbacks = None;
    ASE_OK
}

unsafe extern "system" fn control_panel(_: *mut IAsio) -> AsioError {
    ASE_OK
}

unsafe extern "system" fn future(_: *mut IAsio, _: i32, _: *mut c_void) -> AsioError {
    ASE_NOT_PRESENT
}

unsafe extern "system" fn output_ready(this: *mut IAsio) -> AsioError {
    let f = me(this);
    f.cfg.probe.output_ready_calls.fetch_add(1, Ordering::Relaxed);
    if f.cfg.supports_output_ready {
        ASE_OK
    } else {
        ASE_NOT_PRESENT
    }
}

static FAKE_VTBL: IAsioVtbl = IAsioVtbl {
    query_interface: qi,
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
