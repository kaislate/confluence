//! Proves the audio-thread entry points never allocate. `AllocDisabler`
//! only exists in debug builds, so this file is compiled there only.
#![cfg(debug_assertions)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, soft_output, BridgeConfig};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::matrix::matrix;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn router_process_with_pending_snapshot_does_not_allocate() {
    let (mut ctl, mut router) = matrix(8, 8, Duration::from_millis(10), 48_000.0);
    let inputs = PlanarBuffer::new(8, 256);
    let mut outputs = PlanarBuffer::new(8, 256);
    for n in 0..8 {
        ctl.set_point(n, 7 - n, PointParams::default()).unwrap();
    }
    ctl.tick(); // snapshot now waiting in the mailbox
    assert_no_alloc(|| {
        router.process(&inputs, &mut outputs);
        router.process(&inputs, &mut outputs);
    });
    ctl.tick(); // old snapshot freed here, on the control side
}

#[test]
fn bridges_do_not_allocate() {
    let cfg = BridgeConfig {
        channels: 2,
        device_rate: 44_100.0,
        device_block: 128,
        master_rate: 48_000.0,
        master_block: 256,
        quality: AsrcQuality::Sinc64,
        margin_frames: 24,
    };
    let (mut in_dev, mut in_eng, _) = soft_input(cfg).unwrap();
    let (mut out_eng, mut out_dev, _) = soft_output(cfg).unwrap();
    let dev_block = vec![0.25f32; 256];
    let mut dev_out = vec![0.0f32; 256];
    let mut block = PlanarBuffer::new(2, 256);
    // Prime past the start-up threshold so the resampling paths run.
    for n in 0..8 {
        in_dev.write_interleaved(&dev_block, n as f64 * 0.003);
    }
    assert_no_alloc(|| {
        for n in 0..4 {
            let now = 0.03 + n as f64 * 0.005;
            in_dev.write_interleaved(&dev_block, now);
            in_eng.read(&mut block, 0, now, 0.0);
            out_eng.write(&block, 0, now, 0.0);
            out_dev.read_interleaved(&mut dev_out, now);
        }
    });
}
