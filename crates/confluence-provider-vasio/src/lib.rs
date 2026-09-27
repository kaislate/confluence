//! Engine side of the virtual ASIO driver (spec §7.4). VASIO instance `n` is
//! a strict slot: it runs on the engine's clock with no resampling. Each
//! master block the engine hands the DAW that block's audio and takes the
//! DAW's output from the block before, so the round trip adds two blocks.
//!
//! Naming follows the DAW: the DAW's *inputs* carry engine outputs, and the
//! DAW's *outputs* become engine inputs.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use confluence_core::buffer::PlanarBuffer;
use confluence_shm::ring::{RingReader, RingWriter};
use confluence_shm::{Layout, Server, ShmError};

pub mod config;

/// Driver instances the DLL registers ("Confluence VASIO 1" … "8").
pub const INSTANCES: u32 = 8;
/// Channel limits per direction (spec §7.4).
pub const MIN_CHANNELS: usize = 2;
pub const MAX_CHANNELS: usize = 128;
/// A DAW whose heartbeat stalls this long counts as gone.
const CLIENT_TIMEOUT_S: f64 = 0.25;
/// Ring capacity in blocks.
const RING_BLOCKS: u32 = 4;

/// Environment variable that moves every VASIO stream into a private
/// namespace. Tests set it so they never meet a real engine or DAW.
pub const NAMESPACE_VAR: &str = "CONFLUENCE_VASIO_NAMESPACE";

/// Shared-memory base name of instance `n`.
pub fn stream_name(instance: u32) -> String {
    match std::env::var(NAMESPACE_VAR) {
        Ok(ns) if !ns.is_empty() => format!("VASIO.{ns}.{instance}"),
        _ => format!("VASIO.{instance}"),
    }
}

/// Puts this process's VASIO streams in a private namespace (for tests).
pub fn isolate_for_tests() {
    std::env::set_var(NAMESPACE_VAR, format!("test-{}", std::process::id()));
}

/// Counters readable from the control side.
#[derive(Debug, Default)]
pub struct VasioStats {
    /// Blocks the DAW delivered too late (the engine used silence).
    pub underruns: AtomicU64,
    /// Blocks the DAW had not consumed (the engine dropped them).
    pub overruns: AtomicU64,
    /// A DAW is currently running this instance.
    pub connected: AtomicBool,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VasioError {
    #[error("VASIO instance {0} does not exist (1-{INSTANCES})")]
    Instance(u32),
    #[error("VASIO channel counts must be {MIN_CHANNELS}-{MAX_CHANNELS}, not {0}")]
    Channels(usize),
    #[error(transparent)]
    Shm(#[from] ShmError),
}

/// The engine's end of one VASIO instance. Dropping it tells the DAW's driver
/// that the engine is gone (it keeps the DAW running on silence).
pub struct VasioSlot {
    to_daw: RingWriter,
    from_daw: RingReader,
    // Declared after the ring ends so they are dropped first.
    server: Server,
    block: usize,
    daw_inputs: usize,
    daw_outputs: usize,
    last_client_heartbeat: u64,
    quiet_blocks: u32,
    timeout_blocks: u32,
    /// The DAW stopped taking blocks: hold off until it has drained the ring,
    /// so one stall counts as one overrun rather than one per block.
    stalled: bool,
    stats: Arc<VasioStats>,
}

impl VasioSlot {
    /// Publishes instance `instance` with the engine's rate and block.
    pub fn open(
        instance: u32,
        daw_inputs: usize,
        daw_outputs: usize,
        sample_rate: f64,
        block: usize,
    ) -> Result<Self, VasioError> {
        if !(1..=INSTANCES).contains(&instance) {
            return Err(VasioError::Instance(instance));
        }
        for ch in [daw_inputs, daw_outputs] {
            if !(MIN_CHANNELS..=MAX_CHANNELS).contains(&ch) {
                return Err(VasioError::Channels(ch));
            }
        }
        let layout = Layout {
            sample_rate,
            block: block as u32,
            to_client_channels: daw_inputs as u32,
            from_client_channels: daw_outputs as u32,
            capacity_frames: block as u32 * RING_BLOCKS,
        };
        let server = Server::create(&stream_name(instance), layout)?;
        // SAFETY: the ends are stored next to `server` in `VasioSlot` and
        // declared before it, so they are dropped first; taken once.
        let (to_daw, from_daw) = unsafe { server.ends() };
        let timeout_blocks = ((CLIENT_TIMEOUT_S * sample_rate / block as f64).ceil() as u32).max(2);
        Ok(VasioSlot {
            to_daw,
            from_daw,
            server,
            block,
            daw_inputs,
            daw_outputs,
            last_client_heartbeat: 0,
            quiet_blocks: u32::MAX,
            timeout_blocks,
            stalled: false,
            stats: Arc::default(),
        })
    }

    pub fn stats(&self) -> Arc<VasioStats> {
        self.stats.clone()
    }

    pub fn daw_inputs(&self) -> usize {
        self.daw_inputs
    }

    pub fn daw_outputs(&self) -> usize {
        self.daw_outputs
    }

    /// Before routing: writes the DAW's latest output block into engine input
    /// channels `first_channel..first_channel + daw_outputs` (silence if none).
    /// Real-time safe.
    pub fn receive(&mut self, inputs: &mut PlanarBuffer, first_channel: usize) {
        let hb = self.server.header().client_heartbeat.load(Ordering::Acquire);
        if hb != self.last_client_heartbeat {
            self.last_client_heartbeat = hb;
            self.quiet_blocks = 0;
        } else {
            self.quiet_blocks = self.quiet_blocks.saturating_add(1);
        }
        let active = self.server.header().client_active.load(Ordering::Acquire) != 0;
        let connected = active && self.quiet_blocks < self.timeout_blocks;
        self.stats.connected.store(connected, Ordering::Relaxed);
        let block = self.block.min(inputs.frames());
        // Keep only the newest block: a backlog would only add latency.
        let backlog = self.from_daw.available().saturating_sub(block as u64);
        self.from_daw.skip(backlog);
        let got = self.from_daw.read_frames(block, |ch, f, s| inputs.channel_mut(first_channel + ch)[f] = s);
        if !got {
            for ch in 0..self.daw_outputs {
                inputs.channel_mut(first_channel + ch)[..block].fill(0.0);
            }
            if connected {
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// After routing: hands engine output channels `first_channel..` to the
    /// DAW's inputs and wakes its driver. Real-time safe.
    pub fn send(&mut self, outputs: &PlanarBuffer, first_channel: usize) {
        let h = self.server.header();
        h.server_heartbeat.fetch_add(1, Ordering::Release);
        if self.stalled && self.to_daw.is_empty() {
            self.stalled = false;
        }
        if self.stats.connected.load(Ordering::Relaxed) && !self.stalled {
            let block = self.block.min(outputs.frames());
            if !self.to_daw.write_frames(block, |ch, f| outputs.channel(first_channel + ch)[f]) {
                self.stats.overruns.fetch_add(1, Ordering::Relaxed);
                self.stalled = true;
            }
        }
        self.server.wake_client();
    }
}
