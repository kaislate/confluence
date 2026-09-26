//! Preallocated planar audio buffer used on both sides of the engine.

/// `channels` planar channels of up to `capacity` frames each, stored contiguously.
/// `frames()` is the number of valid frames in the current block.
pub struct PlanarBuffer {
    data: Box<[f32]>,
    channels: usize,
    capacity: usize,
    frames: usize,
}

impl PlanarBuffer {
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self { data: vec![0.0; channels * capacity].into_boxed_slice(), channels, capacity, frames: capacity }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Sets the valid block length, clamped to `capacity`.
    pub fn set_frames(&mut self, frames: usize) {
        self.frames = frames.min(self.capacity);
    }

    /// The valid frames of channel `ch`. Panics if `ch >= channels()`.
    pub fn channel(&self, ch: usize) -> &[f32] {
        let start = ch * self.capacity;
        &self.data[start..start + self.frames]
    }

    /// The valid frames of channel `ch`, mutably. Panics if `ch >= channels()`.
    pub fn channel_mut(&mut self, ch: usize) -> &mut [f32] {
        let start = ch * self.capacity;
        &mut self.data[start..start + self.frames]
    }

    /// Zeroes the valid frames of every channel.
    pub fn clear(&mut self) {
        for ch in 0..self.channels {
            self.channel_mut(ch).fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_independent_and_sized_by_frames() {
        let mut b = PlanarBuffer::new(2, 8);
        b.set_frames(4);
        b.channel_mut(1).fill(1.0);
        assert_eq!(b.channel(0), &[0.0; 4]);
        assert_eq!(b.channel(1), &[1.0; 4]);
        b.set_frames(100);
        assert_eq!(b.frames(), 8);
    }
}
