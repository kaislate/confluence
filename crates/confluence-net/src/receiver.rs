//! One received stream: puts packets back in order, reassembles blocks split
//! across packets, conceals losses, and hands whole blocks (with their arrival
//! time) to the slot's bridge, which does the clock recovery.
//!
//! Losses are concealed on time: from the earliest arrivals against the
//! sender's timestamps the receiver knows when each block is due, and a block
//! overdue by the wait is concealed then, without waiting for a later packet
//! (a burst of losses must not drain the bridge).

use crate::packet::Header;

/// Packet times a gap is at least waited for before it is concealed.
const HOLD_PACKETS: f64 = 2.0;
/// The longest a gap is waited for, however out of order packets arrive.
pub const MAX_HOLD_S: f64 = 0.020;
/// How much longer than the reordering seen a gap is waited for.
const HOLD_MARGIN: f64 = 1.25;
/// Per block handed on, the wait shrinks by this fraction back toward its
/// minimum: at 1 ms packets it halves in about 6 minutes, so a network that
/// misbehaves every few seconds or minutes keeps the wait it needs.
const HOLD_DECAY: f64 = 2e-6;
/// A packet this far ahead of the expected one (in seconds) means the sender's
/// clock jumped: follow it rather than conceal the gap.
const AHEAD_RESYNC_S: f64 = 0.2;
/// A packet this far behind (in seconds) means the sender restarted.
const BEHIND_RESYNC_S: f64 = 1.0;
/// Blocks held at most; beyond, the oldest gap is concealed at once.
const MAX_PENDING: usize = 64;
/// When blocks are due is judged from the earliest arrivals over the last one
/// to two of these (seconds): long enough to see the best case, short enough
/// to follow the sender's clock drifting against ours.
const DUE_WINDOW_S: f64 = 0.5;
/// Nothing arriving for this long (seconds) means the stream stopped: stop
/// concealing (the bridge then runs dry and restarts when it returns).
const MAX_CONCEAL_S: f64 = 0.5;
/// The share of packets expected to arrive within the spread: the rest (a
/// stall, a delay spike) are concealed if overdue, and teach the wait if they
/// then turn up.
const SPREAD_QUANTILE: f64 = 0.99;
/// How fast (seconds per packet) the spread follows the jitter seen.
const SPREAD_STEP_S: f64 = 2e-5;
/// The sender's timeline is re-anchored this often (in frames), well within
/// the wrapping timestamp's range.
const REANCHOR_FRAMES: i64 = 1 << 24;

/// What a receiver has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    pub packets: u64,
    /// Blocks concealed (never arrived, or arrived incomplete).
    pub lost: u64,
    /// Packets that arrived after their block was concealed.
    pub late: u64,
    /// Packets that arrived after a later one.
    pub reordered: u64,
    /// Times the sender's timeline was picked up afresh.
    pub resyncs: u64,
    /// Packets at a rate or block size this receiver does not take.
    pub mismatched: u64,
}

struct Pending {
    ts: u32,
    frames: usize,
    data: Vec<f32>,
    got: u64,
    need: u64,
    first_arrival: f64,
    last_arrival: f64,
}

/// Reorders, reassembles and conceals one stream for a slot of `channels`.
pub struct Receiver {
    channels: usize,
    rate: u32,
    expected: Option<u32>,
    /// The newest block seen, and when it arrived.
    newest: Option<(u32, f64)>,
    /// How long a gap is waited for (learned from the reordering seen).
    hold: f64,
    /// The last gap concealed: its blocks `start..end` and when the block after
    /// it arrived (to learn how late its packets turn up).
    concealed: Option<(u32, u32, f64)>,
    pending: Vec<Pending>,
    /// The last block that arrived whole (what concealment repeats).
    last: Vec<f32>,
    /// The last block handed on (what a returning stream crossfades from).
    prev: Vec<f32>,
    /// Blocks concealed in a row.
    lost_run: u32,
    /// The time the last block was handed on with: the bridge recovers the
    /// sender's clock from these, so they never go backwards.
    last_time: f64,
    /// When blocks are due, from arrival offsets `arrival - (ts - anchor) / rate`.
    due: Due,
    /// Frames per block (from the last packet).
    block_frames: usize,
    /// Since when blocks have been concealed on time with nothing arriving.
    concealing_since: Option<f64>,
    /// Blocks `start..end` concealed on time (late packets of these teach the wait).
    timed: Option<(u32, u32)>,
    stats: ReceiverStats,
}

/// Arrival offsets (arrival minus the sender's time since the anchor): the
/// earliest over a sliding window, and how much later than that packets come.
struct Due {
    anchor: u32,
    /// When the current window began (`None` before the first packet).
    since: Option<f64>,
    current: f64,
    previous: f64,
    /// How much later than the earliest most packets arrive (a running
    /// `SPREAD_QUANTILE` estimate).
    spread: f64,
}

impl Default for Due {
    fn default() -> Due {
        Due { anchor: 0, since: None, current: f64::INFINITY, previous: f64::INFINITY, spread: 0.0 }
    }
}

impl Due {
    /// The earliest offset over the last one to two windows; infinite until
    /// a whole window has been seen.
    fn earliest(&self) -> f64 {
        if self.previous.is_finite() {
            self.current.min(self.previous)
        } else {
            f64::INFINITY
        }
    }

    fn observe(&mut self, offset: f64, arrival: f64) {
        let since = *self.since.get_or_insert(arrival);
        if arrival - since >= DUE_WINDOW_S {
            (self.previous, self.current, self.since) = (self.current, offset, Some(arrival));
        } else {
            self.current = self.current.min(offset);
        }
        let earliest = self.current.min(self.previous);
        let step = if offset - earliest > self.spread { SPREAD_QUANTILE } else { SPREAD_QUANTILE - 1.0 };
        self.spread = (self.spread + SPREAD_STEP_S * step).clamp(0.0, MAX_HOLD_S);
    }

    /// The anchor moves `by` seconds later on the sender's timeline.
    fn shift(&mut self, by: f64) {
        self.current += by;
        self.previous += by;
    }
}

fn mask(channels: u8) -> u64 {
    if channels >= 64 {
        u64::MAX
    } else {
        (1u64 << channels) - 1
    }
}

/// `a - b` on the wrapping RTP timeline.
fn diff(a: u32, b: u32) -> i64 {
    a.wrapping_sub(b) as i32 as i64
}

impl Receiver {
    pub fn new(channels: usize, rate: u32) -> Receiver {
        Receiver {
            channels: channels.max(1),
            rate,
            expected: None,
            newest: None,
            hold: 0.0,
            concealed: None,
            pending: Vec::new(),
            last: Vec::new(),
            prev: Vec::new(),
            lost_run: 0,
            last_time: f64::NEG_INFINITY,
            due: Due::default(),
            block_frames: 0,
            concealing_since: None,
            timed: None,
            stats: ReceiverStats::default(),
        }
    }

    pub fn stats(&self) -> ReceiverStats {
        self.stats
    }

    /// How long a gap is waited for now, in seconds.
    pub fn hold(&self) -> f64 {
        self.hold
    }

    /// Frames the slot's bridge must hold beyond its base: a block can be
    /// handed on as late as the spread plus the wait after its earliest
    /// arrival (see `InputDeviceSide::set_latency_floor`).
    pub fn latency_floor(&self) -> f64 {
        let hold = self.hold.max(HOLD_PACKETS * self.block_frames as f64 / self.rate as f64);
        (self.due.spread + hold) * self.rate as f64
    }

    /// A packet turned up `late` seconds after a newer one: wait that long (and a bit).
    fn learn(&mut self, late: f64, frames: usize) {
        let min = HOLD_PACKETS * frames as f64 / self.rate as f64;
        self.hold = (late * HOLD_MARGIN).max(self.hold).clamp(min, MAX_HOLD_S);
    }

    fn resync(&mut self, ts: u32) {
        self.pending.clear();
        self.expected = Some(ts);
        self.newest = None;
        self.due = Due::default();
        self.timed = None;
        self.stats.resyncs += 1;
    }

    /// Seconds from the anchor to `ts` on the sender's timeline.
    fn since_anchor(&self, ts: u32) -> f64 {
        diff(ts, self.due.anchor) as f64 / self.rate as f64
    }

    /// When block `ts` arrives at the latest without a stall, once known.
    fn due_at(&self, ts: u32) -> Option<f64> {
        let earliest = self.due.earliest();
        earliest.is_finite().then(|| earliest + self.since_anchor(ts) + self.due.spread)
    }

    /// Learns from a packet's arrival when blocks are due.
    fn observe(&mut self, ts: u32, arrival: f64) {
        if self.due.since.is_none() {
            self.due.anchor = ts;
        } else if diff(ts, self.due.anchor).abs() > REANCHOR_FRAMES {
            let shift = self.since_anchor(ts);
            self.due.shift(shift);
            self.due.anchor = ts;
        }
        let offset = arrival - self.since_anchor(ts);
        self.due.observe(offset, arrival);
    }

    /// Takes a packet (`samples` interleaved, `h.channels` per frame) that
    /// arrived at `arrival` seconds; hands on whatever is now in order.
    pub fn push(&mut self, h: &Header, samples: &[f32], arrival: f64, out: &mut dyn FnMut(&[f32], f64)) {
        self.stats.packets += 1;
        let pc = h.channels as usize;
        if h.rate != self.rate || pc == 0 || !samples.len().is_multiple_of(pc) {
            self.stats.mismatched += 1;
            return;
        }
        let frames = samples.len() / pc;
        let ts = h.timestamp;
        match self.expected {
            None => self.expected = Some(ts),
            Some(e) => {
                let d = diff(ts, e);
                if d < -(BEHIND_RESYNC_S * self.rate as f64) as i64 || d > (AHEAD_RESYNC_S * self.rate as f64) as i64 {
                    self.resync(ts);
                } else if d < 0 {
                    self.stats.late += 1;
                    if let Some((start, end, after)) = self.concealed {
                        if diff(ts, start) >= 0 && diff(end, ts) > 0 {
                            self.learn(arrival - after, frames);
                        }
                    }
                    if let (Some((start, end)), Some(due)) = (self.timed, self.due_at(ts)) {
                        if diff(ts, start) >= 0 && diff(end, ts) > 0 {
                            self.learn(arrival - due, frames);
                        }
                    }
                    return;
                }
            }
        }
        self.observe(ts, arrival);
        self.block_frames = frames;
        self.concealing_since = None;
        match self.newest {
            Some((n, at)) if diff(ts, n) < 0 => {
                self.stats.reordered += 1;
                self.learn(arrival - at, frames);
            }
            _ => self.newest = Some((ts, arrival)),
        }
        let c = self.channels;
        let i = match self.pending.iter().position(|p| p.ts == ts) {
            Some(i) => i,
            None => {
                self.pending.push(Pending {
                    ts,
                    frames,
                    data: vec![0.0; frames * c],
                    got: 0,
                    need: mask(h.total_channels),
                    first_arrival: arrival,
                    last_arrival: arrival,
                });
                self.pending.len() - 1
            }
        };
        let p = &mut self.pending[i];
        if p.frames != frames {
            self.stats.mismatched += 1;
            return;
        }
        for k in 0..pc {
            let sc = h.first_channel as usize + k;
            if sc < 64 {
                p.got |= 1 << sc;
            }
            if sc < c {
                for f in 0..frames {
                    p.data[f * c + sc] = samples[f * pc + k];
                }
            }
        }
        p.last_arrival = arrival;
        self.release(arrival, out);
    }

    /// Conceals gaps that have waited long enough (call every millisecond or so).
    pub fn poll(&mut self, now: f64, out: &mut dyn FnMut(&[f32], f64)) {
        self.release(now, out);
    }

    fn release(&mut self, now: f64, out: &mut dyn FnMut(&[f32], f64)) {
        while let Some(e) = self.expected {
            if let Some(i) = self.pending.iter().position(|p| p.ts == e && p.got & p.need == p.need) {
                let p = self.pending.swap_remove(i);
                self.expected = Some(e.wrapping_add(p.frames as u32));
                self.emit_real(p.data, p.last_arrival, out);
                continue;
            }
            let Some(i) = (0..self.pending.len()).min_by_key(|&i| diff(self.pending[i].ts, e)) else {
                if self.conceal_overdue(e, None, now, out) {
                    continue;
                }
                return;
            };
            let p = &self.pending[i];
            let hold = self.hold.max(HOLD_PACKETS * p.frames as f64 / self.rate as f64);
            if now < p.first_arrival + hold && self.pending.len() <= MAX_PENDING {
                let next = (p.ts != e).then_some(p.ts);
                if self.conceal_overdue(e, next, now, out) {
                    continue;
                }
                return;
            }
            if p.ts == e {
                // Waited long enough and still incomplete: what came is used.
                let p = self.pending.swap_remove(i);
                self.stats.lost += 1;
                self.expected = Some(e.wrapping_add(p.frames as u32));
                self.emit_real(p.data, p.last_arrival, out);
                continue;
            }
            let (gap, frames, next) = (diff(p.ts, e) as usize, p.frames, p.first_arrival);
            self.concealed = Some((e, p.ts, next));
            // The missing blocks would have arrived before the next one: stamp
            // them spread out up to its arrival, not with the (later) time now,
            // or the bridge would see the sender's clock jump back and forth.
            let start = if self.last_time.is_finite() { self.last_time.min(next) } else { next };
            let blocks = gap.div_ceil(frames);
            let mut done = 0;
            for k in 1..=blocks {
                let n = frames.min(gap - done);
                self.emit_concealed(n, start + (next - start) * k as f64 / blocks as f64, out);
                done += n;
            }
            self.expected = Some(e.wrapping_add(gap as u32));
        }
    }

    /// Conceals block `e` if it is overdue (and the stream has not stopped),
    /// up to `next` (the next block held, if any). Returns whether it did.
    fn conceal_overdue(&mut self, e: u32, next: Option<u32>, now: f64, out: &mut dyn FnMut(&[f32], f64)) -> bool {
        let Some(due) = self.due_at(e) else { return false };
        let frames = self.block_frames;
        let hold = self.hold.max(HOLD_PACKETS * frames as f64 / self.rate as f64);
        if frames == 0 || now < due + hold {
            return false;
        }
        let since = *self.concealing_since.get_or_insert(now);
        if now - since > MAX_CONCEAL_S {
            return false;
        }
        let n = next.map_or(frames, |t| frames.min(diff(t, e).max(0) as usize));
        if n == 0 {
            return false;
        }
        let end = e.wrapping_add(n as u32);
        self.timed = match self.timed {
            Some((start, prev_end)) if prev_end == e => Some((start, end)),
            _ => Some((e, end)),
        };
        self.emit_concealed(n, due.min(now), out);
        self.expected = Some(end);
        true
    }

    fn emit_real(&mut self, mut data: Vec<f32>, time: f64, out: &mut dyn FnMut(&[f32], f64)) {
        let c = self.channels;
        if self.lost_run > 0 {
            // Crossfade from what was played in its place.
            let frames = data.len() / c;
            for f in 0..frames {
                let w = (f + 1) as f32 / frames as f32;
                for ch in 0..c {
                    let from = self.prev.get(f * c + ch).copied().unwrap_or(0.0);
                    data[f * c + ch] = from * (1.0 - w) + data[f * c + ch] * w;
                }
            }
            self.lost_run = 0;
        }
        self.hold *= 1.0 - HOLD_DECAY;
        self.last_time = self.last_time.max(time);
        out(&data, self.last_time);
        self.last.clone_from(&data);
        self.prev = data;
    }

    fn emit_concealed(&mut self, frames: usize, time: f64, out: &mut dyn FnMut(&[f32], f64)) {
        let c = self.channels;
        self.lost_run += 1;
        self.stats.lost += 1;
        let mut data = vec![0.0; frames * c];
        if self.lost_run <= 3 && !self.last.is_empty() {
            for (i, d) in data.iter_mut().enumerate() {
                *d = self.last[i % self.last.len()];
            }
            if self.lost_run == 3 {
                for f in 0..frames {
                    let g = 1.0 - (f + 1) as f32 / frames as f32;
                    for d in &mut data[f * c..(f + 1) * c] {
                        *d *= g;
                    }
                }
            }
        }
        self.last_time = self.last_time.max(time);
        out(&data, self.last_time);
        self.prev = data;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{Format, Header};

    const FRAMES: usize = 48;

    fn header(ts: u32, first: u8, channels: u8, total: u8) -> Header {
        Header {
            seq: 0,
            timestamp: ts,
            ssrc: 7,
            format: Format::F32,
            total_channels: total,
            first_channel: first,
            channels,
            rate: 48_000,
            stream: "Main".into(),
        }
    }

    /// A block whose every sample is `v` (per packet channel count).
    fn block(v: f32, channels: usize) -> Vec<f32> {
        vec![v; FRAMES * channels]
    }

    /// Collects what the receiver releases: (first sample of each frame of channel 0, time).
    #[derive(Default)]
    struct Out {
        frames: Vec<f32>,
        all: Vec<f32>,
        times: Vec<f64>,
    }

    impl Out {
        fn sink(&mut self, channels: usize) -> impl FnMut(&[f32], f64) + '_ {
            move |data, t| {
                self.frames.extend(data.chunks(channels).map(|f| f[0]));
                self.all.extend_from_slice(data);
                self.times.push(t);
            }
        }
    }

    fn rx(channels: usize) -> Receiver {
        Receiver::new(channels, 48_000)
    }

    const PT: f64 = 0.001; // one packet time

    #[test]
    fn packets_in_order_are_released_at_once() {
        let mut r = rx(2);
        let mut out = Out::default();
        r.push(&header(1000, 0, 2, 2), &block(0.1, 2), 0.0, &mut out.sink(2));
        r.push(&header(1048, 0, 2, 2), &block(0.2, 2), PT, &mut out.sink(2));
        assert_eq!(out.frames.len(), 96);
        assert_eq!((out.frames[0], out.frames[95]), (0.1, 0.2));
        assert_eq!(out.times, [0.0, PT]);
        assert_eq!(r.stats().packets, 2);
    }

    #[test]
    fn a_block_split_across_packets_is_released_when_whole() {
        let mut r = rx(4);
        let mut out = Out::default();
        r.push(&header(0, 2, 2, 4), &block(0.3, 2), 0.0, &mut out.sink(4));
        assert!(out.all.is_empty(), "half a block");
        r.push(&header(0, 0, 2, 4), &block(0.1, 2), 0.0001, &mut out.sink(4));
        assert_eq!(out.all.len(), FRAMES * 4);
        assert_eq!(&out.all[..4], &[0.1, 0.1, 0.3, 0.3]);
    }

    #[test]
    fn channels_are_fitted_to_the_slot() {
        let mut wide = rx(4);
        let mut out = Out::default();
        wide.push(&header(0, 0, 2, 2), &block(0.5, 2), 0.0, &mut out.sink(4));
        assert_eq!(&out.all[..4], &[0.5, 0.5, 0.0, 0.0], "missing channels are silent");
        let mut narrow = rx(1);
        let mut out = Out::default();
        narrow.push(&header(0, 0, 2, 2), &block(0.5, 2), 0.0, &mut out.sink(1));
        assert_eq!(out.all.len(), FRAMES, "extra channels are dropped");
    }

    #[test]
    fn reordered_packets_are_put_back_in_order() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(0, 0, 1, 1), &block(0.1, 1), 0.0, &mut out.sink(1));
        r.push(&header(96, 0, 1, 1), &block(0.3, 1), PT, &mut out.sink(1));
        r.push(&header(48, 0, 1, 1), &block(0.2, 1), 1.2 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 144);
        assert_eq!((out.frames[0], out.frames[48], out.frames[96]), (0.1, 0.2, 0.3));
        assert_eq!(r.stats().reordered, 1);
        assert_eq!(r.stats().lost, 0);
    }

    #[test]
    fn a_lost_packet_is_concealed_after_a_short_wait() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(0, 0, 1, 1), &block(0.1, 1), 0.0, &mut out.sink(1));
        r.push(&header(96, 0, 1, 1), &block(0.3, 1), 2.0 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 48, "held while 48 may still come");
        r.poll(4.0 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 144, "concealed, then released");
        assert_eq!(out.frames[50], 0.1, "the last block repeated");
        assert!(out.frames[96] < 0.3 && out.frames[96] > 0.1, "crossfaded into the next");
        assert_eq!(out.frames[143], 0.3);
        assert_eq!(r.stats().lost, 1);
        // It turns up after all: too late.
        r.push(&header(48, 0, 1, 1), &block(0.2, 1), 4.5 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 144);
        assert_eq!(r.stats().late, 1);
    }

    #[test]
    fn a_long_loss_fades_to_silence() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(0, 0, 1, 1), &block(0.5, 1), 0.0, &mut out.sink(1));
        r.push(&header(6 * 48, 0, 1, 1), &block(0.5, 1), 6.0 * PT, &mut out.sink(1));
        r.poll(9.0 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 7 * 48, "five concealed blocks keep the timing");
        assert_eq!(out.frames[48], 0.5, "first two repeat");
        assert_eq!(out.frames[2 * 48], 0.5);
        assert!(out.frames[3 * 48 + 47] < 0.05, "the third fades out");
        assert_eq!(out.frames[4 * 48], 0.0, "then silence");
        assert_eq!(r.stats().lost, 5);
    }

    #[test]
    fn a_restarted_sender_is_followed_without_a_flood_of_concealment() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(1_000_000, 0, 1, 1), &block(0.1, 1), 0.0, &mut out.sink(1));
        // Restarted: timestamps begin again far behind.
        r.push(&header(5, 0, 1, 1), &block(0.2, 1), 2.0, &mut out.sink(1));
        r.push(&header(53, 0, 1, 1), &block(0.3, 1), 2.0 + PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 144);
        // And far ahead (a long pause in the sender's clock).
        r.push(&header(9_000_000, 0, 1, 1), &block(0.4, 1), 3.0, &mut out.sink(1));
        assert_eq!(out.frames.len(), 192);
        assert_eq!(r.stats().lost, 0);
        assert_eq!(r.stats().resyncs, 2);
    }

    #[test]
    fn the_wait_for_a_gap_grows_with_the_reordering_seen() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(0, 0, 1, 1), &block(0.1, 1), 0.0, &mut out.sink(1));
        let mut t = PT;
        let mut lost_after_learning = None;
        for i in 0..40u32 {
            let (a, b) = ((2 * i + 1) * 48, (2 * i + 2) * 48);
            // B overtakes A by 3 packet times on the way.
            r.push(&header(b, 0, 1, 1), &block(0.2, 1), t, &mut out.sink(1));
            for k in 1..=6 {
                r.poll(t + k as f64 * 0.5 * PT, &mut out.sink(1));
            }
            r.push(&header(a, 0, 1, 1), &block(0.1, 1), t + 3.0 * PT, &mut out.sink(1));
            t += 2.0 * PT + 3.0 * PT;
            if i == 3 {
                lost_after_learning = Some(r.stats().lost);
            }
        }
        assert!(lost_after_learning.unwrap() <= 2, "{:?}", r.stats());
        assert_eq!(r.stats().lost, lost_after_learning.unwrap(), "learned: no more losses {:?}", r.stats());
        assert!(r.hold() > 3.0 * PT && r.hold() <= MAX_HOLD_S, "{}", r.hold());
        assert!(r.latency_floor() >= r.hold() * 48_000.0, "the bridge must cover the wait");
    }

    /// Steady 1 ms packets (jittering by up to `jitter` packet times) from block 0
    /// for `seconds`; returns the next block's timestamp and arrival time.
    fn steady(r: &mut Receiver, out: &mut Out, seconds: f64, jitter: f64) -> (u32, f64) {
        let mut rng = 0x1234_5678u64;
        let n = (seconds / PT) as u32;
        for i in 0..n {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (rng >> 33) as f64 / (1u64 << 31) as f64 * jitter * PT;
            let t = i as f64 * PT + j;
            r.push(&header(i * 48, 0, 1, 1), &block(0.1, 1), t, &mut out.sink(1));
            r.poll(t, &mut out.sink(1));
        }
        (n * 48, n as f64 * PT)
    }

    #[test]
    fn a_long_loss_is_concealed_on_time_without_waiting_for_the_next_packet() {
        let mut r = rx(1);
        let mut out = Out::default();
        let (ts, t0) = steady(&mut r, &mut out, 2.0, 0.5);
        let before = out.frames.len();
        // Nothing arrives for 30 ms: the gap is filled as it goes, a few ms behind.
        for k in 1..=30 {
            r.poll(t0 + k as f64 * PT, &mut out.sink(1));
        }
        let filled = (out.frames.len() - before) / 48;
        assert!((25..=30).contains(&filled), "{filled} of 30 blocks filled in on time");
        assert!(out.times.windows(2).all(|w| w[1] >= w[0]), "times never go back");
        let last = *out.times.last().unwrap();
        assert!(last <= t0 + 30.0 * PT && last > t0 + 20.0 * PT, "stamped on the sender's timeline: {last}");
        // The stream comes back where it should be: no extra latency, nothing late.
        let next = ts + 30 * 48;
        r.push(&header(next, 0, 1, 1), &block(0.2, 1), t0 + 30.0 * PT, &mut out.sink(1));
        r.poll(t0 + 32.5 * PT, &mut out.sink(1));
        assert_eq!(out.frames.len() - before, 31 * 48);
        assert_eq!(r.stats().late, 0);
    }

    #[test]
    fn a_stream_that_stops_is_not_concealed_for_ever() {
        let mut r = rx(1);
        let mut out = Out::default();
        let (_, t0) = steady(&mut r, &mut out, 2.0, 0.0);
        for k in 1..=3000 {
            r.poll(t0 + k as f64 * PT, &mut out.sink(1));
        }
        let concealed = out.frames.len() / 48 - 2000;
        assert!((400..=600).contains(&concealed), "{concealed} blocks concealed after the stream stopped");
    }

    #[test]
    fn a_stall_that_delivers_late_packets_teaches_the_wait() {
        let mut r = rx(1);
        let mut out = Out::default();
        let (ts, t0) = steady(&mut r, &mut out, 2.0, 0.0);
        // The link stalls for 12 ms, then everything queued arrives at once.
        for k in 0..12u32 {
            r.poll(t0 + k as f64 * PT, &mut out.sink(1));
        }
        for k in 0..12u32 {
            r.push(&header(ts + k * 48, 0, 1, 1), &block(0.1, 1), t0 + 12.0 * PT, &mut out.sink(1));
        }
        assert!(r.stats().late > 0, "concealed while stalled: {:?}", r.stats());
        assert!(r.hold() >= 0.010, "the wait covers a stall like that now: {}", r.hold());
    }

    #[test]
    fn packets_at_another_rate_are_refused() {
        let mut r = rx(1);
        let mut out = Out::default();
        let h = Header { rate: 44_100, ..header(0, 0, 1, 1) };
        r.push(&h, &block(0.1, 1), 0.0, &mut out.sink(1));
        assert!(out.frames.is_empty());
        assert_eq!(r.stats().mismatched, 1);
    }

    #[test]
    fn timestamps_wrap_around() {
        let mut r = rx(1);
        let mut out = Out::default();
        r.push(&header(u32::MAX - 47, 0, 1, 1), &block(0.1, 1), 0.0, &mut out.sink(1));
        r.push(&header(0, 0, 1, 1), &block(0.2, 1), PT, &mut out.sink(1));
        assert_eq!(out.frames.len(), 96);
        assert_eq!(r.stats().resyncs, 0);
    }
}
