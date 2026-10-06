//! The network host over loopback: two hosts on this PC (ports chosen by the
//! system), a sender feeding one and a receiver on the other.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_core::buffer::PlanarBuffer;
use confluence_net::host::{FrameSink, NetHost, SendSpec};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Collects every frame received, interleaved.
#[derive(Clone, Default)]
struct Tap(Arc<Mutex<Vec<f32>>>);
impl FrameSink for Tap {
    fn write(&mut self, data: &[f32], _time: f64) {
        self.0.lock().unwrap().extend_from_slice(data);
    }
    fn set_latency_floor(&self, _frames: f64) {}
}

fn start() -> NetHost {
    NetHost::start(SocketAddr::new(LOCAL, 0), 0xC0FFEE).unwrap()
}

/// Sends `blocks` blocks of 256 frames, each channel `c` holding `(c + 1) / 100`,
/// paced at real time.
fn stream(side: &mut confluence_net::host::SendSide, channels: usize, blocks: usize) {
    let mut block = PlanarBuffer::new(channels, 256);
    for c in 0..channels {
        block.channel_mut(c).fill((c + 1) as f32 / 100.0);
    }
    let started = Instant::now();
    for b in 0..blocks {
        side.write(&block, 0);
        let due = started + Duration::from_secs_f64((b + 1) as f64 * 256.0 / 48_000.0);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}

/// Frames sent for `blocks` blocks: whole 1 ms packets only (the rest waits for more).
fn whole(blocks: usize) -> usize {
    blocks * 256 / 48 * 48
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn audio_sent_to_a_host_arrives_in_its_receiver() {
    let (a, b) = (start(), start());
    let tap = Tap::default();
    let rx = b.add_receiver(LOCAL, "Main", 2, 48_000, Box::new(tap.clone()));
    let dest = SocketAddr::new(LOCAL, b.port());
    let (mut side, tx) = a.add_sender(SendSpec { dest, stream: "Main".into(), channels: 2, rate: 48_000 });
    stream(&mut side, 2, 40); // ~213 ms
    wait_for("all frames", || tap.0.lock().unwrap().len() >= whole(40) * 2);
    let got = tap.0.lock().unwrap().clone();
    assert!(got.chunks(2).all(|f| (f[0] - 0.01).abs() < 1e-6 && (f[1] - 0.02).abs() < 1e-6));
    let s = rx.stats();
    assert_eq!((s.lost, s.late, s.malformed), (0, 0, 0), "{s:?} sent {} dropped {}", tx.packets(), tx.dropped());
    assert!(s.packets >= 40 * 256 / 48, "{s:?}");
    assert!(tx.packets() >= s.packets);
}

#[test]
fn many_channels_are_split_and_put_back_together() {
    let (a, b) = (start(), start());
    let tap = Tap::default();
    let _rx = b.add_receiver(LOCAL, "Wide", 40, 48_000, Box::new(tap.clone()));
    let dest = SocketAddr::new(LOCAL, b.port());
    let (mut side, _tx) = a.add_sender(SendSpec { dest, stream: "Wide".into(), channels: 40, rate: 48_000 });
    stream(&mut side, 40, 10);
    wait_for("all frames", || tap.0.lock().unwrap().len() >= whole(10) * 40);
    let got = tap.0.lock().unwrap().clone();
    for frame in got.chunks(40) {
        for (c, v) in frame.iter().enumerate() {
            assert!((v - (c + 1) as f32 / 100.0).abs() < 1e-6, "channel {c}: {v}");
        }
    }
}

#[test]
fn a_stream_nobody_added_is_listed_but_not_played() {
    let (a, b) = (start(), start());
    let dest = SocketAddr::new(LOCAL, b.port());
    let (mut side, _tx) = a.add_sender(SendSpec { dest, stream: "Guest".into(), channels: 6, rate: 48_000 });
    stream(&mut side, 6, 5);
    wait_for("the stream to be heard", || !b.heard().is_empty());
    let heard = b.heard();
    assert_eq!(heard.len(), 1);
    assert_eq!(
        (heard[0].from, heard[0].stream.as_str(), heard[0].channels, heard[0].rate),
        (LOCAL, "Guest", 6, 48_000)
    );
}

#[test]
fn the_socket_buffers_a_long_stall() {
    // If the network thread is held up (a busy PC), packets wait in the
    // socket: Windows' default buffer holds only a few milliseconds of a wide stream.
    assert!(start().receive_buffer_bytes() >= 1 << 20);
}

#[test]
fn junk_from_a_source_is_counted_against_its_receiver() {
    let b = start();
    let tap = Tap::default();
    let rx = b.add_receiver(LOCAL, "Main", 2, 48_000, Box::new(tap.clone()));
    let junk = UdpSocket::bind((LOCAL, 0)).unwrap();
    // A burst bigger than Windows' default socket buffer: none may be dropped.
    for n in 0..200u32 {
        junk.send_to(&vec![n as u8; 10 + (n as usize * 7) % 1390], (LOCAL, b.port())).unwrap();
    }
    wait_for("junk counted", || rx.stats().malformed == 200);
    assert!(tap.0.lock().unwrap().is_empty());
    assert!(b.heard().is_empty());
}

#[test]
fn a_removed_receiver_gets_nothing_more_and_silence_is_reported() {
    let (a, b) = (start(), start());
    let tap = Tap::default();
    let rx = b.add_receiver(LOCAL, "Main", 2, 48_000, Box::new(tap.clone()));
    let dest = SocketAddr::new(LOCAL, b.port());
    let (mut side, _tx) = a.add_sender(SendSpec { dest, stream: "Main".into(), channels: 2, rate: 48_000 });
    stream(&mut side, 2, 5);
    wait_for("frames", || !tap.0.lock().unwrap().is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert!(rx.stats().silent_ms >= 250, "{:?}", rx.stats());
    drop(rx);
    let before = tap.0.lock().unwrap().len();
    stream(&mut side, 2, 5);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(tap.0.lock().unwrap().len(), before, "removed");
    wait_for("heard instead", || !b.heard().is_empty());
}
