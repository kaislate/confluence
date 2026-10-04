//! Against the real driver, in the VAIO test VM only: set CONFLUENCE_VM_VAIO=1
//! and run with --ignored --test-threads=1 (tools/vaio-vm/Invoke-VaioVmTests.ps1).
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::f64::consts::TAU;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use confluence_core::buffer::PlanarBuffer;
use confluence_provider_vaio::{VaioError, VaioSlot};
use confluence_provider_wasapi::{endpoints, Direction, Handler, Target, WasapiStream};

fn enabled() -> bool {
    std::env::var("CONFLUENCE_VM_VAIO").is_ok_and(|v| v == "1")
}

/// Plays a 997 Hz sine to the VAIO endpoint until dropped.
fn play_tone() -> (WasapiStream, Arc<AtomicU64>) {
    let ep = endpoints(Direction::Render)
        .unwrap()
        .into_iter()
        .find(|e| e.name.contains("Confluence VAIO"))
        .expect("no Confluence VAIO endpoint: is the driver installed?");
    let mut stream = WasapiStream::open(Target::Endpoint { id: ep.id.clone(), direction: Direction::Render }).unwrap();
    let f = stream.format();
    let played = Arc::new(AtomicU64::new(0));
    let p = played.clone();
    let mut n = 0u64;
    stream
        .start(Handler::Render(Box::new(move |buf: &mut [f32], _now| {
            for frame in buf.chunks_mut(f.channels) {
                let s = (0.5 * (TAU * 997.0 * n as f64 / f.sample_rate).sin()) as f32;
                frame.fill(s);
                n += 1;
            }
            p.fetch_add((buf.len() / f.channels) as u64, Ordering::Relaxed);
        })))
        .unwrap();
    (stream, played)
}

/// Runs a fake engine clock: a block every `block / rate` seconds for `secs`,
/// handing each block's left channel to `each`.
fn run_engine(slot: &mut VaioSlot, block: usize, rate: f64, secs: f64, mut each: impl FnMut(&[f32])) {
    let mut buf = PlanarBuffer::new(2, block);
    let start = Instant::now();
    let period = block as f64 / rate;
    let mut k = 0u64;
    while start.elapsed().as_secs_f64() < secs {
        let due = start + Duration::from_secs_f64(k as f64 * period);
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        slot.receive(&mut buf, 0);
        each(buf.channel(0));
        k += 1;
    }
}

#[test]
#[ignore = "needs the VAIO driver (VM only)"]
fn a_tone_played_to_vaio_arrives_in_the_engine() {
    if !enabled() {
        return;
    }
    let mut slot = VaioSlot::open(48_000.0, 256).unwrap();
    let (_stream, _) = play_tone();
    let mut samples = Vec::new();
    run_engine(&mut slot, 256, 48_000.0, 3.0, |b| samples.extend_from_slice(b));
    let tail = &samples[samples.len() / 2..]; // after start-up
    let rms = (tail.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
    assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.05, "rms {rms}");
    let crossings = tail.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count() as f64;
    let hz = crossings / (tail.len() as f64 / 48_000.0);
    assert!((hz - 997.0).abs() < 10.0, "frequency {hz}");
    let jump = tail.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0f32, f32::max);
    assert!(jump < 0.02, "a click in the audio (second difference {jump})");
    assert_eq!(slot.stats().underruns.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "needs the VAIO driver (VM only)"]
fn the_endpoint_follows_the_engine_clock() {
    if !enabled() {
        return;
    }
    // An engine running 2% slow: the app must be paced by it, not by the system clock.
    let mut slot = VaioSlot::open(48_000.0, 256).unwrap();
    let (_stream, played) = play_tone();
    std::thread::sleep(Duration::from_millis(500));
    let mut taken = 0u64;
    let before = played.load(Ordering::Relaxed);
    run_engine(&mut slot, 256, 48_000.0 * 0.98, 5.0, |b| taken += b.len() as u64);
    let app = played.load(Ordering::Relaxed) - before;
    let drift = app as f64 - taken as f64;
    assert!(drift.abs() < 4096.0, "the app played {app} frames while the engine took {taken}");
}

#[test]
#[ignore = "needs the VAIO driver (VM only)"]
fn the_endpoint_keeps_playing_without_the_engine() {
    if !enabled() {
        return;
    }
    let (_stream, played) = play_tone();
    std::thread::sleep(Duration::from_millis(500));
    let before = played.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    let frames = played.load(Ordering::Relaxed) - before;
    assert!((frames as f64 - 96_000.0).abs() < 9_600.0, "{frames} frames in 2 s without an engine");
}

#[test]
#[ignore = "needs the VAIO driver (VM only)"]
fn a_second_engine_cannot_attach() {
    if !enabled() {
        return;
    }
    let _first = VaioSlot::open(48_000.0, 256).unwrap();
    assert_eq!(VaioSlot::open(48_000.0, 256).err(), Some(VaioError::InUse));
}

#[test]
#[ignore = "needs the VAIO driver (VM only)"]
fn a_crashed_engine_releases_the_driver() {
    if !enabled() {
        return;
    }
    if std::env::var("CONFLUENCE_VAIO_CRASH_CHILD").is_ok() {
        // The child: attach, then die without cleaning up.
        let _slot = VaioSlot::open(48_000.0, 256).unwrap();
        std::process::abort();
    }
    let (_stream, played) = play_tone();
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new(exe)
        .args(["a_crashed_engine_releases_the_driver", "--ignored", "--exact", "--test-threads=1"])
        .env("CONFLUENCE_VAIO_CRASH_CHILD", "1")
        .status()
        .unwrap();
    assert!(!status.success(), "the child aborts");
    // Windows cancelled its request; a new engine can attach at once, and the app kept playing.
    let before = played.load(Ordering::Relaxed);
    let _slot = VaioSlot::open(48_000.0, 256).expect("the driver let go of the dead engine");
    std::thread::sleep(Duration::from_millis(500));
    assert!(played.load(Ordering::Relaxed) > before, "the app kept playing");
}
