//! The engine's real-time entry point never allocates, even while it
//! adopts a newly added slot and a new routing snapshot.
#![cfg(debug_assertions)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use confluence_api::Command;
use confluence_core::asrc::AsrcQuality;
use confluence_engine::{BusSpec, Engine, EngineConfig, SoftSlotSpec};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn process_block_does_not_allocate() {
    let mut cfg = EngineConfig::new(48_000.0, 256);
    cfg.max_inputs = 16;
    cfg.max_outputs = 16;
    let (mut engine, mut audio) = Engine::new(cfg);
    let spec = |name: &str| SoftSlotSpec {
        name: name.into(),
        channels: 2,
        device_rate: 44_100.0,
        device_block: 441,
        quality: AsrcQuality::Sinc64,
        device: String::new(),
        first_channel: None,
    };
    let (_, mut dev_in) = engine.add_soft_input(&spec("in")).unwrap();
    let (_, mut dev_out) = engine.add_soft_output(&spec("out")).unwrap();
    engine.handle(&Command::SetPoint { input: 0, output: 1, gain_db: 0.0, mute: false, invert: false });
    engine.tick(); // routing snapshot and both slot messages are now pending
    let block = vec![0.1f32; 441 * 2];
    let mut out = vec![0.0f32; 441 * 2];
    for n in 0..6 {
        dev_in.write_interleaved(&block, n as f64 * 0.01);
    }
    assert_no_alloc(|| {
        for n in 0..8 {
            let now = 0.06 + n as f64 * (256.0 / 48_000.0);
            audio.process_block(now);
            dev_out.read_interleaved(&mut out, now);
        }
    });
}

#[test]
fn bus_blocks_do_not_allocate() {
    let mut cfg = EngineConfig::new(48_000.0, 256);
    cfg.max_inputs = 16;
    cfg.max_outputs = 16;
    let (mut engine, mut audio) = Engine::new(cfg);
    let spec = |n: &str| BusSpec { name: n.into(), channels: 2, first_input: None, first_output: None };
    engine.add_bus(&spec("a")).unwrap();
    engine.add_bus(&spec("b")).unwrap();
    let slots = engine.slots();
    let (a, b) = (&slots[0], &slots[1]);
    let set = |input, output| Command::SetPoint { input, output, gain_db: 0.0, mute: false, invert: false };
    engine.handle(&set(15, a.first_output));
    engine.handle(&set(a.first_input, b.first_output));
    engine.handle(&set(b.first_input, 15));
    engine.tick(); // two bus messages, a plan and a routing snapshot are pending
    assert_no_alloc(|| {
        for n in 0..8 {
            audio.process_block(n as f64 * 0.005);
        }
    });
}
