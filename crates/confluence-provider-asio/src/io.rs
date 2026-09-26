//! What an ASIO callback sees: one half of the driver's double buffers.

use crate::convert::{decode, encode, SampleFormat};

/// One channel's pair of driver buffers.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub buffers: [*mut u8; 2],
    pub format: SampleFormat,
}

// SAFETY: the pointers refer to driver-owned buffers that are only touched from
// the driver's callback thread while the stream runs.
unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

/// Access to the current buffer half during one driver callback. All methods
/// are real-time safe: no allocation, no locks, and out-of-range channels are
/// ignored instead of panicking.
pub struct AsioIo<'a> {
    pub(crate) frames: usize,
    pub(crate) now: f64,
    pub(crate) sample_position: i64,
    pub(crate) frames_since_last: u32,
    pub(crate) inputs: &'a [Channel],
    pub(crate) outputs: &'a [Channel],
    pub(crate) half: usize,
}

impl AsioIo<'_> {
    /// Frames in this callback (the stream's block size).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Callback time in seconds on the engine time base (`confluence_rt::now_seconds`).
    pub fn now(&self) -> f64 {
        self.now
    }

    /// The driver's sample position at the start of this block.
    pub fn sample_position(&self) -> i64 {
        self.sample_position
    }

    /// Device frames elapsed since the previous callback: the block size
    /// normally, more after the driver skipped buffers.
    pub fn frames_since_last(&self) -> u32 {
        self.frames_since_last
    }

    pub fn input_channels(&self) -> usize {
        self.inputs.len()
    }

    pub fn output_channels(&self) -> usize {
        self.outputs.len()
    }

    /// Decodes input channel `ch` into `dst` (up to `frames()` samples).
    pub fn read_input(&self, ch: usize, dst: &mut [f32]) {
        if let Some(c) = self.inputs.get(ch) {
            let n = dst.len().min(self.frames);
            // SAFETY: the driver's buffer holds `frames` samples of `format`.
            let bytes = unsafe { std::slice::from_raw_parts(c.buffers[self.half], n * c.format.bytes_per_sample()) };
            decode(c.format, bytes, &mut dst[..n]);
        }
    }

    /// Encodes `src` (up to `frames()` samples) into output channel `ch`.
    pub fn write_output(&mut self, ch: usize, src: &[f32]) {
        if let Some(c) = self.outputs.get(ch) {
            let n = src.len().min(self.frames);
            // SAFETY: as above; outputs are ours to write during the callback.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(c.buffers[self.half], n * c.format.bytes_per_sample()) };
            encode(c.format, &src[..n], bytes);
        }
    }

    /// Writes silence to every output channel (all-zero bytes are silence in
    /// every supported format).
    pub fn silence_outputs(&mut self) {
        for c in self.outputs {
            // SAFETY: as above.
            unsafe { std::ptr::write_bytes(c.buffers[self.half], 0, self.frames * c.format.bytes_per_sample()) };
        }
    }
}
