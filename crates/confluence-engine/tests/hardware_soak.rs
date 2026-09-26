//! Opt-in soak through the whole engine on real devices. Never runs in CI or
//! plain `cargo test`. Nothing is routed to an audible output.
//!
//!     CONFLUENCE_HW_MASTER="GoXLR ASIO Driver" \
//!     CONFLUENCE_HW_ASIO_SOFT="VB-Matrix VASIO-8" \
//!     CONFLUENCE_HW_CAPTURE="Microphone (HD Pro Webcam C920)" \
//!     CONFLUENCE_HW_SOAK_SECONDS=60 \
//!       cargo test -p confluence-engine --test hardware_soak -- --ignored --nocapture
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use confluence_api::{Command, DeviceKind, Response, SlotHealth};
use confluence_engine::devices::{start_asio_master, DeviceManager};
use confluence_engine::{Engine, EngineConfig};
use confluence_provider_asio::AsioDevice;
use confluence_provider_wasapi::{default_endpoint, Direction, Handler, Target, WasapiStream};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}; see file header"))
}

fn health(engine: &mut Engine) -> Vec<SlotHealth> {
    match engine.handle(&Command::Health) {
        Response::Health { slots, .. } => slots,
        other => panic!("{other:?}"),
    }
}

#[test]
#[ignore = "needs real devices; see file header"]
fn engine_runs_clean_on_real_devices() {
    let seconds: f64 = std::env::var("CONFLUENCE_HW_SOAK_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let master_name = var("CONFLUENCE_HW_MASTER");
    let mut master = AsioDevice::open_installed(&master_name).unwrap();
    let (rate, block) = (master.info().sample_rate, master.info().preferred_block as usize);
    let (mut engine, audio) = Engine::new(EngineConfig::new(rate, block));
    let (master_id, stream, _) = start_asio_master(&mut master, &mut engine, audio, &master_name, None).unwrap();
    println!("master {master_name}: {stream:?}");

    let mut devices = DeviceManager::new(None);
    let asio_ids = devices.add(&mut engine, DeviceKind::Asio, &var("CONFLUENCE_HW_ASIO_SOFT")).unwrap();
    let cap_ids = devices.add(&mut engine, DeviceKind::WasapiCapture, &var("CONFLUENCE_HW_CAPTURE")).unwrap();
    let app_ids = devices.add(&mut engine, DeviceKind::AppCapture, &std::process::id().to_string()).unwrap();
    // Give app capture something to capture: this process plays silence.
    let out = default_endpoint(Direction::Render).unwrap();
    let mut player = WasapiStream::open(Target::Endpoint { id: out.id, direction: Direction::Render }).unwrap();
    player.start(Handler::Render(Box::new(|b: &mut [f32], _| b.fill(0.0)))).unwrap();

    // Route the capture device into the (inaudible) virtual ASIO outputs.
    let slots = engine.slots();
    let cap = slots.iter().find(|s| s.id == cap_ids[0]).unwrap();
    let vout = slots.iter().find(|s| s.id == asio_ids[1]).unwrap();
    for c in 0..cap.inputs.min(vout.outputs) {
        let cmd = Command::SetPoint {
            input: cap.first_input + c,
            output: vout.first_output + c,
            gain_db: 0.0,
            mute: false,
            invert: false,
        };
        assert_eq!(engine.handle(&cmd), Response::Ok);
    }
    for s in &slots {
        println!(
            "  slot #{} {:<40} {:?} in {}+{} out {}+{}",
            s.id, s.name, s.role, s.first_input, s.inputs, s.first_output, s.outputs
        );
    }

    let settle = 30.0f64.min(seconds / 2.0);
    let start = Instant::now();
    let mut baseline: Option<Vec<SlotHealth>> = None;
    let mut next_report = 10.0;
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(10));
        engine.tick();
        let t = start.elapsed().as_secs_f64();
        if baseline.is_none() && t >= settle {
            baseline = Some(health(&mut engine));
        }
        if t >= next_report {
            next_report += 10.0;
            for h in health(&mut engine) {
                println!(
                    "  t={t:5.1}s #{:<2} xruns {}/{} fill {:6.1}/{:6.1} drift {:+8.2} ppm corr {:+7.2}",
                    h.id, h.underruns, h.overruns, h.fill_frames, h.target_frames, h.device_ppm, h.correction_ppm
                );
            }
        }
    }
    let end = health(&mut engine);
    let baseline = baseline.unwrap();
    println!("master drift vs QPC: {:+.2} ppm, blocks {}", engine.master_ppm(), engine.blocks());
    drop(player);
    drop(devices);
    master.stop();

    let expected_blocks = seconds * rate / block as f64;
    assert!((engine.blocks() as f64 / expected_blocks - 1.0).abs() < 0.05, "master drove the engine on time");
    for id in asio_ids.iter().chain(&cap_ids).chain(&app_ids) {
        let (b, e) = (baseline.iter().find(|h| h.id == *id).unwrap(), end.iter().find(|h| h.id == *id).unwrap());
        assert_eq!(e.underruns + e.overruns, b.underruns + b.overruns, "slot {id}: xruns after settling: {e:?}");
    }
    assert!(end.iter().any(|h| h.id == master_id));
}
