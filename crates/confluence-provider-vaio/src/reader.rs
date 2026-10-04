//! The audio thread's side of the ring: one engine block per engine block.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::region::Region;
use crate::VaioStats;

/// 32-bit signed PCM (the endpoint's format) to float, full scale = 1.0.
fn pcm_to_f32(s: i32) -> f32 {
    s as f32 / 2_147_483_648.0
}

pub struct Reader {
    region: Arc<Region>,
    stats: Arc<VaioStats>,
    /// Frames taken, ever (the engine owns `read_frames`).
    read: u64,
    /// A block came through since the app stream started; only then is a
    /// dry ring an underrun.
    primed: bool,
    /// In a dry spell (counted once).
    dry: bool,
}

impl Reader {
    pub fn new(region: Arc<Region>, stats: Arc<VaioStats>) -> Self {
        Reader { region, stats, read: 0, primed: false, dry: false }
    }

    /// Takes `block` frames, calling `out(channel, frame, sample)`. Returns
    /// false (and takes nothing) when fewer are queued. Real-time safe.
    pub fn read(&mut self, block: usize, mut out: impl FnMut(usize, usize, f32)) -> bool {
        let h = self.region.header();
        h.engine_heartbeat.fetch_add(1, Ordering::Release);
        let streaming = h.streaming.load(Ordering::Acquire) != 0;
        self.stats.streaming.store(streaming, Ordering::Relaxed);
        if !streaming {
            self.primed = false;
        }
        let written = h.write_frames.load(Ordering::Acquire);
        let queued = written.wrapping_sub(self.read);
        if queued > u64::from(self.region.capacity()) {
            // Nonsense (or a restart): don't trust it, pick up from the writer.
            self.read = written;
            h.read_frames.store(self.read, Ordering::Release);
            return false;
        }
        if queued < block as u64 {
            if streaming && self.primed && !self.dry {
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
            self.dry = self.primed;
            return false;
        }
        for f in 0..block {
            let [l, r] = self.region.frame(self.read + f as u64);
            out(0, f, pcm_to_f32(l));
            out(1, f, pcm_to_f32(r));
        }
        self.read += block as u64;
        h.read_frames.store(self.read, Ordering::Release);
        self.primed = streaming;
        self.dry = false;
        true
    }
}
