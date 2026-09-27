//! Shared-memory stream protocol (spec §3.1): one layout for every
//! cross-process audio stream (VASIO now; VAIO's user side and the plugin
//! sandbox later).
//!
//! A *server* (the engine) publishes a stream under a base name; a *client*
//! (e.g. the VASIO DLL inside a DAW) connects to it. Each stream is:
//! - a header: version, sample rate, block, channel counts, capacity,
//!   heartbeats and the counters of two rings;
//! - a ring to the client and a ring from the client (interleaved f32);
//! - a named auto-reset event the server sets after each block it writes.
//!
//! Windows named mappings cannot be resized, and a client may still hold an
//! old one across a server restart. So each base name has a tiny fixed-size
//! *directory* holding the current generation, and the stream's mapping and
//! event are named after that generation. A client notices a new generation
//! (or a closed directory) and reconnects.

pub mod ring;

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{Client, Server};

use std::sync::atomic::{AtomicU32, AtomicU64};

use ring::RingCounters;

/// "CNFLSHM1".
pub const MAGIC: u64 = 0x314D_4853_4C46_4E43;
pub const VERSION: u32 = 1;

/// Shape of a stream, fixed for the life of one generation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub sample_rate: f64,
    /// Frames per block.
    pub block: u32,
    pub to_client_channels: u32,
    pub from_client_channels: u32,
    /// Frames each ring holds.
    pub capacity_frames: u32,
}

impl Layout {
    fn samples_offset() -> usize {
        std::mem::size_of::<Header>()
    }

    /// Bytes of the whole mapping.
    pub fn bytes(&self) -> usize {
        let ch = (self.to_client_channels + self.from_client_channels) as usize;
        Self::samples_offset() + ch * self.capacity_frames as usize * std::mem::size_of::<f32>()
    }

    fn is_sane(&self) -> bool {
        self.sample_rate.is_finite()
            && self.sample_rate > 0.0
            && self.block > 0
            && self.capacity_frames >= self.block
            && self.to_client_channels <= 1024
            && self.from_client_channels <= 1024
            && self.capacity_frames <= 1 << 20
    }
}

/// The start of every stream mapping.
#[repr(C)]
#[derive(Debug)]
pub struct Header {
    pub magic: u64,
    pub version: u32,
    pub block: u32,
    pub sample_rate: f64,
    pub to_client_channels: u32,
    pub from_client_channels: u32,
    pub capacity_frames: u32,
    /// Set by the client while it is streaming, cleared when it stops: a
    /// stopped client is not a late one.
    pub client_active: AtomicU32,
    /// Blocks the server has run (it advances even with no client).
    pub server_heartbeat: AtomicU64,
    /// Blocks the client has processed.
    pub client_heartbeat: AtomicU64,
    pub to_client: RingCounters,
    pub from_client: RingCounters,
}

impl Header {
    pub fn layout(&self) -> Layout {
        Layout {
            sample_rate: self.sample_rate,
            block: self.block,
            to_client_channels: self.to_client_channels,
            from_client_channels: self.from_client_channels,
            capacity_frames: self.capacity_frames,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ShmError {
    #[error("{call}: {message}")]
    Win32 { call: &'static str, message: String },
    #[error("invalid stream layout {0:?}")]
    Layout(Layout),
    #[error("the stream mapping is malformed")]
    Malformed,
}
