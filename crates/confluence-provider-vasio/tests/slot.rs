//! The engine side of VASIO against a minimal stand-in for the DLL.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;

use confluence_core::buffer::PlanarBuffer;
use confluence_provider_vasio::{VasioError, VasioSlot};
use confluence_shm::Client;

const BLOCK: usize = 64;

/// A DAW stand-in: each engine block it reads the engine's audio and answers
/// with that audio plus `offset`, one block later (like a real driver).
struct Daw {
    client: Client,
    reader: confluence_shm::ring::RingReader,
    writer: confluence_shm::ring::RingWriter,
    offset: f32,
}

impl Daw {
    fn connect(instance: u32, offset: f32) -> Daw {
        let client = Client::connect(&confluence_provider_vasio::stream_name(instance)).unwrap().unwrap();
        // SAFETY: the ends are stored with the client and dropped with it; taken once.
        let (mut reader, writer) = unsafe { client.ends() };
        reader.skip_all();
        client.header().client_active.store(1, Ordering::Release);
        Daw { client, reader, writer, offset }
    }

    /// A clean stop, as the DLL does when the DAW stops streaming.
    fn stop(self) {
        self.client.header().client_active.store(0, Ordering::Release);
    }

    fn process(&mut self) {
        assert!(self.client.wait(1000), "the engine wakes the DAW every block");
        let mut block = vec![vec![0.0f32; BLOCK]; 2];
        self.reader.read_frames(BLOCK, |ch, f, s| block[ch][f] = s);
        let offset = self.offset;
        assert!(self.writer.write_frames(BLOCK, |ch, f| block[ch][f] + offset));
        self.client.header().client_heartbeat.fetch_add(1, Ordering::Release);
    }
}

/// One engine block: take the DAW's output, "route" it, send `value` to the DAW.
fn engine_block(slot: &mut VasioSlot, value: f32) -> f32 {
    let mut inputs = PlanarBuffer::new(4, BLOCK);
    let mut outputs = PlanarBuffer::new(4, BLOCK);
    slot.receive(&mut inputs, 2);
    for ch in 0..2 {
        outputs.channel_mut(1 + ch).fill(value);
    }
    slot.send(&outputs, 1);
    inputs.channel(2)[0]
}

#[test]
fn audio_goes_to_the_daw_and_comes_back_one_block_later() {
    confluence_provider_vasio::isolate_for_tests();
    let mut slot = VasioSlot::open(3, 2, 2, 48_000.0, BLOCK).unwrap();
    let mut daw = Daw::connect(3, 100.0);
    // Until the engine has seen the DAW run, it sends nothing and hears silence.
    assert_eq!(engine_block(&mut slot, 1.0), 0.0);
    daw.process();
    let mut heard = Vec::new();
    for value in 2..8 {
        heard.push(engine_block(&mut slot, value as f32));
        daw.process();
    }
    assert!(slot.stats().connected.load(Ordering::Relaxed));
    // Engine block n sends n; the DAW answers n + 100, heard in block n + 1.
    assert_eq!(heard[2..], [103.0, 104.0, 105.0, 106.0]);
    assert_eq!(slot.stats().underruns.load(Ordering::Relaxed), 0);
    assert_eq!(slot.stats().overruns.load(Ordering::Relaxed), 0);
}

#[test]
fn a_daw_that_stops_is_noticed_and_costs_only_silence() {
    confluence_provider_vasio::isolate_for_tests();
    let mut slot = VasioSlot::open(4, 2, 2, 48_000.0, BLOCK).unwrap();
    let mut daw = Daw::connect(4, 0.5);
    for _ in 0..4 {
        engine_block(&mut slot, 1.0);
        daw.process();
    }
    assert!(slot.stats().connected.load(Ordering::Relaxed));
    drop(daw);
    // 0.25 s of blocks without a heartbeat: disconnected, silent, no overruns piling up.
    let mut last = 1.0;
    for _ in 0..300 {
        last = engine_block(&mut slot, 1.0);
    }
    assert!(!slot.stats().connected.load(Ordering::Relaxed));
    assert_eq!(last, 0.0);
    let overruns = slot.stats().overruns.load(Ordering::Relaxed);
    assert!(overruns < 10, "a gone DAW is not an endless stream of overruns: {overruns}");
}

#[test]
fn instances_and_channel_counts_are_checked() {
    confluence_provider_vasio::isolate_for_tests();
    assert_eq!(VasioSlot::open(0, 2, 2, 48_000.0, BLOCK).err(), Some(VasioError::Instance(0)));
    assert_eq!(VasioSlot::open(9, 2, 2, 48_000.0, BLOCK).err(), Some(VasioError::Instance(9)));
    assert_eq!(VasioSlot::open(1, 1, 2, 48_000.0, BLOCK).err(), Some(VasioError::Channels(1)));
    assert_eq!(VasioSlot::open(1, 2, 129, 48_000.0, BLOCK).err(), Some(VasioError::Channels(129)));
}

#[test]
fn a_daw_that_stops_cleanly_costs_no_xruns() {
    confluence_provider_vasio::isolate_for_tests();
    let mut slot = VasioSlot::open(5, 2, 2, 48_000.0, BLOCK).unwrap();
    let mut daw = Daw::connect(5, 0.5);
    for _ in 0..4 {
        engine_block(&mut slot, 1.0);
        daw.process();
    }
    daw.stop();
    for _ in 0..300 {
        engine_block(&mut slot, 1.0);
    }
    assert!(!slot.stats().connected.load(Ordering::Relaxed), "a stopped DAW is not connected");
    assert_eq!(slot.stats().underruns.load(Ordering::Relaxed), 0, "stopping is not a glitch");
    assert_eq!(slot.stats().overruns.load(Ordering::Relaxed), 0);
}
