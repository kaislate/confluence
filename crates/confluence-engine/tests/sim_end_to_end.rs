//! Headless end-to-end run in simulated time: a drifting capture device is
//! routed through the matrix to a differently drifting playback device.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use confluence_api::{Command, Response};
use confluence_core::asrc::AsrcQuality;
use confluence_engine::sim::{SimInput, SimOutput, Simulation};
use confluence_engine::{Engine, EngineConfig, SoftSlotSpec};

fn spec(name: &str, rate: f64, block: usize) -> SoftSlotSpec {
    SoftSlotSpec {
        name: name.into(),
        channels: 2,
        device_rate: rate,
        device_block: block,
        quality: AsrcQuality::Sinc64,
        device: String::new(),
        first_channel: None,
    }
}

#[test]
fn drifting_input_reaches_drifting_output_cleanly() {
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (_, in_dev) = engine.add_soft_input(&spec("usb-in", 48_000.0, 128)).unwrap();
    let (_, out_dev) = engine.add_soft_output(&spec("usb-out", 44_100.0, 441)).unwrap();
    // Input channel 0 (sine) → output channel 1 at −6 dB.
    let set = Command::SetPoint { input: 0, output: 1, gain_db: -6.0206, mute: false, invert: false };
    assert_eq!(engine.handle(&set), Response::Ok);

    let mut sim = Simulation::new(audio, 48_000.0);
    sim.inputs.push(SimInput::new(in_dev, 2, 48_000.0 * (1.0 + 250e-6), 128, 997.0, 0.5));
    sim.outputs.push(SimOutput::new(out_dev, 2, 44_100.0 * (1.0 - 150e-6), 441, 1, 30.0));
    sim.run_until(90.0, 0.02, |_| engine.tick());

    let Response::Health { blocks, slots, .. } = engine.handle(&Command::Health) else { panic!() };
    assert!(blocks > 16_000, "blocks {blocks}");
    for h in &slots {
        assert_eq!(h.underruns + h.overruns, 0, "{h:?}");
    }

    let rec = &sim.outputs[0].recording;
    assert!(rec.len() > 44_100 * 50, "recorded {}", rec.len());
    let peak = rec.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!((peak - 0.25).abs() < 0.01, "peak {peak} (0.5 at −6 dB)");
    // 997 Hz at 44.1 kHz, amplitude 0.25: second differences ≤ 0.25·(2π·997/44100)² ≈ 0.005.
    let worst = rec.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0f32, f32::max);
    assert!(worst < 0.01, "discontinuity {worst}");
}
