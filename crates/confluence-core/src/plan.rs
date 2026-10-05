//! The execution plan: the order in which output columns are mixed and insert
//! buses are run each block. Compiled on the control side, swapped in on the
//! audio thread like a routing snapshot, and returned there to be freed.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::mailbox::{self, Receiver, Sender};

/// Maximum plans in flight between controller and runner.
pub const PLAN_QUEUE: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Zero and mix these output columns.
    Mix(Range<u32>),
    /// Run the bus with this id: its sends → its returns.
    Bus(u32),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionPlan {
    steps: Vec<Step>,
}

impl ExecutionPlan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The plan with no buses: every column in one pass.
    pub fn mix_all(max_outputs: u32) -> Self {
        ExecutionPlan { steps: vec![Step::Mix(0..max_outputs)] }
    }
}

/// A bus as the planner sees it: its send columns and return rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusSpan {
    pub id: u32,
    pub sends: Range<u32>,
    pub returns: Range<u32>,
}

/// The routes make some bus feed itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cycle;

/// Bus ids in an order where every bus comes after the buses feeding it.
/// A point (input, output) from bus A's returns to bus B's sends is an edge
/// A → B. Ties go to the lower id, so equal inputs give equal plans.
pub fn order<I: IntoIterator<Item = (u32, u32)>>(buses: &[BusSpan], points: I) -> Result<Vec<u32>, Cycle> {
    let n = buses.len();
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut fed_by = vec![0usize; n];
    let mut edges = BTreeSet::new();
    for (input, output) in points {
        let from = buses.iter().position(|b| b.returns.contains(&input));
        let to = buses.iter().position(|b| b.sends.contains(&output));
        if let (Some(a), Some(b)) = (from, to) {
            if a == b {
                return Err(Cycle);
            }
            if edges.insert((a, b)) {
                next[a].push(b);
                fed_by[b] += 1;
            }
        }
    }
    let mut ready: BTreeSet<(u32, usize)> = (0..n).filter(|&i| fed_by[i] == 0).map(|i| (buses[i].id, i)).collect();
    let mut out = Vec::with_capacity(n);
    while let Some((id, i)) = ready.pop_first() {
        out.push(id);
        for &j in &next[i] {
            fed_by[j] -= 1;
            if fed_by[j] == 0 {
                ready.insert((buses[j].id, j));
            }
        }
    }
    if out.len() == n {
        Ok(out)
    } else {
        Err(Cycle)
    }
}

/// For each bus in `order`: mix its sends, then run it. Then mix every column
/// no bus sends to, so each column in `0..max_outputs` is mixed exactly once.
pub fn compile(buses: &[BusSpan], order: &[u32], max_outputs: u32) -> ExecutionPlan {
    let clamp = |r: &Range<u32>| r.start.min(max_outputs)..r.end.min(max_outputs);
    let mut steps = Vec::with_capacity(3 * buses.len() + 1);
    for id in order {
        if let Some(b) = buses.iter().find(|b| b.id == *id) {
            let sends = clamp(&b.sends);
            if !sends.is_empty() {
                steps.push(Step::Mix(sends));
            }
            steps.push(Step::Bus(*id));
        }
    }
    let mut sends: Vec<Range<u32>> = buses.iter().map(|b| clamp(&b.sends)).collect();
    sends.sort_by_key(|r| r.start);
    let mut at = 0;
    for r in sends {
        if r.start > at {
            steps.push(Step::Mix(at..r.start));
        }
        at = at.max(r.end);
    }
    if at < max_outputs {
        steps.push(Step::Mix(at..max_outputs));
    }
    ExecutionPlan { steps }
}

/// Control half: holds the latest plan until the audio side has room for it.
pub struct PlanController {
    to_audio: Sender<Box<ExecutionPlan>>,
    graveyard: Receiver<Box<ExecutionPlan>>,
    in_flight: usize,
    pending: Option<Box<ExecutionPlan>>,
}

impl PlanController {
    /// Replaces the plan; it reaches the audio side on the next `tick`.
    pub fn set(&mut self, plan: ExecutionPlan) {
        self.pending = Some(Box::new(plan));
    }

    /// Frees returned plans and sends the pending one if there is room.
    pub fn tick(&mut self) {
        while let Some(old) = self.graveyard.try_recv() {
            drop(old);
            self.in_flight -= 1;
        }
        if self.in_flight < PLAN_QUEUE {
            if let Some(p) = self.pending.take() {
                match self.to_audio.try_send(p) {
                    Ok(()) => self.in_flight += 1,
                    Err(p) => self.pending = Some(p),
                }
            }
        }
    }

    /// True when the latest plan has been handed to the audio side.
    pub fn is_synced(&self) -> bool {
        self.pending.is_none()
    }
}

/// Audio half: the current plan. Never allocates or frees.
pub struct PlanRunner {
    plan: Box<ExecutionPlan>,
    inbox: Receiver<Box<ExecutionPlan>>,
    graveyard: Sender<Box<ExecutionPlan>>,
}

impl PlanRunner {
    /// Adopts the newest delivered plan, returning replaced ones.
    pub fn begin_block(&mut self) {
        while let Some(next) = self.inbox.try_recv() {
            let old = std::mem::replace(&mut self.plan, next);
            if let Err(old) = self.graveyard.try_send(old) {
                // Unreachable: in-flight plans are bounded by the queue capacity.
                // Leaking beats freeing on the audio thread.
                std::mem::forget(old);
            }
        }
    }

    pub fn steps(&self) -> &[Step] {
        self.plan.steps()
    }
}

/// A connected controller/runner pair, starting with [`ExecutionPlan::mix_all`].
pub fn plan(max_outputs: u32) -> (PlanController, PlanRunner) {
    let (to_audio, inbox) = mailbox::channel(PLAN_QUEUE);
    let (grave_tx, graveyard) = mailbox::channel(PLAN_QUEUE);
    (
        PlanController { to_audio, graveyard, in_flight: 0, pending: None },
        PlanRunner { plan: Box::new(ExecutionPlan::mix_all(max_outputs)), inbox, graveyard: grave_tx },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use Step::*;

    fn bus(id: u32, sends: Range<u32>, returns: Range<u32>) -> BusSpan {
        BusSpan { id, sends, returns }
    }

    #[test]
    fn no_buses_is_one_mix_over_everything() {
        assert_eq!(compile(&[], &[], 16).steps(), &[Mix(0..16)]);
        assert_eq!(ExecutionPlan::mix_all(16).steps(), &[Mix(0..16)]);
    }

    #[test]
    fn a_bus_mixes_its_sends_runs_then_the_rest_is_mixed() {
        let b = [bus(7, 4..6, 10..12)];
        let o = order(&b, []).unwrap();
        assert_eq!(compile(&b, &o, 16).steps(), &[Mix(4..6), Bus(7), Mix(0..4), Mix(6..16)]);
    }

    #[test]
    fn complements_at_the_edges_and_between_adjacent_buses() {
        let b = [bus(1, 0..2, 0..2), bus(2, 2..4, 2..4), bus(3, 14..16, 4..6)];
        let o = order(&b, []).unwrap();
        assert_eq!(o, vec![1, 2, 3], "ties by id");
        assert_eq!(
            compile(&b, &o, 16).steps(),
            &[Mix(0..2), Bus(1), Mix(2..4), Bus(2), Mix(14..16), Bus(3), Mix(4..14)]
        );
    }

    #[test]
    fn a_bus_feeding_another_runs_first_whatever_the_ids() {
        // bus 9 returns (rows 20..22) → bus 1 sends (cols 0..2)
        let b = [bus(1, 0..2, 0..2), bus(9, 8..10, 20..22)];
        assert_eq!(order(&b, [(20, 1)]).unwrap(), vec![9, 1]);
    }

    #[test]
    fn loops_are_cycles() {
        let b = [bus(1, 0..2, 0..2), bus(2, 2..4, 2..4)];
        assert_eq!(order(&b, [(0, 1)]), Err(Cycle), "a bus into itself");
        assert_eq!(order(&b, [(0, 2), (3, 1)]), Err(Cycle), "1 → 2 → 1");
        assert!(order(&b, [(0, 2), (5, 1)]).is_ok(), "row 5 is no bus's return");
    }

    #[test]
    fn plans_are_delivered_and_returned_for_freeing() {
        let (mut ctl, mut run) = plan(8);
        assert_eq!(run.steps(), &[Mix(0..8)]);
        ctl.set(compile(&[bus(1, 0..2, 0..2)], &[1], 8));
        assert!(!ctl.is_synced());
        ctl.tick();
        assert!(ctl.is_synced());
        run.begin_block();
        assert_eq!(run.steps()[1], Bus(1));
        for _ in 0..3 * PLAN_QUEUE {
            ctl.set(ExecutionPlan::mix_all(8));
            ctl.tick();
            run.begin_block(); // never overflows: old plans go back
        }
        assert!(ctl.is_synced());
    }
}
