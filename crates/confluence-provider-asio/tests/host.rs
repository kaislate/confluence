//! The ASIO host against the in-process fake driver: every host path without hardware.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use confluence_provider_asio::convert::SampleFormat;
use confluence_provider_asio::fake::FakeConfig;
use confluence_provider_asio::sys::{ST_FLOAT32_LSB, ST_INT24_LSB, ST_INT32_LSB};
use confluence_provider_asio::{AsioCallback, AsioDevice, AsioHostError, AsioIo, DriverSource, StreamConfig};

fn open(cfg: FakeConfig) -> AsioDevice {
    AsioDevice::open(DriverSource::Fake(cfg)).unwrap()
}

/// Copies input 0 to outputs 0 and 1 at half level.
fn half_level_copy() -> impl FnMut(&mut AsioIo<'_>) + Send + 'static {
    let mut buf = vec![0f32; 4096];
    move |io: &mut AsioIo<'_>| {
        let n = io.frames();
        io.read_input(0, &mut buf[..n]);
        for s in &mut buf[..n] {
            *s *= 0.5;
        }
        io.write_output(0, &buf[..n]);
        io.write_output(1, &buf[..n]);
    }
}

fn idle() -> Box<dyn AsioCallback> {
    Box::new(|_: &mut AsioIo<'_>| {})
}

#[test]
fn info_reflects_the_driver() {
    let mut cfg = FakeConfig::new("Fake A");
    cfg.inputs = 4;
    cfg.outputs = 6;
    cfg.sample_type = ST_INT24_LSB;
    let dev = open(cfg);
    let info = dev.info();
    assert_eq!(info.name, "Fake A");
    assert_eq!((info.inputs(), info.outputs()), (4, 6));
    assert_eq!(info.input_names[2], "Fake In 3");
    assert!(info.output_formats.iter().all(|f| *f == Some(SampleFormat::I24)));
    assert_eq!((info.preferred_block, info.sample_rate), (128, 48_000.0));
}

#[test]
fn callbacks_move_audio_through_driver_buffers() {
    for sample_type in [ST_INT32_LSB, ST_INT24_LSB, ST_FLOAT32_LSB] {
        let mut cfg = FakeConfig::new("Fake B");
        cfg.sample_type = sample_type;
        let probe = cfg.probe.clone();
        let mut dev = open(cfg);
        let info = dev.start(StreamConfig::default(), Box::new(half_level_copy())).unwrap();
        assert_eq!((info.block, info.sample_rate), (128, 48_000.0));
        assert!(info.post_output);
        std::thread::sleep(Duration::from_millis(150));
        dev.stop();
        let calls = dev.health().callbacks.load(Ordering::Relaxed);
        assert!(calls > 20, "{calls} callbacks");
        assert_eq!(dev.health().faults.load(Ordering::Relaxed), 0);
        let out = probe.last_output.lock().unwrap().clone();
        assert!(out.iter().all(|&s| (s - 0.125).abs() < 1e-4), "type {sample_type}: {:?}", &out[..4]);
        assert!(probe.output_ready_calls.load(Ordering::Relaxed) >= calls, "outputReady after each switch");
    }
}

#[test]
fn frames_since_last_follows_the_sample_position() {
    for use_time_info in [true, false] {
        let mut cfg = FakeConfig::new("Fake C");
        cfg.use_time_info = use_time_info;
        cfg.skip_every = Some(5);
        let seen = Arc::new(AtomicU32::new(0));
        let max_seen = seen.clone();
        let mut dev = open(cfg);
        dev.start(
            StreamConfig::default(),
            Box::new(move |io: &mut AsioIo<'_>| {
                max_seen.fetch_max(io.frames_since_last(), Ordering::Relaxed);
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        dev.stop();
        let max = seen.load(Ordering::Relaxed);
        assert_eq!(max, 256, "a skipped buffer shows as two blocks (time info: {use_time_info})");
        assert!(dev.health().gaps.load(Ordering::Relaxed) > 0);
    }
}

#[test]
fn a_panicking_callback_is_contained_and_silenced() {
    let cfg = FakeConfig::new("Fake D");
    let probe = cfg.probe.clone();
    let mut dev = open(cfg);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    dev.start(
        StreamConfig::default(),
        Box::new(|io: &mut AsioIo<'_>| {
            io.write_output(0, &[1.0; 128]);
            panic!("handler bug");
        }),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    dev.stop();
    std::panic::set_hook(prev);
    let h = dev.health();
    assert!(h.faults.load(Ordering::Relaxed) > 5);
    assert_eq!(h.faults.load(Ordering::Relaxed), h.callbacks.load(Ordering::Relaxed));
    assert!(probe.last_output.lock().unwrap().iter().all(|&s| s == 0.0), "outputs silenced after a fault");
}

#[test]
fn two_drivers_stream_side_by_side() {
    let counts: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut devs: Vec<AsioDevice> = (0..2).map(|i| open(FakeConfig::new(&format!("Fake E{i}")))).collect();
    for (dev, c) in devs.iter_mut().zip(&counts) {
        let c = c.clone();
        let handler = move |_: &mut AsioIo<'_>| {
            c.fetch_add(1, Ordering::Relaxed);
        };
        dev.start(StreamConfig::default(), Box::new(handler)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(150));
    for d in &mut devs {
        d.stop();
    }
    for c in &counts {
        assert!(c.load(Ordering::Relaxed) > 20, "each driver reaches its own handler");
    }
}

#[test]
fn driver_requests_are_counted() {
    let mut cfg = FakeConfig::new("Fake F");
    cfg.reset_after = Some(3);
    let mut dev = open(cfg);
    dev.start(StreamConfig::default(), idle()).unwrap();
    std::thread::sleep(Duration::from_millis(80));
    dev.stop();
    assert_eq!(dev.health().reset_requests.load(Ordering::Relaxed), 1);
}

#[test]
fn unsupported_settings_are_rejected_without_starting() {
    let mut dev = open(FakeConfig::new("Fake G"));
    let err = dev.start(StreamConfig { sample_rate: None, block: Some(100) }, idle());
    assert!(matches!(err, Err(AsioHostError::Block { block: 100, .. })), "{err:?}");
    let err = dev.start(StreamConfig { sample_rate: Some(12_345.0), block: None }, idle());
    assert_eq!(err.err(), Some(AsioHostError::Rate(12_345.0)));
    let ok = dev.start(StreamConfig { sample_rate: Some(96_000.0), block: Some(256) }, idle());
    assert_eq!(ok.map(|i| (i.sample_rate, i.block)).unwrap(), (96_000.0, 256));
    assert_eq!(dev.start(StreamConfig::default(), idle()).err(), Some(AsioHostError::AlreadyRunning));
}

#[test]
fn init_failure_is_reported_with_the_driver_message() {
    let mut cfg = FakeConfig::new("Fake H");
    cfg.fail_init = true;
    let probe = cfg.probe.clone();
    let err = AsioDevice::open(DriverSource::Fake(cfg)).err();
    assert_eq!(err, Some(AsioHostError::Init("fake driver asked to fail".into())));
    assert!(probe.released.load(Ordering::Acquire), "the driver object is released");
}

#[test]
fn slots_are_released_so_restarts_never_run_out() {
    let cfg = FakeConfig::new("Fake I");
    let probe = cfg.probe.clone();
    let mut dev = open(cfg);
    for _ in 0..40 {
        dev.start(StreamConfig::default(), idle()).unwrap();
        dev.stop();
    }
    drop(dev);
    assert!(probe.released.load(Ordering::Acquire));
}

#[test]
fn a_missing_driver_is_a_clear_error() {
    let err = AsioDevice::open_installed("definitely not an installed driver").err();
    assert_eq!(err, Some(AsioHostError::NotInstalled("definitely not an installed driver".into())));
}
