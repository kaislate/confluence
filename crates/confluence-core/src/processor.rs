//! What an insert bus runs between its sends and its returns.

use std::any::Any;

use crate::buffer::PlanarBuffer;

/// A processor failed this block (its bus goes silent and stops calling it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessError;

/// The real-time half of a bus's processing. `process` is called once per
/// block on the audio thread and must not allocate, lock or block.
///
/// `Any` lets the owner that created a processor take it back as its own type
/// once the audio side returns it (e.g. to deactivate a plugin on its thread).
pub trait Processor: Any + Send {
    /// Sends → returns for one block. `Err` is a processing error: the bus
    /// goes silent and stops calling this processor.
    fn process(&mut self, io: BusIo<'_>) -> Result<(), ProcessError>;

    /// Called on the audio thread when the processor is taken off its bus.
    fn stop(&mut self) {}
}

/// A bus's send channels (read) and return channels (written) for one block.
pub struct BusIo<'a> {
    sends: &'a PlanarBuffer,
    first_send: usize,
    returns: &'a mut PlanarBuffer,
    first_return: usize,
    channels: usize,
}

impl<'a> BusIo<'a> {
    /// `channels` is clamped so both ranges lie inside their buffers.
    pub fn new(
        sends: &'a PlanarBuffer,
        first_send: usize,
        returns: &'a mut PlanarBuffer,
        first_return: usize,
        channels: usize,
    ) -> Self {
        let channels = channels
            .min(sends.channels().saturating_sub(first_send))
            .min(returns.channels().saturating_sub(first_return));
        BusIo { sends, first_send, returns, first_return, channels }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.sends.frames().min(self.returns.frames())
    }

    /// Send channel `c` (0-based within the bus); empty if `c >= channels()`.
    pub fn send(&self, c: usize) -> &[f32] {
        if c >= self.channels {
            return &[];
        }
        &self.sends.channel(self.first_send + c)[..self.frames()]
    }

    /// Return channel `c`, mutably; empty if `c >= channels()`.
    pub fn ret(&mut self, c: usize) -> &mut [f32] {
        if c >= self.channels {
            return &mut [];
        }
        let f = self.frames();
        &mut self.returns.channel_mut(self.first_return + c)[..f]
    }

    /// Returns = sends.
    pub fn passthrough(&mut self) {
        let f = self.frames();
        for c in 0..self.channels {
            let src = &self.sends.channel(self.first_send + c)[..f];
            self.returns.channel_mut(self.first_return + c)[..f].copy_from_slice(src);
        }
    }

    /// Returns = silence.
    pub fn silence(&mut self) {
        let f = self.frames();
        for c in 0..self.channels {
            self.returns.channel_mut(self.first_return + c)[..f].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bufs() -> (PlanarBuffer, PlanarBuffer) {
        let mut s = PlanarBuffer::new(6, 4);
        let mut r = PlanarBuffer::new(6, 4);
        s.set_frames(4);
        r.set_frames(4);
        for c in 0..6 {
            s.channel_mut(c).fill(c as f32 + 1.0);
            r.channel_mut(c).fill(-1.0);
        }
        (s, r)
    }

    #[test]
    fn passthrough_copies_the_bus_channels_only() {
        let (s, mut r) = bufs();
        BusIo::new(&s, 2, &mut r, 4, 2).passthrough();
        assert_eq!(r.channel(4), &[3.0; 4]);
        assert_eq!(r.channel(5), &[4.0; 4]);
        assert_eq!(r.channel(3), &[-1.0; 4], "outside the bus: untouched");
    }

    #[test]
    fn silence_zeroes_the_returns() {
        let (s, mut r) = bufs();
        BusIo::new(&s, 0, &mut r, 1, 2).silence();
        assert_eq!(r.channel(1), &[0.0; 4]);
        assert_eq!(r.channel(2), &[0.0; 4]);
        assert_eq!(r.channel(0), &[-1.0; 4]);
    }

    #[test]
    fn a_bus_past_the_end_is_clamped() {
        let (s, mut r) = bufs();
        let mut io = BusIo::new(&s, 5, &mut r, 0, 4);
        assert_eq!(io.channels(), 1);
        assert_eq!(io.frames(), 4);
        assert_eq!(io.send(0), &[6.0; 4]);
        io.ret(0).fill(2.0);
        assert!(io.send(1).is_empty() && io.ret(1).is_empty(), "past the bus: nothing");
        assert_eq!(r.channel(0), &[2.0; 4]);
    }
}
