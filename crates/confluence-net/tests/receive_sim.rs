//! Simulated network: a sender whose clock drifts sends 1 ms packets in bursts
//! (one burst per master block), the network delays them unevenly (with
//! occasional Wi-Fi-like spikes, so they also arrive out of order), and the
//! receiver feeds a soft-input bridge read by this engine's master clock.
//! Once settled there must be no xruns and no discontinuities, and the
//! sender's drift must be measured.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::f64::consts::TAU;

use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, BridgeConfig};
use confluence_core::buffer::PlanarBuffer;
use confluence_net::packet::{Format, Header};
use confluence_net::receiver::Receiver;

const RATE: f64 = 48_000.0;
const PACKET: usize = 48;
const SENDER_BLOCK: usize = 256;
const MASTER_BLOCK: usize = 256;

struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn run(sender_ppm: f64, spikes: bool, seconds: f64, settle: f64) {
    let cfg = BridgeConfig {
        channels: 1,
        device_rate: RATE,
        device_block: PACKET,
        master_rate: RATE,
        master_block: MASTER_BLOCK,
        quality: AsrcQuality::Sinc64,
        margin_frames: 2 * PACKET,
        max_growth_frames: Some((0.040 * RATE) as usize),
    };
    let (mut dev, mut eng, stats) = soft_input(cfg).unwrap();
    let mut rx = Receiver::new(1, RATE as u32);
    let sender_rate = RATE * (1.0 + sender_ppm * 1e-6);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);

    // Every packet with its arrival time, in arrival order.
    let mut arrivals: Vec<(f64, u32, Vec<f32>)> = Vec::new();
    let mut frame = 0u64;
    let mut block_end = SENDER_BLOCK as u64;
    while (frame as f64) / sender_rate < seconds {
        let sent = block_end as f64 / sender_rate; // the burst leaves when the block is done
        while frame + PACKET as u64 <= block_end {
            let samples: Vec<f32> = (0..PACKET)
                .map(|n| (0.5 * (TAU * 997.0 * (frame + n as u64) as f64 / sender_rate).sin()) as f32)
                .collect();
            let mut delay = 0.001 + 0.003 * rng.unit();
            if spikes && rng.unit() < 0.0005 {
                delay += 0.015;
            }
            arrivals.push((sent + delay, frame as u32, samples));
            frame += PACKET as u64;
        }
        block_end += SENDER_BLOCK as u64;
    }
    arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut out = PlanarBuffer::new(1, MASTER_BLOCK);
    let (mut next, mut blocks) = (0usize, 0u64);
    let mut xruns_at_settle = None;
    let mut lost_at_settle = 0;
    let (mut prev, mut max_jump, mut primed) = ([0f32; 2], 0f32, 0u32);
    let mut poll_at = 0.0;
    loop {
        let t_master = (blocks + 1) as f64 * MASTER_BLOCK as f64 / RATE;
        if t_master > seconds - 0.1 {
            break;
        }
        // Network and receiver work up to this master callback.
        while next < arrivals.len() && arrivals[next].0 <= t_master {
            while poll_at < arrivals[next].0 {
                rx.poll(poll_at, &mut |d, t| dev.write_interleaved(d, t));
                poll_at += 0.001;
            }
            let (t, ts, ref s) = arrivals[next];
            let h = Header {
                seq: 0,
                timestamp: ts,
                ssrc: 1,
                format: Format::F32,
                total_channels: 1,
                first_channel: 0,
                channels: 1,
                rate: RATE as u32,
                stream: "Main".into(),
            };
            rx.push(&h, s, t, &mut |d, t| dev.write_interleaved(d, t));
            dev.set_latency_floor(rx.latency_floor());
            next += 1;
        }
        eng.read(&mut out, 0, t_master, 0.0);
        blocks += 1;
        if t_master >= settle {
            if xruns_at_settle.is_none() {
                let h = stats.snapshot();
                xruns_at_settle = Some(h.underruns + h.overruns);
                lost_at_settle = rx.stats().lost;
            }
            for &x in out.channel(0) {
                if primed >= 2 {
                    let j = (x - 2.0 * prev[1] + prev[0]).abs();
                    max_jump = max_jump.max(j);
                }
                primed += 1;
                prev = [prev[1], x];
            }
        }
    }
    let h = stats.snapshot();
    let at_settle = xruns_at_settle.unwrap();
    assert_eq!(h.underruns + h.overruns, at_settle, "xruns after settling: {h:?}, receiver {:?}", rx.stats());
    assert!(max_jump < 0.02, "discontinuity {max_jump} ({h:?})");
    assert!((h.device_ppm - sender_ppm).abs() < 10.0, "ppm {h:?}");
    assert_eq!(rx.stats().lost, lost_at_settle, "losses after settling: {:?}", rx.stats());
}

#[test]
fn a_wired_stream_with_drift_and_bursts_plays_cleanly() {
    run(150.0, false, 60.0, 30.0);
}

#[test]
fn delay_spikes_grow_the_buffer_and_then_play_cleanly() {
    run(-120.0, true, 90.0, 45.0);
}
