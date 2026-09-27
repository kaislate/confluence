//! Single-producer single-consumer f32 frame rings over shared memory.
//!
//! Positions are monotonic 64-bit frame counters; a sample lives at
//! `(position % capacity) * channels + channel`. The writer publishes samples
//! with a release store of `write`, the reader frees them with a release store
//! of `read`, and each side acquires the other's counter, so the rings work
//! across processes without locks.

use std::sync::atomic::{AtomicU64, Ordering};

/// The two counters of one ring, living in shared memory.
#[repr(C)]
#[derive(Debug, Default)]
pub struct RingCounters {
    pub write: AtomicU64,
    pub read: AtomicU64,
}

/// Where a ring lives: its counters and `capacity * channels` samples.
#[derive(Clone, Copy, Debug)]
pub struct RingMemory {
    pub counters: *const RingCounters,
    pub samples: *mut f32,
    pub capacity: u64,
    pub channels: usize,
}

impl RingMemory {
    fn counters(&self) -> &RingCounters {
        // SAFETY: `RingMemory` is only built (by `ring` below and the shm views)
        // from counters that outlive every writer and reader made from it.
        unsafe { &*self.counters }
    }

    fn slot(&self, position: u64, channel: usize) -> *mut f32 {
        let frame = (position % self.capacity) as usize;
        // SAFETY: frame < capacity and channel < channels, so the offset is
        // inside the `capacity * channels` sample region.
        unsafe { self.samples.add(frame * self.channels + channel) }
    }
}

/// Builds the writer and reader of a ring.
///
/// # Safety
/// `memory` must describe valid, suitably aligned counters and
/// `capacity * channels` samples that stay mapped while either end is alive,
/// and at most one writer and one reader may exist for it at a time.
pub unsafe fn ring(memory: RingMemory) -> (RingWriter, RingReader) {
    (RingWriter(memory), RingReader(memory))
}

/// The producing end of a ring.
#[derive(Debug)]
pub struct RingWriter(RingMemory);

// SAFETY: the ring protocol makes one writer and one reader safe to use from
// different threads (and processes).
unsafe impl Send for RingWriter {}

impl RingWriter {
    /// Frames that can be written without overtaking the reader.
    pub fn free_frames(&self) -> u64 {
        let c = self.0.counters();
        let w = c.write.load(Ordering::Relaxed);
        let r = c.read.load(Ordering::Acquire);
        self.0.capacity.saturating_sub(w.wrapping_sub(r))
    }

    /// True when the reader has consumed everything written.
    pub fn is_empty(&self) -> bool {
        self.free_frames() == self.0.capacity
    }

    /// Writes `frames` frames whose samples come from `sample(channel, frame)`.
    /// Writes nothing and returns false if they do not fit.
    pub fn write_frames(&mut self, frames: usize, sample: impl Fn(usize, usize) -> f32) -> bool {
        if (frames as u64) > self.free_frames() {
            return false;
        }
        let c = self.0.counters();
        let w = c.write.load(Ordering::Relaxed);
        for f in 0..frames {
            for ch in 0..self.0.channels {
                // SAFETY: the frame is free (checked above), so only this writer touches it.
                unsafe { self.0.slot(w + f as u64, ch).write(sample(ch, f)) };
            }
        }
        c.write.store(w + frames as u64, Ordering::Release);
        true
    }
}

/// The consuming end of a ring.
#[derive(Debug)]
pub struct RingReader(RingMemory);

// SAFETY: as for `RingWriter`.
unsafe impl Send for RingReader {}

impl RingReader {
    /// Frames written and not yet read.
    pub fn available(&self) -> u64 {
        let c = self.0.counters();
        c.write.load(Ordering::Acquire).wrapping_sub(c.read.load(Ordering::Relaxed)).min(self.0.capacity)
    }

    /// Reads `frames` frames into `sink(channel, frame, sample)`. Reads nothing
    /// and returns false if fewer are available.
    pub fn read_frames(&mut self, frames: usize, mut sink: impl FnMut(usize, usize, f32)) -> bool {
        if (frames as u64) > self.available() {
            return false;
        }
        let c = self.0.counters();
        let r = c.read.load(Ordering::Relaxed);
        for f in 0..frames {
            for ch in 0..self.0.channels {
                // SAFETY: the frame was published by the writer (checked above).
                sink(ch, f, unsafe { self.0.slot(r + f as u64, ch).read() });
            }
        }
        c.read.store(r + frames as u64, Ordering::Release);
        true
    }

    /// Drops up to `frames` unread frames (the oldest first).
    pub fn skip(&mut self, frames: u64) {
        let c = self.0.counters();
        let n = frames.min(self.available());
        c.read.store(c.read.load(Ordering::Relaxed) + n, Ordering::Release);
    }

    /// Drops everything unread, so the next read gets only new frames.
    pub fn skip_all(&mut self) {
        self.skip(u64::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring on the heap, for tests.
    struct HeapRing {
        counters: Box<RingCounters>,
        samples: Vec<f32>,
    }

    impl HeapRing {
        fn new(capacity: usize, channels: usize) -> (Self, RingMemory) {
            let mut r = HeapRing { counters: Box::default(), samples: vec![0.0; capacity * channels] };
            let m = RingMemory {
                counters: &*r.counters,
                samples: r.samples.as_mut_ptr(),
                capacity: capacity as u64,
                channels,
            };
            (r, m)
        }
    }

    #[test]
    fn frames_come_out_in_order_across_the_wrap() {
        let (_keep, mem) = HeapRing::new(8, 2);
        // SAFETY: `_keep` outlives both ends.
        let (mut w, mut r) = unsafe { ring(mem) };
        let mut next = 0.0f32;
        let mut expect = 0.0f32;
        for _ in 0..20 {
            let base = next;
            assert!(w.write_frames(3, |ch, f| base + f as f32 + ch as f32 * 1000.0));
            next += 3.0;
            assert!(r.read_frames(3, |ch, f, s| {
                assert_eq!(s, expect + f as f32 + ch as f32 * 1000.0);
            }));
            expect += 3.0;
        }
    }

    #[test]
    fn a_full_ring_refuses_writes_and_an_empty_one_refuses_reads() {
        let (_keep, mem) = HeapRing::new(4, 1);
        // SAFETY: `_keep` outlives both ends.
        let (mut w, mut r) = unsafe { ring(mem) };
        assert!(!r.read_frames(1, |_, _, _| {}));
        assert!(w.write_frames(4, |_, f| f as f32));
        assert_eq!(w.free_frames(), 0);
        assert!(!w.write_frames(1, |_, _| 9.0), "must not overwrite unread frames");
        assert!(r.read_frames(4, |_, f, s| assert_eq!(s, f as f32)));
        assert_eq!(r.available(), 0);
    }

    #[test]
    fn skipping_keeps_only_the_newest_frames() {
        let (_keep, mem) = HeapRing::new(16, 1);
        // SAFETY: `_keep` outlives both ends.
        let (mut w, mut r) = unsafe { ring(mem) };
        assert!(w.write_frames(10, |_, f| f as f32));
        r.skip(r.available() - 2);
        assert!(r.read_frames(2, |_, f, s| assert_eq!(s, 8.0 + f as f32)));
        assert!(w.write_frames(5, |_, f| f as f32));
        r.skip_all();
        assert_eq!(r.available(), 0);
    }
}
