//! Sparse matrix router (audio side) and its controller (control side).
//!
//! Gains travel through [`ParamTable`] (atomics). Topology travels as immutable
//! [`RoutingSnapshot`]s through a [`mailbox`]; the audio thread returns each
//! replaced snapshot on a second mailbox so it is freed on the control side.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::buffer::PlanarBuffer;
use crate::gain::PointParams;
use crate::mailbox::{self, Receiver, Sender};
use crate::params::ParamTable;

/// Maximum snapshots in flight between controller and router.
pub const SNAPSHOT_QUEUE: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Point {
    input: u32,
    cell: u32,
}

/// Immutable routing topology in CSR form: the points feeding output `o`
/// are `points[offsets[o]..offsets[o + 1]]`.
pub struct RoutingSnapshot {
    offsets: Vec<u32>,
    points: Vec<Point>,
    /// Cells removed since the previous snapshot; their ramp state is reset on swap.
    retired: Vec<u32>,
}

impl RoutingSnapshot {
    fn empty(num_outputs: usize) -> Self {
        Self { offsets: vec![0; num_outputs + 1], points: Vec::new(), retired: Vec::new() }
    }

    fn num_outputs(&self) -> usize {
        self.offsets.len() - 1
    }
}

#[derive(Clone, Copy, Default)]
struct Ramp {
    cur: f32,
    target: f32,
    step: f32,
}

/// Audio-thread half. `process` never allocates, locks or frees.
pub struct MatrixRouter {
    snapshot: Box<RoutingSnapshot>,
    params: Arc<ParamTable>,
    ramps: Box<[Ramp]>,
    ramp_samples: f32,
    inbox: Receiver<Box<RoutingSnapshot>>,
    graveyard: Sender<Box<RoutingSnapshot>>,
}

impl MatrixRouter {
    /// Mixes `inputs` into `outputs` for `outputs.frames()` frames.
    /// Output channels beyond the routing size are zeroed.
    pub fn process(&mut self, inputs: &PlanarBuffer, outputs: &mut PlanarBuffer) {
        while let Some(next) = self.inbox.try_recv() {
            for &cell in &next.retired {
                self.ramps[cell as usize] = Ramp::default();
            }
            let old = std::mem::replace(&mut self.snapshot, next);
            if let Err(old) = self.graveyard.try_send(old) {
                // Unreachable: the controller bounds snapshots in flight to the
                // queue capacity. Leaking beats freeing on the audio thread.
                std::mem::forget(old);
            }
        }

        let frames = outputs.frames();
        let Self { snapshot, params, ramps, ramp_samples, .. } = self;
        let num_outputs = snapshot.num_outputs();
        for o in 0..outputs.channels() {
            let out = outputs.channel_mut(o);
            out.fill(0.0);
            if o >= num_outputs || frames == 0 {
                continue;
            }
            let span = snapshot.offsets[o] as usize..snapshot.offsets[o + 1] as usize;
            for p in &snapshot.points[span] {
                if p.input as usize >= inputs.channels() {
                    continue;
                }
                let input = &inputs.channel(p.input as usize)[..frames];
                let target = params.get(p.cell);
                let r = &mut ramps[p.cell as usize];
                if target != r.target {
                    r.target = target;
                    r.step = (target - r.cur) / *ramp_samples;
                }
                if r.cur == r.target {
                    let g = r.cur;
                    if g != 0.0 {
                        for (y, x) in out.iter_mut().zip(input) {
                            *y += x * g;
                        }
                    }
                } else {
                    let (step, tgt) = (r.step, r.target);
                    let mut g = r.cur;
                    if step > 0.0 {
                        for (y, x) in out.iter_mut().zip(input) {
                            g = (g + step).min(tgt);
                            *y += x * g;
                        }
                    } else {
                        for (y, x) in out.iter_mut().zip(input) {
                            g = (g + step).max(tgt);
                            *y += x * g;
                        }
                    }
                    r.cur = g;
                }
            }
        }
    }
}

/// Control-thread half: owns the point list and publishes snapshots.
pub struct MatrixController {
    params: Arc<ParamTable>,
    /// Keyed (output, input) so iteration order is already CSR order.
    points: BTreeMap<(u32, u32), PointParams>,
    /// Points faded to silence, awaiting removal from the topology.
    fading_out: Vec<((u32, u32), Instant)>,
    retired: Vec<u32>,
    ramp: Duration,
    dirty: bool,
    in_flight: usize,
    to_audio: Sender<Box<RoutingSnapshot>>,
    graveyard: Receiver<Box<RoutingSnapshot>>,
}

/// Error returned when a point lies outside the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange;

/// Builds a connected controller/router pair.
/// `ramp` is the click-free fade time; `sample_rate` converts it to samples.
pub fn matrix(
    max_inputs: usize,
    max_outputs: usize,
    ramp: Duration,
    sample_rate: f32,
) -> (MatrixController, MatrixRouter) {
    let params = Arc::new(ParamTable::new(max_inputs, max_outputs));
    let (to_audio, inbox) = mailbox::channel(SNAPSHOT_QUEUE);
    let (grave_tx, graveyard) = mailbox::channel(SNAPSHOT_QUEUE);
    let router = MatrixRouter {
        snapshot: Box::new(RoutingSnapshot::empty(max_outputs)),
        params: params.clone(),
        ramps: vec![Ramp::default(); params.len()].into_boxed_slice(),
        ramp_samples: (ramp.as_secs_f32() * sample_rate).max(1.0),
        inbox,
        graveyard: grave_tx,
    };
    let controller = MatrixController {
        params,
        points: BTreeMap::new(),
        fading_out: Vec::new(),
        retired: Vec::new(),
        ramp,
        dirty: false,
        in_flight: 0,
        to_audio,
        graveyard,
    };
    (controller, router)
}

impl MatrixController {
    /// Adds or updates a point. Gain/mute/phase changes take effect immediately
    /// (ramped); a new point becomes audible after the next `tick`.
    pub fn set_point(&mut self, input: u32, output: u32, p: PointParams) -> Result<(), OutOfRange> {
        self.check(input, output)?;
        self.params.set(self.params.cell(input, output), p.effective_gain());
        self.fading_out.retain(|(k, _)| *k != (output, input));
        if self.points.insert((output, input), p).is_none() {
            self.dirty = true;
        }
        Ok(())
    }

    /// Fades a point to silence now and unlinks it after twice the ramp time.
    pub fn remove_point(&mut self, input: u32, output: u32) -> Result<(), OutOfRange> {
        self.remove_point_at(input, output, Instant::now())
    }

    /// As [`remove_point`](Self::remove_point) with an explicit clock, for tests.
    pub fn remove_point_at(&mut self, input: u32, output: u32, now: Instant) -> Result<(), OutOfRange> {
        self.check(input, output)?;
        let key = (output, input);
        if self.points.contains_key(&key) && !self.fading_out.iter().any(|(k, _)| *k == key) {
            self.params.set(self.params.cell(input, output), 0.0);
            self.fading_out.push((key, now + self.ramp * 2));
        }
        Ok(())
    }

    /// Current parameters of a point, if it exists and is not being removed.
    pub fn point(&self, input: u32, output: u32) -> Option<PointParams> {
        let key = (output, input);
        if self.fading_out.iter().any(|(k, _)| *k == key) {
            return None;
        }
        self.points.get(&key).copied()
    }

    /// All live points as (input, output, params), ordered by output then input.
    pub fn points(&self) -> Vec<(u32, u32, PointParams)> {
        self.points
            .iter()
            .filter(|(k, _)| !self.fading_out.iter().any(|(f, _)| f == *k))
            .map(|(&(o, i), &p)| (i, o, p))
            .collect()
    }

    /// Frees returned snapshots, finishes due removals and publishes pending changes.
    /// Call regularly from the control thread (e.g. every 10–20 ms).
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// As [`tick`](Self::tick) with an explicit clock, for tests.
    pub fn tick_at(&mut self, now: Instant) {
        while let Some(old) = self.graveyard.try_recv() {
            drop(old);
            self.in_flight -= 1;
        }
        let Self { fading_out, points, params, retired, dirty, .. } = self;
        fading_out.retain(|&((out, inp), due)| {
            if due > now {
                return true;
            }
            points.remove(&(out, inp));
            retired.push(params.cell(inp, out));
            *dirty = true;
            false
        });
        if self.dirty && self.in_flight < SNAPSHOT_QUEUE {
            self.publish();
        }
    }

    /// True when every change has been handed to the audio side.
    pub fn is_synced(&self) -> bool {
        !self.dirty
    }

    fn check(&self, input: u32, output: u32) -> Result<(), OutOfRange> {
        if input < self.params.max_inputs() && output < self.params.max_outputs() {
            Ok(())
        } else {
            Err(OutOfRange)
        }
    }

    fn publish(&mut self) {
        let num_outputs = self.params.max_outputs() as usize;
        let mut offsets = vec![0u32; num_outputs + 1];
        let mut pts = Vec::with_capacity(self.points.len());
        for &(out, inp) in self.points.keys() {
            pts.push(Point { input: inp, cell: self.params.cell(inp, out) });
            offsets[out as usize + 1] += 1;
        }
        for o in 0..num_outputs {
            offsets[o + 1] += offsets[o];
        }
        let snap = Box::new(RoutingSnapshot { offsets, points: pts, retired: std::mem::take(&mut self.retired) });
        match self.to_audio.try_send(snap) {
            Ok(()) => {
                self.in_flight += 1;
                self.dirty = false;
            }
            Err(snap) => {
                // Unreachable given the in-flight bound; keep the retired list for the retry.
                self.retired = snap.retired;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAMP: Duration = Duration::from_millis(10); // 480 samples at 48 kHz

    fn buffers(inputs: usize, outputs: usize, frames: usize) -> (PlanarBuffer, PlanarBuffer) {
        let mut i = PlanarBuffer::new(inputs, frames);
        let mut o = PlanarBuffer::new(outputs, frames);
        i.set_frames(frames);
        o.set_frames(frames);
        (i, o)
    }

    fn run_blocks(router: &mut MatrixRouter, i: &PlanarBuffer, o: &mut PlanarBuffer, n: usize) {
        for _ in 0..n {
            router.process(i, o);
        }
    }

    #[test]
    fn routes_with_gain_and_phase() {
        let (mut ctl, mut router) = matrix(2, 2, RAMP, 48_000.0);
        ctl.set_point(0, 1, PointParams { gain_db: -6.0206, mute: false, invert: true }).unwrap();
        ctl.tick();
        let (mut i, mut o) = buffers(2, 2, 64);
        i.channel_mut(0).fill(1.0);
        run_blocks(&mut router, &i, &mut o, 20);
        assert!(o.channel(0).iter().all(|&s| s == 0.0));
        assert!(o.channel(1).iter().all(|&s| (s + 0.5).abs() < 1e-4));
    }

    #[test]
    fn new_point_fades_in_linearly_without_clicks() {
        let (mut ctl, mut router) = matrix(1, 1, RAMP, 48_000.0);
        ctl.set_point(0, 0, PointParams::default()).unwrap();
        ctl.tick();
        let (mut i, mut o) = buffers(1, 1, 64);
        i.channel_mut(0).fill(1.0);
        let mut prev = 0.0f32;
        let mut max_step = 0.0f32;
        for _ in 0..10 {
            router.process(&i, &mut o);
            for &s in o.channel(0) {
                max_step = max_step.max((s - prev).abs());
                prev = s;
            }
        }
        assert!(max_step <= 1.0 / 480.0 + 1e-6, "step {max_step}");
        assert_eq!(prev, 1.0);
    }

    #[test]
    fn gain_change_needs_no_new_snapshot() {
        let (mut ctl, mut router) = matrix(1, 1, RAMP, 48_000.0);
        ctl.set_point(0, 0, PointParams::default()).unwrap();
        ctl.tick();
        let (mut i, mut o) = buffers(1, 1, 480);
        i.channel_mut(0).fill(1.0);
        run_blocks(&mut router, &i, &mut o, 2);
        ctl.set_point(0, 0, PointParams { gain_db: -6.0206, ..Default::default() }).unwrap();
        assert!(ctl.is_synced());
        run_blocks(&mut router, &i, &mut o, 2);
        assert!((o.channel(0)[479] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn removal_fades_then_unlinks_and_resets_ramp() {
        let (mut ctl, mut router) = matrix(1, 1, RAMP, 48_000.0);
        let t0 = Instant::now();
        ctl.set_point(0, 0, PointParams::default()).unwrap();
        ctl.tick_at(t0);
        let (mut i, mut o) = buffers(1, 1, 480);
        i.channel_mut(0).fill(1.0);
        run_blocks(&mut router, &i, &mut o, 2);
        ctl.remove_point_at(0, 0, t0).unwrap();
        assert_eq!(ctl.point(0, 0), None);
        run_blocks(&mut router, &i, &mut o, 1);
        assert_eq!(o.channel(0)[479], 0.0, "faded out within one ramp");
        ctl.tick_at(t0 + Duration::from_millis(5));
        assert!(ctl.is_synced(), "not yet due");
        ctl.tick_at(t0 + Duration::from_millis(25));
        run_blocks(&mut router, &i, &mut o, 1);
        // Re-adding starts from silence again (ramp state was reset).
        ctl.set_point(0, 0, PointParams::default()).unwrap();
        ctl.tick_at(t0 + Duration::from_millis(30));
        router.process(&i, &mut o);
        assert!((o.channel(0)[0] - 1.0 / 480.0).abs() < 1e-6);
    }

    #[test]
    fn controller_never_overfills_when_audio_is_stopped() {
        let (mut ctl, _router) = matrix(4, 4, RAMP, 48_000.0);
        for n in 0..(SNAPSHOT_QUEUE as u32 * 2) {
            ctl.set_point(n % 4, n / 4 % 4, PointParams::default()).unwrap();
            ctl.tick();
        }
        assert!(!ctl.is_synced(), "changes are held back, not dropped");
    }

    #[test]
    fn out_of_range_points_are_rejected() {
        let (mut ctl, _router) = matrix(2, 2, RAMP, 48_000.0);
        assert_eq!(ctl.set_point(2, 0, PointParams::default()), Err(OutOfRange));
        assert_eq!(ctl.remove_point(0, 5), Err(OutOfRange));
    }
}
