//! Opt-in tests against real Windows audio endpoints. Never run in CI or plain
//! `cargo test`: set `CONFLUENCE_HW_WASAPI=1` and run with `--ignored`.
//! Render tests play silence only; capture tests only read.
//!
//!     CONFLUENCE_HW_WASAPI=1 cargo test -p confluence-provider-wasapi --test hardware -- --ignored --nocapture --test-threads=1
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use confluence_core::clock::{RateEstimator, DEFAULT_RATE_BANDWIDTH_HZ};
use confluence_provider_wasapi::{default_endpoint, endpoints, Direction, Handler, Target, WasapiStream};

fn enabled() -> bool {
    std::env::var("CONFLUENCE_HW_WASAPI").is_ok_and(|v| v == "1")
}

fn run(target: Target, seconds: f64) -> (u64, u64, f64, f64) {
    let mut stream = WasapiStream::open(target).unwrap();
    let fmt = stream.format();
    println!("  format: {fmt:?}");
    let est = Arc::new(Mutex::new(RateEstimator::new(fmt.sample_rate, DEFAULT_RATE_BANDWIDTH_HZ)));
    let e = est.clone();
    let ch = fmt.channels;
    let handler = match fmt.direction {
        Direction::Render => Handler::Render(Box::new(move |buf: &mut [f32], now: f64| {
            buf.fill(0.0);
            if let Ok(mut e) = e.try_lock() {
                e.update((buf.len() / ch) as u32, now);
            }
        })),
        Direction::Capture => Handler::Capture(Box::new(move |buf: &[f32], now: f64| {
            if let Ok(mut e) = e.try_lock() {
                e.update((buf.len() / ch) as u32, now);
            }
        })),
    };
    stream.start(handler).unwrap();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let h = stream.health();
    let (calls, frames) =
        (h.callbacks.load(std::sync::atomic::Ordering::Relaxed), h.frames.load(std::sync::atomic::Ordering::Relaxed));
    drop(stream);
    let ppm = est.lock().unwrap().ppm();
    (calls, frames, fmt.sample_rate, ppm)
}

#[test]
#[ignore = "needs real audio endpoints; see file header"]
fn lists_endpoints() {
    assert!(enabled(), "set CONFLUENCE_HW_WASAPI=1");
    for dir in [Direction::Render, Direction::Capture] {
        for ep in endpoints(dir).unwrap() {
            println!("{dir:?}: {} — {} ch @ {} Hz", ep.name, ep.channels, ep.sample_rate);
        }
    }
}

#[test]
#[ignore = "needs real audio endpoints; see file header"]
fn default_render_and_capture_run_at_their_rate() {
    assert!(enabled(), "set CONFLUENCE_HW_WASAPI=1");
    for dir in [Direction::Render, Direction::Capture] {
        let ep = default_endpoint(dir).unwrap();
        println!("{dir:?} default: {}", ep.name);
        let seconds = 5.0;
        let (calls, frames, rate, ppm) = run(Target::Endpoint { id: ep.id, direction: dir }, seconds);
        println!("  {calls} callbacks, {frames} frames (expected ≈{:.0}), clock {ppm:+.1} ppm vs QPC", seconds * rate);
        assert!(calls > 50);
        assert!((frames as f64 / (seconds * rate) - 1.0).abs() < 0.1, "frame rate off");
    }
}

#[test]
#[ignore = "needs a Windows audio engine; see file header"]
fn app_capture_receives_what_this_process_plays() {
    assert!(enabled(), "set CONFLUENCE_HW_WASAPI=1");
    // This process plays silence on the default output; capturing our own
    // process must then deliver frames at the loopback rate.
    let out = default_endpoint(Direction::Render).unwrap();
    let mut player = WasapiStream::open(Target::Endpoint { id: out.id, direction: Direction::Render }).unwrap();
    player.start(Handler::Render(Box::new(|buf: &mut [f32], _| buf.fill(0.0)))).unwrap();
    let (calls, frames, rate, _) = run(Target::App { pid: std::process::id() }, 3.0);
    drop(player);
    println!("  app capture of our own process: {calls} callbacks, {frames} frames (expected ≈{:.0})", 3.0 * rate);
    assert!((frames as f64 / (3.0 * rate) - 1.0).abs() < 0.15, "loopback delivered {frames} frames");
}

#[test]
#[ignore = "needs a Windows audio engine; see file header"]
fn app_capture_opens_and_closes_repeatedly() {
    assert!(enabled(), "set CONFLUENCE_HW_WASAPI=1");
    for _ in 0..10 {
        let mut s = WasapiStream::open(Target::App { pid: std::process::id() }).unwrap();
        s.start(Handler::Capture(Box::new(|_: &[f32], _| {}))).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
}
