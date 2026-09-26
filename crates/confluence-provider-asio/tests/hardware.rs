//! Opt-in tests against real ASIO drivers. They never run in CI or plain
//! `cargo test`: set `CONFLUENCE_HW_ASIO` to a `;`-separated list of installed
//! driver names and run with `--ignored`. Outputs are always silent.
//!
//!     CONFLUENCE_HW_ASIO="MOTU Gen 5;GoXLR ASIO Driver" \
//!       cargo test -p confluence-provider-asio --test hardware -- --ignored --nocapture --test-threads=1
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use confluence_core::clock::{RateEstimator, DEFAULT_RATE_BANDWIDTH_HZ};
use confluence_provider_asio::{AsioDevice, AsioIo, StreamConfig};

fn devices() -> Vec<String> {
    std::env::var("CONFLUENCE_HW_ASIO")
        .map(|v| v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Silent handler that measures the device clock against the engine time base.
fn measuring(rate: f64) -> (impl FnMut(&mut AsioIo<'_>) + Send + 'static, Arc<Mutex<RateEstimator>>) {
    let est = Arc::new(Mutex::new(RateEstimator::new(rate, DEFAULT_RATE_BANDWIDTH_HZ)));
    let shared = est.clone();
    let handler = move |io: &mut AsioIo<'_>| {
        io.silence_outputs();
        // Test-only: a lock in the callback is fine for a measurement harness.
        if let Ok(mut e) = shared.try_lock() {
            e.update(io.frames_since_last(), io.now());
        }
    };
    (handler, est)
}

#[test]
#[ignore = "needs real ASIO hardware; see file header"]
fn each_listed_driver_streams_silence_on_time() {
    let names = devices();
    assert!(!names.is_empty(), "set CONFLUENCE_HW_ASIO");
    for name in names {
        let mut dev = AsioDevice::open_installed(&name).unwrap_or_else(|e| panic!("{name}: {e}"));
        let info = dev.info().clone();
        println!(
            "{name}: '{}' v{} — {} in / {} out, {} Hz, block {} (min {}, max {}, gran {}), formats in {:?} out {:?}",
            info.name,
            info.version,
            info.inputs(),
            info.outputs(),
            info.sample_rate,
            info.preferred_block,
            info.min_block,
            info.max_block,
            info.granularity,
            info.input_formats.first(),
            info.output_formats.first()
        );
        let (handler, est) = measuring(info.sample_rate);
        let stream = dev.start(StreamConfig::default(), Box::new(handler)).unwrap_or_else(|e| panic!("{name}: {e}"));
        println!("  stream: {stream:?}");
        let seconds = 5.0;
        std::thread::sleep(Duration::from_secs_f64(seconds));
        dev.stop();
        let h = dev.health();
        let calls = h.callbacks.load(Ordering::Relaxed) as f64;
        let expected = seconds * stream.sample_rate / stream.block as f64;
        let ppm = est.lock().unwrap().ppm();
        println!(
            "  {calls} callbacks (expected ≈{expected:.0}), faults {}, gaps {}, resets {}, clock {ppm:+.1} ppm vs QPC",
            h.faults.load(Ordering::Relaxed),
            h.gaps.load(Ordering::Relaxed),
            h.reset_requests.load(Ordering::Relaxed)
        );
        assert!((calls / expected - 1.0).abs() < 0.1, "{name}: callback rate off");
        assert_eq!(h.faults.load(Ordering::Relaxed), 0);
    }
}

#[test]
#[ignore = "needs two real ASIO devices; see file header"]
fn two_drivers_stream_at_once_and_their_drift_is_measured() {
    let names = devices();
    assert!(names.len() >= 2, "list two drivers in CONFLUENCE_HW_ASIO");
    let mut devs = Vec::new();
    let mut ests = Vec::new();
    let mut counts = Vec::new();
    for name in &names[..2] {
        let mut dev = AsioDevice::open_installed(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (mut handler, est) = measuring(dev.info().sample_rate);
        let count = Arc::new(AtomicU64::new(0));
        let c = count.clone();
        let counting = move |io: &mut AsioIo<'_>| {
            c.fetch_add(1, Ordering::Relaxed);
            handler(io);
        };
        dev.start(StreamConfig::default(), Box::new(counting)).unwrap_or_else(|e| panic!("{name}: {e}"));
        devs.push(dev);
        ests.push(est);
        counts.push(count);
    }
    std::thread::sleep(Duration::from_secs(30));
    for d in &mut devs {
        d.stop();
    }
    let ppm: Vec<f64> = ests.iter().map(|e| e.lock().unwrap().ppm()).collect();
    for (i, name) in names[..2].iter().enumerate() {
        println!("{name}: {} callbacks, {:+.2} ppm vs QPC", counts[i].load(Ordering::Relaxed), ppm[i]);
        assert_eq!(devs[i].health().faults.load(Ordering::Relaxed), 0);
        assert!(counts[i].load(Ordering::Relaxed) > 100);
    }
    println!("relative drift between the two devices: {:+.2} ppm", ppm[0] - ppm[1]);
}
