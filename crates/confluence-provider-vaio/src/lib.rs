//! Engine side of VAIO, the virtual Windows playback endpoint (spec §7.5).
//!
//! What apps play to "Confluence VAIO" arrives here as a strict slot with two
//! inputs, on the engine's clock: the driver advances the endpoint only as
//! fast as this side takes audio out of the shared ring.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use confluence_core::buffer::PlanarBuffer;

pub mod abi;
mod attach;
mod reader;
mod region;

pub use abi::CHANNELS;
pub use reader::Reader;
pub use region::Region;

/// Frames queued beyond one engine block, so the driver's 1 ms refill and
/// DPC jitter never starve the engine (4 ms at 48 kHz).
pub const TARGET_MARGIN_FRAMES: u32 = 192;

#[derive(Debug, Default)]
pub struct VaioStats {
    /// Engine blocks with too little audio while an app was playing.
    pub underruns: AtomicU64,
    /// The driver holds this engine's region.
    pub attached: AtomicBool,
    /// An app stream is playing to the endpoint.
    pub streaming: AtomicBool,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VaioError {
    #[error("VAIO runs at 48 kHz, but the engine runs at {0} Hz")]
    Rate(f64),
    #[error("an engine block of {0} frames does not fit VAIO's ring (1-8192)")]
    Block(usize),
    #[error("the Confluence VAIO driver is not installed")]
    NotInstalled,
    #[error("Confluence VAIO is already in use by another engine (or this account may not open it)")]
    InUse,
    #[error("the Confluence VAIO driver refused the engine's ring")]
    Rejected,
    #[error("VAIO: {0}")]
    Io(String),
}

pub fn check_rate(sample_rate: f64) -> Result<(), VaioError> {
    if (sample_rate - f64::from(abi::SAMPLE_RATE)).abs() < 0.5 {
        Ok(())
    } else {
        Err(VaioError::Rate(sample_rate))
    }
}

/// (capacity, target) of the ring for an engine block: the target is one
/// block plus the margin, and the capacity at least twice the target.
pub fn ring_shape(block: usize) -> Result<(u32, u32), VaioError> {
    if block == 0 || block > 8192 {
        return Err(VaioError::Block(block));
    }
    let target = block as u32 + TARGET_MARGIN_FRAMES;
    let capacity = (target * 2).next_power_of_two().max(abi::MIN_CAPACITY);
    if capacity > abi::MAX_CAPACITY {
        return Err(VaioError::Block(block));
    }
    Ok((capacity, target))
}

/// Whether the driver's control device exists (it does not open it).
pub fn installed() -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut buf = [0u16; 512];
    // SAFETY: valid name and buffer.
    unsafe { QueryDosDeviceW(&HSTRING::from(abi::DOS_NAME), Some(&mut buf)) != 0 }
}

/// The engine's end of the VAIO endpoint. Dropping it detaches the driver,
/// which keeps the endpoint playing on its own clock.
pub struct VaioSlot {
    reader: Reader,
    block: usize,
    stats: Arc<VaioStats>,
    // Dropped last: cancels the request and waits for it before the region can go.
    _attachment: attach::Attachment,
}

impl VaioSlot {
    pub fn open(sample_rate: f64, block: usize) -> Result<VaioSlot, VaioError> {
        check_rate(sample_rate)?;
        let (capacity, target) = ring_shape(block)?;
        let region = Arc::new(Region::new(capacity, target)?);
        let stats = Arc::new(VaioStats::default());
        let attachment = attach::Attachment::start(region.clone(), stats.clone())?;
        Ok(VaioSlot { reader: Reader::new(region, stats.clone()), block, stats, _attachment: attachment })
    }

    pub fn stats(&self) -> Arc<VaioStats> {
        self.stats.clone()
    }

    /// Before routing: writes what apps played into engine inputs
    /// `first_channel..first_channel + 2`, or silence. Real-time safe.
    pub fn receive(&mut self, inputs: &mut PlanarBuffer, first_channel: usize) {
        let block = self.block.min(inputs.frames());
        if !self.reader.read(block, |ch, f, s| inputs.channel_mut(first_channel + ch)[f] = s) {
            for ch in 0..CHANNELS {
                inputs.channel_mut(first_channel + ch)[..block].fill(0.0);
            }
        }
    }
}
