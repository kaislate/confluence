//! VASIO as a DAW sees it: our own ASIO host loads the driver through its COM
//! class factory (as `CoCreateInstance` would) and streams, with and without
//! a running engine side.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_core::buffer::PlanarBuffer;
use confluence_provider_asio::{AsioCallback, AsioDevice, AsioHostError, AsioIo, DriverSource, StreamConfig};
use confluence_provider_vasio::VasioSlot;

fn open(instance: u32) -> AsioDevice {
    AsioDevice::open(DriverSource::ClassFactory {
        get_class_object: confluence_vasio::DllGetClassObject,
        clsid: confluence_vasio::clsid(instance),
        name: confluence_vasio::driver_name(instance),
    })
    .unwrap()
}

/// What the "DAW" observed.
#[derive(Default)]
struct Seen {
    callbacks: AtomicU64,
    last_input: Mutex<f32>,
}

/// A DAW that monitors input 1 and plays input 1 + 0.5 on output 1.
fn loopback_daw(seen: Arc<Seen>) -> Box<dyn AsioCallback> {
    let mut buf = Vec::new();
    Box::new(move |io: &mut AsioIo<'_>| {
        buf.resize(io.frames(), 0.0);
        io.read_input(0, &mut buf);
        *seen.last_input.lock().unwrap() = buf[0];
        for s in buf.iter_mut() {
            *s += 0.5;
        }
        io.write_output(0, &buf);
        seen.callbacks.fetch_add(1, Ordering::Relaxed);
    })
}

/// The engine side of `instance` running at real time on its own thread:
/// sends `send` on DAW input 1 every block and records what DAW output 1 returns.
struct Engine {
    run: Arc<AtomicBool>,
    /// Stop without shutting the stream down (like a crash).
    crash: Arc<AtomicBool>,
    /// While set, the engine runs no blocks (a stall).
    paused: Arc<AtomicBool>,
    heard: Arc<Mutex<f32>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    fn start(instance: u32, rate: f64, block: usize, send: f32) -> Engine {
        let mut slot = VasioSlot::open(instance, 2, 2, rate, block).unwrap();
        let (run, heard) = (Arc::new(AtomicBool::new(true)), Arc::new(Mutex::new(0.0)));
        let crash = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let (r, h, c, pz) = (run.clone(), heard.clone(), crash.clone(), paused.clone());
        let thread = std::thread::spawn(move || {
            let period = Duration::from_secs_f64(block as f64 / rate);
            let (mut ins, mut outs) = (PlanarBuffer::new(2, block), PlanarBuffer::new(2, block));
            outs.channel_mut(0).fill(send);
            let mut next = Instant::now();
            while r.load(Ordering::Acquire) {
                if pz.load(Ordering::Acquire) {
                    std::thread::sleep(period);
                    next = Instant::now();
                    continue;
                }
                slot.receive(&mut ins, 0);
                *h.lock().unwrap() = ins.channel(0)[0];
                slot.send(&outs, 0);
                next += period;
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            if c.load(Ordering::Acquire) {
                // A crashed engine never marks its stream closed.
                std::mem::forget(slot);
            }
        });
        Engine { run, crash, paused, heard, thread: Some(thread) }
    }

    /// Runs no blocks for `d` (the engine stalls), then carries on.
    fn stall(&self, d: Duration) {
        self.paused.store(true, Ordering::Release);
        std::thread::sleep(d);
        self.paused.store(false, Ordering::Release);
    }

    fn crash(self) {
        self.crash.store(true, Ordering::Release);
    }

    fn heard(&self) -> f32 {
        *self.heard.lock().unwrap()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.run.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn without_the_engine_the_daw_keeps_running_on_silence() {
    confluence_provider_vasio::isolate_for_tests();
    let mut dev = open(5);
    assert_eq!(dev.info().name, "Confluence VASIO 5");
    assert!(dev.info().inputs() >= 2 && dev.info().outputs() >= 2);
    let seen = Arc::new(Seen::default());
    let stream = dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    let expected = stream.sample_rate / stream.block as f64;
    let got = seen.callbacks.load(Ordering::Relaxed) as f64;
    assert!((got / expected - 1.0).abs() < 0.25, "timer-paced callbacks: {got} in 1 s, expected ≈{expected}");
    assert_eq!(*seen.last_input.lock().unwrap(), 0.0, "silence while the engine is absent");
    dev.stop();
}

#[test]
fn audio_goes_through_the_engine_and_back() {
    confluence_provider_vasio::isolate_for_tests();
    let engine = Engine::start(6, 48_000.0, 256, 0.25);
    let mut dev = open(6);
    assert_eq!((dev.info().sample_rate, dev.info().preferred_block), (48_000.0, 256), "the engine's shape");
    let seen = Arc::new(Seen::default());
    let stream = dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    assert_eq!(stream.block, 256);
    wait_until("the DAW to hear the engine", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.25);
    wait_until("the engine to hear the DAW", Duration::from_secs(3), || engine.heard() == 0.75);
    dev.stop();
}

#[test]
fn only_the_engines_rate_and_block_are_accepted() {
    confluence_provider_vasio::isolate_for_tests();
    let _engine = Engine::start(7, 48_000.0, 256, 0.0);
    let mut dev = open(7);
    let seen = Arc::new(Seen::default());
    let wrong_rate = StreamConfig { sample_rate: Some(44_100.0), block: None };
    assert!(matches!(dev.start(wrong_rate, loopback_daw(seen.clone())), Err(AsioHostError::Rate(_))));
    let wrong_block = StreamConfig { sample_rate: None, block: Some(128) };
    assert!(matches!(dev.start(wrong_block, loopback_daw(seen.clone())), Err(AsioHostError::Block { .. })));
    assert!(dev.start(StreamConfig::default(), loopback_daw(seen)).is_ok());
    dev.stop();
}

#[test]
fn an_engine_restart_is_survived_and_the_audio_comes_back() {
    confluence_provider_vasio::isolate_for_tests();
    let engine = Engine::start(8, 48_000.0, 256, 0.25);
    let mut dev = open(8);
    let seen = Arc::new(Seen::default());
    dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    wait_until("audio", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.25);
    drop(engine);
    let before = seen.callbacks.load(Ordering::Relaxed);
    wait_until("silence after the engine stops", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.0);
    std::thread::sleep(Duration::from_millis(500));
    assert!(seen.callbacks.load(Ordering::Relaxed) > before + 50, "the DAW keeps getting callbacks");
    let engine = Engine::start(8, 48_000.0, 256, 0.125);
    wait_until("audio from the restarted engine", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.125);
    wait_until("the restarted engine to hear the DAW", Duration::from_secs(3), || engine.heard() == 0.625);
    dev.stop();
}

/// The built `confluence_vasio.dll` itself (its own copy of the code and
/// statics, loaded the way a DAW loads it), not the test's linked copy.
#[test]
fn the_built_dll_streams_like_the_linked_code() {
    confluence_provider_vasio::isolate_for_tests();
    use windows::core::{s, HSTRING};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/daw-*.exe -> target/<profile>/confluence_vasio.dll
    let dll = exe.parent().unwrap().parent().unwrap().join("confluence_vasio.dll");
    assert!(dll.exists(), "{} is missing: run `cargo build -p confluence-vasio` first", dll.display());
    // SAFETY: loading our own DLL; it is never unloaded during the test.
    let module = unsafe { LoadLibraryW(&HSTRING::from(dll.as_os_str())) }.unwrap();
    // SAFETY: the export has exactly the GetClassObject signature.
    let get_class_object: confluence_provider_asio::GetClassObject =
        unsafe { std::mem::transmute(GetProcAddress(module, s!("DllGetClassObject")).unwrap()) };
    let engine = Engine::start(2, 48_000.0, 256, 0.25);
    let mut dev = AsioDevice::open(DriverSource::ClassFactory {
        get_class_object,
        clsid: confluence_vasio::clsid(2),
        name: "dll".into(),
    })
    .unwrap();
    assert_eq!(dev.info().name, "Confluence VASIO 2");
    let seen = Arc::new(Seen::default());
    dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    wait_until("the DAW to hear the engine", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.25);
    wait_until("the engine to hear the DAW", Duration::from_secs(3), || engine.heard() == 0.75);
    dev.stop();
}

#[test]
fn a_second_daw_on_the_same_instance_gets_silence_until_the_first_stops() {
    confluence_provider_vasio::isolate_for_tests();
    let engine = Engine::start(3, 48_000.0, 256, 0.25);
    let (mut first, mut second) = (open(3), open(3));
    let (a, b) = (Arc::new(Seen::default()), Arc::new(Seen::default()));
    first.start(StreamConfig::default(), loopback_daw(a.clone())).unwrap();
    wait_until("the first DAW's audio", Duration::from_secs(3), || *a.last_input.lock().unwrap() == 0.25);
    second.start(StreamConfig::default(), loopback_daw(b.clone())).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(*b.last_input.lock().unwrap(), 0.0, "the second DAW does not share the first one's stream");
    assert!(b.callbacks.load(Ordering::Relaxed) > 50, "but it keeps running");
    assert_eq!(*a.last_input.lock().unwrap(), 0.25, "the first DAW's audio is untouched");
    assert_eq!(engine.heard(), 0.75);
    // An engine stall must not hand the instance to the waiting DAW.
    engine.stall(Duration::from_millis(1500));
    wait_until("the first DAW's audio after a stall", Duration::from_secs(3), || *a.last_input.lock().unwrap() == 0.25);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(*b.last_input.lock().unwrap(), 0.0, "the waiting DAW did not take over a live DAW's instance");
    first.stop();
    wait_until("the second DAW to take over", Duration::from_secs(3), || *b.last_input.lock().unwrap() == 0.25);
    second.stop();
}

#[test]
fn a_crashed_engine_is_survived_and_a_new_one_is_picked_up() {
    confluence_provider_vasio::isolate_for_tests();
    let engine = Engine::start(1, 48_000.0, 256, 0.25);
    let mut dev = open(1);
    let seen = Arc::new(Seen::default());
    dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    wait_until("audio", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.25);
    engine.crash();
    wait_until("silence after the crash", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.0);
    let before = seen.callbacks.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    assert!(seen.callbacks.load(Ordering::Relaxed) > before + 50, "the DAW keeps getting callbacks");
    let _engine = Engine::start(1, 48_000.0, 256, 0.125);
    wait_until("audio from the new engine", Duration::from_secs(3), || *seen.last_input.lock().unwrap() == 0.125);
    dev.stop();
}

#[test]
fn an_engine_with_a_different_block_makes_the_daw_reset() {
    confluence_provider_vasio::isolate_for_tests();
    let mut dev = open(4);
    let health = dev.health();
    let seen = Arc::new(Seen::default());
    let stream = dev.start(StreamConfig::default(), loopback_daw(seen.clone())).unwrap();
    // The DAW opened with the engine absent; the engine then comes up with another block size.
    let other = if stream.block == 128 { 256 } else { 128 };
    let _engine = Engine::start(4, stream.sample_rate, other, 0.25);
    wait_until("a reset request to the DAW", Duration::from_secs(3), || {
        health.reset_requests.load(Ordering::Relaxed) >= 1
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(health.reset_requests.load(Ordering::Relaxed), 1, "asked once, not every retry");
    assert_eq!(*seen.last_input.lock().unwrap(), 0.0, "never linked to a stream of the wrong shape");
    dev.stop();
}
