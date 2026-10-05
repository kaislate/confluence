//! Edits sent but not yet visible in the engine's state. The grid shows their
//! value meanwhile, and drops it once the engine's state has it (or is past
//! the edit's version), or when the edit fails.

use std::collections::HashMap;

use confluence_api::{PointState, State};

use crate::commands::Edit;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// `None`: the route is being removed.
    value: Option<PointState>,
    /// The state version that includes this value, once the engine replied.
    version: Option<u64>,
}

#[derive(Default)]
pub struct Pending {
    map: HashMap<(u32, u32), Entry>,
}

/// The point an edit changes and the value it gives it.
fn edit_value(edit: &Edit) -> Option<((u32, u32), Option<PointState>)> {
    match *edit {
        Edit::SetPoint { input, output, gain_db, mute, invert } => {
            Some(((input, output), Some(PointState { input, output, gain_db, mute, invert })))
        }
        Edit::RemovePoint { input, output } => Some(((input, output), None)),
        _ => None,
    }
}

impl Pending {
    pub fn sent(&mut self, edit: &Edit) {
        if let Some((p, value)) = edit_value(edit) {
            self.map.insert(p, Entry { value, version: None });
        }
    }

    /// The engine applied `edit`. It confirms the pending value only if that
    /// value is still this edit's (a newer one may be pending meanwhile).
    pub fn done(&mut self, edit: &Edit, version: Option<u64>) {
        let Some((p, value)) = edit_value(edit) else { return };
        if let Some(e) = self.map.get_mut(&p) {
            if e.value == value {
                match version {
                    Some(v) => e.version = Some(v),
                    None => {
                        self.map.remove(&p);
                    }
                }
            }
        }
    }

    pub fn failed(&mut self, edit: &Edit) {
        let Some((p, value)) = edit_value(edit) else { return };
        if self.map.get(&p).is_some_and(|e| e.value == value) {
            self.map.remove(&p);
        }
    }

    /// Drops values the state now shows, or whose version it has reached.
    pub fn reconcile(&mut self, state: &State) {
        self.map.retain(|p, e| {
            let actual = state.points.iter().find(|x| (x.input, x.output) == *p);
            let shown = actual == e.value.as_ref();
            let reached = e.version.is_some_and(|v| state.version >= v);
            !(shown || reached)
        });
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn is_pending(&self, p: (u32, u32)) -> bool {
        self.map.contains_key(&p)
    }

    /// What to show for a point: its pending value, or else the engine's.
    pub fn effective(&self, p: (u32, u32), actual: Option<&PointState>) -> Option<PointState> {
        match self.map.get(&p) {
            Some(e) => e.value.clone(),
            None => actual.cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::EngineStatus;

    fn state(version: u64, points: Vec<PointState>) -> State {
        State {
            version,
            status: EngineStatus {
                master: "internal".into(),
                sample_rate: 48_000.0,
                block: 256,
                blocks: 0,
                dsp_load: 0.0,
                xruns: 0,
            },
            slots: Vec::new(),
            points,
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
            scenes: Vec::new(),
            current_scene: None,
            morphing: false,
        }
    }

    fn pt(gain_db: f32) -> PointState {
        PointState { input: 1, output: 2, gain_db, mute: false, invert: false }
    }

    fn set(gain_db: f32) -> Edit {
        Edit::SetPoint { input: 1, output: 2, gain_db, mute: false, invert: false }
    }

    #[test]
    fn a_sent_value_shows_until_the_state_has_it() {
        let mut p = Pending::default();
        p.sent(&set(-6.0));
        assert!(p.is_pending((1, 2)));
        assert_eq!(p.effective((1, 2), None), Some(pt(-6.0)));
        p.reconcile(&state(0, vec![]));
        assert!(p.is_pending((1, 2)), "not there yet");
        p.reconcile(&state(1, vec![pt(-6.0)]));
        assert!(!p.is_pending((1, 2)));
        assert_eq!(p.effective((1, 2), Some(&pt(-6.0))), Some(pt(-6.0)));
    }

    #[test]
    fn a_removal_shows_as_no_route() {
        let mut p = Pending::default();
        p.sent(&Edit::RemovePoint { input: 1, output: 2 });
        assert_eq!(p.effective((1, 2), Some(&pt(0.0))), None);
        p.reconcile(&state(1, vec![]));
        assert!(!p.is_pending((1, 2)));
    }

    #[test]
    fn the_reply_version_confirms_even_if_the_values_differ() {
        let mut p = Pending::default();
        p.sent(&set(-6.0));
        p.done(&set(-6.0), Some(5));
        p.reconcile(&state(4, vec![]));
        assert!(p.is_pending((1, 2)));
        p.reconcile(&state(5, vec![pt(-6.000_01)]));
        assert!(!p.is_pending((1, 2)));
    }

    #[test]
    fn a_failure_drops_the_value() {
        let mut p = Pending::default();
        p.sent(&set(-6.0));
        p.failed(&set(-6.0));
        assert_eq!(p.effective((1, 2), Some(&pt(0.0))), Some(pt(0.0)));
    }

    #[test]
    fn an_older_reply_does_not_confirm_a_newer_value() {
        let mut p = Pending::default();
        p.sent(&set(-6.0));
        p.sent(&set(-3.0)); // still dragging
        p.done(&set(-6.0), Some(5));
        p.failed(&set(-6.0));
        p.reconcile(&state(5, vec![pt(-6.0)]));
        assert_eq!(p.effective((1, 2), Some(&pt(-6.0))), Some(pt(-3.0)), "the newer value stays");
    }

    #[test]
    fn another_clients_later_change_wins() {
        let mut p = Pending::default();
        p.sent(&set(-6.0));
        p.done(&set(-6.0), Some(5));
        // Another GUI changed the point again before this one saw version 5.
        p.reconcile(&state(6, vec![pt(-20.0)]));
        assert_eq!(p.effective((1, 2), Some(&pt(-20.0))), Some(pt(-20.0)));
    }

    #[test]
    fn device_edits_are_not_tracked() {
        let mut p = Pending::default();
        p.sent(&Edit::RemoveSlot { id: 1 });
        assert!(!p.is_pending((1, 2)));
    }
}
