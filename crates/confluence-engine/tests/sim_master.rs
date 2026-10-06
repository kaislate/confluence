//! A drifting hardware master drives the engine: its tone reaches a soft output
//! running on a third clock, and both drifts are measured correctly.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use confluence_api::{Command, Response};
use confluence_core::asrc::AsrcQuality;
use confluence_engine::sim::{SimMaster, SimOutput, Simulation};
use confluence_engine::{Engine, EngineConfig, MasterSlotSpec, SoftSlotSpec};

#[test]
fn drifting_master_feeds_a_soft_output_on_another_clock() {
    let (mut engine, audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let master = MasterSlotSpec {
        name: "master".into(),
        device: "asio:sim".into(),
        inputs: 2,
        outputs: 2,
        first_input: None,
        first_output: None,
    };
    let (_, ch) = engine.add_master_slot(&master).unwrap();
    let out_spec = SoftSlotSpec {
        name: "usb-out".into(),
        device: String::new(),
        channels: 2,
        device_rate: 44_100.0,
        device_block: 441,
        quality: AsrcQuality::Sinc64,
        first_channel: None,
        margin_frames: None,
        max_growth_frames: None,
    };
    let (out_id, out_dev) = engine.add_soft_output(&out_spec).unwrap();
    let out_first = engine.slots().into_iter().find(|s| s.id == out_id).unwrap().first_output;
    let route = Command::SetPoint {
        input: ch.first_input as u32,
        output: out_first + 1,
        gain_db: -6.0206,
        mute: false,
        invert: false,
    };
    assert_eq!(engine.handle(&route), Response::Ok);

    let master_rate = 48_000.0 * (1.0 - 200e-6);
    let mut sim = Simulation::with_master(
        audio,
        SimMaster { true_rate: master_rate, channels: ch, tone_hz: 997.0, amplitude: 0.5 },
    );
    sim.outputs.push(SimOutput::new(out_dev, 2, 44_100.0 * (1.0 + 150e-6), 441, 1, 30.0));
    sim.run_until(90.0, 0.02, |_| engine.tick());

    assert!((engine.master_ppm() + 200.0).abs() < 5.0, "master drift {}", engine.master_ppm());
    let Response::Health { slots, .. } = engine.handle(&Command::Health) else { panic!() };
    let out = slots.iter().find(|h| h.id == out_id).unwrap();
    assert_eq!(out.underruns + out.overruns, 0, "{out:?}");
    assert!((out.device_ppm - 150.0).abs() < 5.0, "soft device drift {out:?}");

    let rec = &sim.outputs[0].recording;
    let peak = rec.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!((peak - 0.25).abs() < 0.01, "peak {peak}");
    let worst = rec.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0f32, f32::max);
    assert!(worst < 0.01, "discontinuity {worst}");
}
