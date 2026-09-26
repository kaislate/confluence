//! The internal clock must deliver blocks at the nominal rate in real time.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use confluence_engine::clock::InternalClock;
use confluence_engine::{Engine, EngineConfig};

#[test]
fn delivers_blocks_at_the_nominal_rate() {
    let (engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let clock = InternalClock::start(audio, 48_000.0).unwrap();
    std::thread::sleep(Duration::from_millis(200)); // warm-up
    let start = engine.blocks();
    std::thread::sleep(Duration::from_secs(2));
    let blocks = engine.blocks() - start;
    assert!(clock.stop().is_some(), "engine is handed back on stop");
    // 2 s at 48 kHz / 256 = 375 blocks; allow scheduler slop.
    assert!((360..=390).contains(&blocks), "{blocks} blocks in 2 s");
}
