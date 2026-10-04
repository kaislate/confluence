//! The pure diff between two published states and its inverse, shared by the
//! engine (which publishes diffs) and every client (which applies them).

use std::collections::BTreeMap;

use crate::{Change, PointState, SlotState, State};

/// The changes that turn `old` into `new`. Version and status are not compared.
pub fn diff(old: &State, new: &State) -> Vec<Change> {
    let mut out = Vec::new();
    let old_slots: BTreeMap<u32, &SlotState> = old.slots.iter().map(|s| (s.id, s)).collect();
    let new_slots: BTreeMap<u32, &SlotState> = new.slots.iter().map(|s| (s.id, s)).collect();
    for (id, s) in &new_slots {
        match old_slots.get(id) {
            None => out.push(Change::SlotAdded((*s).clone())),
            Some(o) if o != s => out.push(Change::SlotChanged((*s).clone())),
            Some(_) => {}
        }
    }
    for id in old_slots.keys().filter(|id| !new_slots.contains_key(id)) {
        out.push(Change::SlotRemoved { id: *id });
    }
    let key = |p: &PointState| (p.input, p.output);
    let old_points: BTreeMap<(u32, u32), &PointState> = old.points.iter().map(|p| (key(p), p)).collect();
    let new_points: BTreeMap<(u32, u32), &PointState> = new.points.iter().map(|p| (key(p), p)).collect();
    for (k, p) in &new_points {
        if old_points.get(k) != Some(p) {
            out.push(Change::PointSet((*p).clone()));
        }
    }
    for (input, output) in old_points.keys().filter(|k| !new_points.contains_key(k)) {
        out.push(Change::PointRemoved { input: *input, output: *output });
    }
    if old.devices != new.devices {
        out.push(Change::DevicesChanged(new.devices.clone()));
    }
    if old.notices != new.notices {
        out.push(Change::NoticesChanged(new.notices.clone()));
    }
    out
}

impl State {
    /// Applies `changes` (from [`diff`]), keeping slots sorted by id and
    /// points by (input, output). Does not touch `version` or `status`.
    pub fn apply(&mut self, changes: &[Change]) {
        for c in changes {
            match c {
                Change::SlotAdded(s) | Change::SlotChanged(s) => match self.slots.binary_search_by_key(&s.id, |x| x.id)
                {
                    Ok(i) => self.slots[i] = s.clone(),
                    Err(i) => self.slots.insert(i, s.clone()),
                },
                Change::SlotRemoved { id } => self.slots.retain(|s| s.id != *id),
                Change::PointSet(p) => {
                    match self.points.binary_search_by_key(&(p.input, p.output), |x| (x.input, x.output)) {
                        Ok(i) => self.points[i] = p.clone(),
                        Err(i) => self.points.insert(i, p.clone()),
                    }
                }
                Change::PointRemoved { input, output } => {
                    self.points.retain(|p| (p.input, p.output) != (*input, *output));
                }
                Change::DevicesChanged(d) => self.devices = d.clone(),
                Change::NoticesChanged(n) => self.notices = n.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClockRole, DeviceInfo, DeviceKind, EngineStatus, PointState, SlotState, State};
    use proptest::prelude::*;

    fn status() -> EngineStatus {
        EngineStatus {
            master: "internal".into(),
            sample_rate: 48_000.0,
            block: 256,
            blocks: 0,
            dsp_load: 0.0,
            xruns: 0,
        }
    }

    fn slot(id: u32, online: bool) -> SlotState {
        SlotState {
            id,
            name: format!("slot {id}"),
            device: String::new(),
            role: ClockRole::Soft,
            online,
            first_input: id * 2,
            inputs: 2,
            first_output: id * 2,
            outputs: 2,
        }
    }

    fn point(input: u32, output: u32, gain_db: f32) -> PointState {
        PointState { input, output, gain_db, mute: false, invert: false }
    }

    fn state(slots: Vec<SlotState>, points: Vec<PointState>) -> State {
        State { version: 0, status: status(), slots, points, devices: Vec::new(), notices: Vec::new() }
    }

    #[test]
    fn each_kind_of_change_is_found_and_applied() {
        let a = state(vec![slot(1, true), slot(2, true)], vec![point(0, 0, 0.0), point(1, 1, -6.0)]);
        let mut b = state(vec![slot(1, false), slot(3, true)], vec![point(0, 0, -3.0), point(2, 2, 0.0)]);
        b.devices = vec![DeviceInfo { kind: DeviceKind::Vaio, name: "1".into(), inputs: 2, outputs: 0 }];
        b.notices = vec!["n".into()];
        let changes = diff(&a, &b);
        assert!(changes.contains(&Change::SlotChanged(slot(1, false))));
        assert!(changes.contains(&Change::SlotRemoved { id: 2 }));
        assert!(changes.contains(&Change::SlotAdded(slot(3, true))));
        assert!(changes.contains(&Change::PointSet(point(0, 0, -3.0))));
        assert!(changes.contains(&Change::PointRemoved { input: 1, output: 1 }));
        assert!(changes.contains(&Change::PointSet(point(2, 2, 0.0))));
        assert!(changes.iter().any(|c| matches!(c, Change::DevicesChanged(d) if d.len() == 1)));
        assert!(changes.contains(&Change::NoticesChanged(vec!["n".into()])));
        let mut c = a.clone();
        c.apply(&changes);
        assert_eq!((c.slots, c.points, c.devices, c.notices), (b.slots, b.points, b.devices, b.notices));
    }

    #[test]
    fn identical_states_have_no_diff() {
        let a = state(vec![slot(1, true)], vec![point(0, 0, 0.0)]);
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn version_and_status_are_not_part_of_the_diff() {
        let a = state(vec![], vec![]);
        let mut b = a.clone();
        b.version = 9;
        b.status.blocks = 1000;
        assert!(diff(&a, &b).is_empty());
    }

    fn arb_state() -> impl Strategy<Value = State> {
        let slots = prop::collection::btree_map(0u32..8, any::<bool>(), 0..6)
            .prop_map(|m| m.into_iter().map(|(id, on)| slot(id, on)).collect::<Vec<_>>());
        let points = prop::collection::btree_map((0u32..6, 0u32..6), -60i32..12, 0..12)
            .prop_map(|m| m.into_iter().map(|((i, o), g)| point(i, o, g as f32)).collect::<Vec<_>>());
        let notices = prop::collection::vec("[a-z]{1,4}", 0..3);
        (slots, points, notices).prop_map(|(s, p, n)| {
            let mut st = state(s, p);
            st.notices = n;
            st
        })
    }

    proptest! {
        #[test]
        fn applying_the_diff_reproduces_the_new_state(a in arb_state(), b in arb_state()) {
            let mut c = a.clone();
            c.apply(&diff(&a, &b));
            prop_assert_eq!(c.slots, b.slots);
            prop_assert_eq!(c.points, b.points);
            prop_assert_eq!(c.notices, b.notices);
        }
    }
}
