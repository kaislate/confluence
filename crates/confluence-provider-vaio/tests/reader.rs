//! The engine's VAIO reader against a fake driver writing into the region.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::sync::Arc;

use confluence_provider_vaio::{check_rate, ring_shape, Reader, Region, VaioError, VaioStats};

/// Does what the driver does: puts frames (n, -n) in the ring and bumps the write counter.
fn driver_writes(region: &Region, frames: u64) {
    let h = region.header();
    let start = h.write_frames.load(Ordering::Acquire);
    for n in start..start + frames {
        // SAFETY: test-only writer; nothing else writes these frames.
        let f = unsafe { region.frame_mut(n) };
        *f = [n as f32, -(n as f32)];
    }
    h.write_frames.store(start + frames, Ordering::Release);
}

fn setup(block: usize) -> (Arc<Region>, Reader, Arc<VaioStats>) {
    let (capacity, target) = ring_shape(block).unwrap();
    let region = Arc::new(Region::new(capacity, target).unwrap());
    let stats = Arc::new(VaioStats::default());
    let reader = Reader::new(region.clone(), stats.clone());
    (region, reader, stats)
}

#[test]
fn a_block_comes_out_in_order_and_deinterleaved() {
    let (region, mut reader, _) = setup(256);
    region.header().streaming.store(1, Ordering::Release);
    driver_writes(&region, 300);
    let mut got = vec![(0usize, 0usize, 0f32); 0];
    assert!(reader.read(256, |ch, f, s| got.push((ch, f, s))));
    assert_eq!(got.len(), 512);
    assert!(got.contains(&(0, 0, 0.0)) && got.contains(&(1, 255, -255.0)));
    assert_eq!(region.header().read_frames.load(Ordering::Acquire), 256);
    assert!(!reader.read(256, |_, _, _| {}), "only 44 left");
}

#[test]
fn every_read_is_a_heartbeat() {
    let (region, mut reader, _) = setup(256);
    for _ in 0..3 {
        reader.read(256, |_, _, _| {});
    }
    assert_eq!(region.header().engine_heartbeat.load(Ordering::Acquire), 3);
}

#[test]
fn nothing_playing_is_silence_not_underruns() {
    let (region, mut reader, stats) = setup(256);
    for _ in 0..10 {
        assert!(!reader.read(256, |_, _, _| {}));
    }
    assert_eq!(stats.underruns.load(Ordering::Relaxed), 0, "no app stream: silence is expected");
    // An app starts: the empty ring before its first block is not an underrun either.
    region.header().streaming.store(1, Ordering::Release);
    assert!(!reader.read(256, |_, _, _| {}));
    assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);
    driver_writes(&region, 256);
    assert!(reader.read(256, |_, _, _| {}));
    // Now a dry ring is an underrun, counted once per dry spell.
    assert!(!reader.read(256, |_, _, _| {}));
    assert!(!reader.read(256, |_, _, _| {}));
    assert_eq!(stats.underruns.load(Ordering::Relaxed), 1);
    assert!(stats.streaming.load(Ordering::Relaxed));
}

#[test]
fn a_nonsense_write_counter_is_skipped_not_trusted() {
    let (region, mut reader, _) = setup(256);
    region.header().streaming.store(1, Ordering::Release);
    region.header().write_frames.store(1 << 40, Ordering::Release); // far more than a ring
    assert!(!reader.read(256, |_, _, _| {}));
    assert_eq!(region.header().read_frames.load(Ordering::Acquire), 1 << 40, "resynced to the writer");
    driver_writes(&region, 256);
    assert!(reader.read(256, |_, _, _| {}));
}

#[test]
fn the_engine_rate_and_block_are_checked_first() {
    assert!(check_rate(48_000.0).is_ok());
    assert!(matches!(check_rate(44_100.0), Err(VaioError::Rate(r)) if r == 44_100.0));
    let msg = check_rate(96_000.0).unwrap_err().to_string();
    assert!(msg.contains("48") && msg.contains("96"), "{msg}");
    let (capacity, target) = ring_shape(256).unwrap();
    assert_eq!((capacity, target), (1024, 448));
    let (capacity, target) = ring_shape(4096).unwrap();
    assert_eq!((capacity, target), (16_384, 4288));
    assert!(matches!(ring_shape(20_000), Err(VaioError::Block(20_000))));
    assert!(matches!(ring_shape(0), Err(VaioError::Block(0))));
}
