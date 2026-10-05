//! The pure diff between two published states and its inverse, shared by the
//! engine (which publishes diffs) and every client (which applies them).

use std::collections::BTreeMap;

use crate::{Change, LoadedPlugin, PointState, SlotState, State};

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
    if old.scenes != new.scenes || old.current_scene != new.current_scene || old.morphing != new.morphing {
        out.push(Change::ScenesChanged(new.scenes.clone(), new.current_scene.clone(), new.morphing));
    }
    if old.midi_inputs != new.midi_inputs
        || old.midi_bindings != new.midi_bindings
        || old.midi_learning != new.midi_learning
    {
        out.push(Change::MidiChanged(new.midi_inputs.clone(), new.midi_bindings.clone(), new.midi_learning));
    }
    if old.scripts != new.scripts {
        out.push(Change::ScriptsChanged(new.scripts.clone()));
    }
    if old.plugins != new.plugins || old.bad_plugins != new.bad_plugins {
        out.push(Change::PluginsChanged(new.plugins.clone(), new.bad_plugins.clone()));
    }
    let old_bp: BTreeMap<u32, &LoadedPlugin> = old.bus_plugins.iter().map(|p| (p.bus, p)).collect();
    for p in &new.bus_plugins {
        match old_bp.get(&p.bus) {
            None => out.push(Change::BusPluginSet(p.clone())),
            Some(o) if *o == p => {}
            Some(o) if only_values_differ(o, p) => {
                for (a, b) in o.params.iter().zip(&p.params) {
                    if a != b {
                        out.push(Change::ParamChanged { bus: p.bus, id: b.id, value: b.value, text: b.text.clone() });
                    }
                }
            }
            Some(_) => out.push(Change::BusPluginSet(p.clone())),
        }
    }
    for bus in old_bp.keys().filter(|b| !new.bus_plugins.iter().any(|p| p.bus == **b)) {
        out.push(Change::BusPluginRemoved { bus: *bus });
    }
    out
}

/// Same plugin, status and parameter list; only values and their texts differ.
fn only_values_differ(a: &LoadedPlugin, b: &LoadedPlugin) -> bool {
    a.info == b.info
        && a.status == b.status
        && a.latency == b.latency
        && a.has_editor == b.has_editor
        && a.editor_open == b.editor_open
        && a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(x, y)| {
            let (mut x, mut y) = (x.clone(), y.clone());
            (x.value, x.text, y.value, y.text) = (0.0, String::new(), 0.0, String::new());
            x == y
        })
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
                Change::ScenesChanged(scenes, current, morphing) => {
                    self.scenes = scenes.clone();
                    self.current_scene = current.clone();
                    self.morphing = *morphing;
                }
                Change::MidiChanged(inputs, bindings, learning) => {
                    self.midi_inputs = inputs.clone();
                    self.midi_bindings = bindings.clone();
                    self.midi_learning = *learning;
                }
                Change::ScriptsChanged(scripts) => self.scripts = scripts.clone(),
                Change::PluginsChanged(found, bad) => {
                    self.plugins = found.clone();
                    self.bad_plugins = bad.clone();
                }
                Change::BusPluginSet(p) => match self.bus_plugins.binary_search_by_key(&p.bus, |x| x.bus) {
                    Ok(i) => self.bus_plugins[i] = p.clone(),
                    Err(i) => self.bus_plugins.insert(i, p.clone()),
                },
                Change::BusPluginRemoved { bus } => self.bus_plugins.retain(|p| p.bus != *bus),
                Change::ParamChanged { bus, id, value, text } => {
                    if let Some(p) = self.bus_plugins.iter_mut().find(|p| p.bus == *bus) {
                        if let Some(q) = p.params.iter_mut().find(|q| q.id == *id) {
                            q.value = *value;
                            q.text = text.clone();
                        }
                    }
                }
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
        State {
            version: 0,
            status: status(),
            slots,
            points,
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
            scenes: Vec::new(),
            current_scene: None,
            morphing: false,
            midi_inputs: Vec::new(),
            midi_bindings: Vec::new(),
            midi_learning: None,
            scripts: Vec::new(),
        }
    }

    fn plugin(bus: u32, gain: f64) -> crate::LoadedPlugin {
        crate::LoadedPlugin {
            bus,
            info: crate::PluginInfo {
                path: "t.clap".into(),
                id: "t".into(),
                name: "T".into(),
                vendor: "V".into(),
                version: "1".into(),
            },
            status: crate::PluginStatus::Running,
            latency: 0,
            has_editor: true,
            editor_open: false,
            params: vec![crate::ParamState {
                id: 1,
                name: "Gain".into(),
                module: String::new(),
                min: -60.0,
                max: 12.0,
                default: 0.0,
                value: gain,
                text: format!("{gain:.1} dB"),
                stepped: false,
                read_only: false,
            }],
        }
    }

    #[test]
    fn script_changes_are_found_and_applied() {
        let a = state(vec![slot(1, true)], vec![]);
        let mut b = a.clone();
        b.scripts = vec![crate::ScriptInfo {
            name: "mute".into(),
            source: "function on_midi(m) end".into(),
            enabled: true,
            status: crate::ScriptStatus::Stopped("boom".into()),
            log: vec!["hello".into()],
        }];
        let changes = diff(&a, &b);
        assert_eq!(changes, vec![Change::ScriptsChanged(b.scripts.clone())]);
        let mut applied = a.clone();
        applied.apply(&changes);
        assert_eq!(applied, b);
    }

    #[test]
    fn midi_changes_are_found_and_applied() {
        let a = state(vec![slot(1, true)], vec![]);
        let mut b = a.clone();
        b.midi_inputs = vec!["nanoKONTROL2".into()];
        b.midi_bindings =
            vec![crate::MidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7, input: 0, output: 0 }];
        b.midi_learning = Some((0, 1));
        let changes = diff(&a, &b);
        assert_eq!(changes, vec![Change::MidiChanged(b.midi_inputs.clone(), b.midi_bindings.clone(), Some((0, 1)))]);
        let mut applied = a.clone();
        applied.apply(&changes);
        assert_eq!(applied, b);
    }

    #[test]
    fn scene_changes_are_found_and_applied() {
        let a = state(vec![slot(1, true)], vec![]);
        let mut b = a.clone();
        b.scenes = vec![crate::SceneInfo { name: "Verse".into(), morph_ms: 500, routes: 3, params: 1 }];
        b.current_scene = Some("Verse".into());
        b.morphing = true;
        let changes = diff(&a, &b);
        assert_eq!(changes, vec![Change::ScenesChanged(b.scenes.clone(), Some("Verse".into()), true)]);
        let mut applied = a.clone();
        applied.apply(&changes);
        assert_eq!(applied, b);
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn a_value_change_is_sent_alone_and_other_changes_resend_the_plugin() {
        let mut a = state(vec![slot(1, true)], vec![]);
        a.bus_plugins = vec![plugin(1, 0.0)];
        let mut b = a.clone();
        b.bus_plugins = vec![plugin(1, -6.0)];
        let changes = diff(&a, &b);
        assert_eq!(changes, vec![Change::ParamChanged { bus: 1, id: 1, value: -6.0, text: "-6.0 dB".into() }]);
        let mut applied = a.clone();
        applied.apply(&changes);
        assert_eq!(applied.bus_plugins, b.bus_plugins);

        let mut opened = b.clone();
        opened.bus_plugins[0].editor_open = true;
        assert_eq!(diff(&b, &opened), vec![Change::BusPluginSet(opened.bus_plugins[0].clone())], "editor opened");
        let mut c = b.clone();
        c.bus_plugins[0].status = crate::PluginStatus::Faulted;
        assert_eq!(diff(&b, &c), vec![Change::BusPluginSet(c.bus_plugins[0].clone())]);
        let mut d = c.clone();
        d.bus_plugins.clear();
        d.plugins = vec![plugin(1, 0.0).info];
        let changes = diff(&c, &d);
        assert!(changes.contains(&Change::BusPluginRemoved { bus: 1 }));
        assert!(changes.contains(&Change::PluginsChanged(d.plugins.clone(), Vec::new())));
        let mut applied = c.clone();
        applied.apply(&changes);
        assert_eq!(applied, d);
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
