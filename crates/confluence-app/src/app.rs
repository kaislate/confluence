//! The window: one `StoreView` per frame drives the top bar, the banner, the
//! matrix, the inspector and the devices panel; edits go to the worker.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_api::PointState;
use confluence_client::{ConnState, StateStore, StoreView, Update};
use eframe::egui::{self, Align, Align2, Button, Id, Key, Layout, RichText};

use crate::commands::{Edit, Outcome, Worker};
use crate::engine_launch::Launcher;
use crate::grid_view;
use crate::matrix::{key_edit, move_selection, selection_valid, CellKey, DeferredUnroute, GridLayout, Selection};
use crate::notify::Notes;
use crate::pending::Pending;
use crate::skin::Look;

/// eframe storage key for the inspector's open state.
pub const INSPECTOR_KEY: &str = "inspector_open";
/// Live but silent for this long: "Not responding".
const QUIET: Duration = Duration::from_secs(2);
/// Reconnecting for this long: offer Start engine too.
const OFFER_START_AFTER: Duration = Duration::from_secs(3);
/// A press that moves further than this before release is a drag, not a
/// click (spec §4.1; egui's default is 6 px).
const CLICK_DIST: f32 = 3.0;
/// The xrun count flashes for this long after it rises.
const XRUN_FLASH: Duration = Duration::from_secs(1);

pub struct AppConfig {
    pub pipe: String,
    pub engine_exe: PathBuf,
    pub engine_args: Vec<String>,
    /// A skin folder (`--skin`); `None` for the built-in skin.
    pub skin: Option<PathBuf>,
}

/// The connection badge's text.
pub fn badge(conn: &ConnState, last_event: Option<Instant>, now: Instant) -> String {
    match conn {
        ConnState::Connecting => "Connecting…".into(),
        ConnState::Reconnecting { since } => {
            format!("Reconnecting ({} s)", now.saturating_duration_since(*since).as_secs())
        }
        ConnState::Live if last_event.is_some_and(|t| now.saturating_duration_since(t) >= QUIET) => {
            "Not responding".into()
        }
        ConnState::Live => "Live".into(),
    }
}

/// What the window says when it cannot open at all (a release build has no
/// console, so this is shown in a message box).
pub fn startup_error_text(e: &dyn std::fmt::Display) -> String {
    format!(
        "Confluence could not open its window: {e}

The audio engine runs on its own; audio is not affected."
    )
}

/// How long telemetry may wait to be drawn: the top bar's figures are fine a
/// few times a second; a slot's live graphs get every sample.
pub const TELEMETRY_REPAINT: Duration = Duration::from_millis(250);

/// `None`: repaint now; else repaint within this long.
pub fn telemetry_repaint(graphs_shown: bool) -> Option<Duration> {
    if graphs_shown {
        None
    } else {
        Some(TELEMETRY_REPAINT)
    }
}

/// True once per new snapshot: `seen` is the snapshot count last handled.
pub fn fresh_snapshot(seen: &mut u64, snapshots: u64) -> bool {
    let fresh = snapshots != *seen;
    *seen = snapshots;
    fresh
}

/// The xrun count last seen and when it last rose (the top bar flashes then).
/// A lower count (a restarted engine) becomes the new baseline, so new xruns
/// flash again rather than only once they pass the old total.
pub fn track_xruns(seen: (u64, Option<Instant>), count: u64, now: Instant) -> (u64, Option<Instant>) {
    if count > seen.0 {
        (count, Some(now))
    } else {
        (count, seen.1)
    }
}

/// Keys that act on the selected cell.
const CELL_KEYS: [(Key, CellKey); 7] = [
    (Key::Space, CellKey::Toggle),
    (Key::Plus, CellKey::Up),
    (Key::Equals, CellKey::Up),
    (Key::Minus, CellKey::Down),
    (Key::M, CellKey::Mute),
    (Key::I, CellKey::Invert),
    (Key::Delete, CellKey::Remove),
];

/// The cell actions for the keys pressed this frame, and whether they step
/// finely. Fine is Alt, not Shift: typing `+` already needs Shift on many
/// layouts. Ctrl/Cmd combinations are left to other shortcuts (e.g. zoom).
pub fn cell_keys(pressed: &[Key], mods: egui::Modifiers) -> (Vec<CellKey>, bool) {
    if mods.ctrl || mods.command {
        return (Vec::new(), false);
    }
    let mut keys: Vec<CellKey> = Vec::new();
    for (k, action) in CELL_KEYS {
        if pressed.contains(&k) && !keys.contains(&action) {
            keys.push(action);
        }
    }
    (keys, mods.alt)
}

/// When the window must look again even if nothing arrives: while not
/// connected (timers in the banner), or when a live engine would cross the
/// "Not responding" threshold.
pub fn next_check(conn: &ConnState, last_event: Option<Instant>, now: Instant) -> Option<Duration> {
    const TICK: Duration = Duration::from_millis(500);
    match (conn, last_event) {
        (ConnState::Live, Some(t)) => {
            let silent = now.saturating_duration_since(t);
            Some(if silent >= QUIET { TICK } else { QUIET - silent + Duration::from_millis(100) })
        }
        _ => Some(TICK),
    }
}

/// The value of `name` in `args`: `--name VALUE` or `--name=VALUE`.
pub fn flag(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}

pub struct ConfluenceApp {
    store: StateStore,
    worker: Worker,
    launcher: Launcher,
    /// The context the store and worker wake, set on the first frame.
    repaint: Arc<Mutex<Option<egui::Context>>>,
    view: Arc<StoreView>,
    pending: Pending,
    notes: Notes,
    selection: Selection,
    cell: f32,
    pub inspector_open: bool,
    xruns: (u64, Option<Instant>),
    /// The store's snapshot count last handled (see `fresh_snapshot`).
    snapshots_seen: u64,
    /// Whether live health graphs are on screen (telemetry repaints at full rate).
    graphs_live: Arc<AtomicBool>,
    look: Look,
    skin_dir: Option<PathBuf>,
    /// A slot waiting for "Remove ‹name›?" to be confirmed.
    confirm_remove: Option<u32>,
    /// A clicked route waiting out the double-click window before it is removed.
    unroute: DeferredUnroute,
    devices_open: bool,
    devices: crate::devices::DevicesState,
    /// The plugin picker, while open.
    picker: Option<crate::plugins::Picker>,
    /// Parameter values sent and not yet confirmed: (bus, param) → value.
    param_pending: HashMap<(u32, u32), f64>,
    /// A bus whose plugin is being loaded.
    plugin_loading: Option<u32>,
}

/// The engine owns plugin editor windows; Windows lets a background process
/// bring a window to the front only with the foreground process's leave.
fn allow_engine_to_the_front() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
        // SAFETY: no pointers; failure only means the editor may open behind.
        let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }
}

impl ConfluenceApp {
    pub fn new(config: AppConfig) -> Self {
        let repaint: Arc<Mutex<Option<egui::Context>>> = Arc::default();
        let wake: Arc<dyn Fn() + Send + Sync> = {
            let repaint = repaint.clone();
            Arc::new(move || {
                if let Ok(ctx) = repaint.lock() {
                    if let Some(ctx) = ctx.as_ref() {
                        ctx.request_repaint();
                    }
                }
            })
        };
        let graphs_live = Arc::new(AtomicBool::new(false));
        let store = StateStore::spawn(config.pipe.clone(), {
            let (repaint, graphs_live) = (repaint.clone(), graphs_live.clone());
            Box::new(move |what| {
                if let Ok(ctx) = repaint.lock() {
                    if let Some(ctx) = ctx.as_ref() {
                        match (what, telemetry_repaint(graphs_live.load(Ordering::Relaxed))) {
                            (Update::Telemetry, Some(after)) => ctx.request_repaint_after(after),
                            _ => ctx.request_repaint(),
                        }
                    }
                }
            })
        });
        let worker = Worker::spawn(config.pipe.clone(), wake);
        let view = store.view();
        ConfluenceApp {
            store,
            worker,
            launcher: Launcher::new(config.engine_exe, config.engine_args),
            repaint,
            view,
            pending: Pending::default(),
            notes: Notes::default(),
            selection: Selection::None,
            cell: Look::builtin().skin.cell,
            inspector_open: true,
            xruns: (0, None),
            snapshots_seen: 0,
            graphs_live,
            look: Look::builtin(),
            skin_dir: config.skin,
            confirm_remove: None,
            unroute: DeferredUnroute::default(),
            devices_open: false,
            devices: crate::devices::DevicesState::default(),
            picker: None,
            param_pending: HashMap::new(),
            plugin_loading: None,
        }
    }

    pub fn look(&self) -> &Look {
        &self.look
    }

    /// What the grid shows for a point (the pending value, else the engine's).
    pub fn point(&self, input: u32, output: u32) -> Option<PointState> {
        let actual = self.view.state.as_ref()?.points.iter().find(|p| (p.input, p.output) == (input, output));
        self.pending.effective((input, output), actual)
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    fn send(&mut self, edit: Edit) {
        match edit {
            Edit::SetParam { bus, param, value } => {
                self.param_pending.insert((bus, param), value);
            }
            Edit::LoadPlugin { bus, .. } => self.plugin_loading = Some(bus),
            Edit::ShowEditor { .. } => allow_engine_to_the_front(),
            _ => {}
        }
        self.pending.sent(&edit);
        self.worker.send(edit);
    }

    fn on_outcome(&mut self, outcome: Outcome, now: Instant) {
        match outcome {
            Outcome::Done { edit, version, ids } => {
                self.pending.done(&edit, version);
                self.on_done(&edit, &ids, now);
            }
            Outcome::Failed { edit, reason } => {
                self.pending.failed(&edit);
                self.on_failed(&edit, now);
                self.notes.error(reason, now);
            }
        }
    }

    /// A parameter's value is shown from the engine again once its edit is
    /// answered (unless a newer one is already on its way).
    fn param_answered(&mut self, edit: &Edit) {
        if let Edit::SetParam { bus, param, value } = edit {
            if self.param_pending.get(&(*bus, *param)) == Some(value) {
                self.param_pending.remove(&(*bus, *param));
            }
        }
        if let Edit::LoadPlugin { bus, .. } = edit {
            if self.plugin_loading == Some(*bus) {
                self.plugin_loading = None;
            }
        }
    }

    fn on_done(&mut self, edit: &Edit, ids: &[u32], now: Instant) {
        self.param_answered(edit);
        if let Edit::AddBus { name, .. } = edit {
            self.devices.adding_bus = false;
            self.devices.bus_name.clear();
            self.notes.info(format!("Added insert bus {name}"), now);
            if let Some(id) = ids.first() {
                self.selection = Selection::Slot(*id);
            }
        }
        if let Edit::AddDevice { kind, name } = edit {
            let base = crate::devices::base_name(*kind, name);
            self.devices.adding.remove(&(*kind, base.clone()));
            self.notes.info(format!("Added {} {}", crate::devices::kind_title(*kind), base), now);
            if let Some(id) = ids.first() {
                self.selection = Selection::Slot(*id);
            }
        }
    }

    fn on_failed(&mut self, edit: &Edit, _now: Instant) {
        self.param_answered(edit);
        if let Edit::AddBus { .. } = edit {
            self.devices.adding_bus = false;
        }
        if let Edit::AddDevice { kind, name } = edit {
            self.devices.adding.remove(&(*kind, crate::devices::base_name(*kind, name)));
        }
    }

    fn live(&self) -> bool {
        matches!(self.view.conn, ConnState::Live)
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let ctx = ui.ctx().clone();
        if let Ok(mut r) = self.repaint.lock() {
            if r.is_none() {
                // First frame: load the skin (textures need the context) and style egui.
                if let Some(dir) = self.skin_dir.clone() {
                    let (look, warnings) = Look::load(&ctx, &dir);
                    self.cell = look.skin.cell;
                    self.look = look;
                    for w in warnings {
                        self.notes.error(w, now);
                    }
                }
                self.look.apply(&ctx);
                ctx.options_mut(|o| o.input_options.max_click_dist = CLICK_DIST);
                *r = Some(ctx.clone());
            }
        }
        self.view = self.store.view();
        for o in self.worker.outcomes() {
            self.on_outcome(o, now);
        }
        if let Some(msg) = self.launcher.poll(now) {
            self.notes.error(msg, now);
        }
        let view = self.view.clone();
        if fresh_snapshot(&mut self.snapshots_seen, view.snapshots) {
            // The snapshot is the truth now; edits pending against an older
            // connection got their outcome there or never will.
            self.pending.clear();
        }
        if let Some(state) = &view.state {
            self.pending.reconcile(state);
            if !selection_valid(&self.selection, &state.slots) {
                self.selection = Selection::None;
            }
        }
        self.notes.prune(now);
        let t = ctx.input(|i| i.time);
        if let Some((input, output)) = self.unroute.due(t) {
            self.send(Edit::RemovePoint { input, output });
        }
        if let Some(wait) = self.unroute.waiting(t) {
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }

        let graphs = self.inspector_open && matches!(self.selection, Selection::Slot(_));
        self.graphs_live.store(graphs, Ordering::Relaxed);
        self.top_bar(ui, &view, now);
        self.side_panels(ui, &view, now);
        self.matrix(ui, &view);
        self.keyboard(ui, &view);
        self.notifications(&ctx, &view);
        self.dialogs(&ctx, &view);

        if let Some(wait) = next_check(&view.conn, view.last_event, now) {
            ctx.request_repaint_after(wait);
        }
        if self.notes.has_info() || self.xruns.1.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, view: &StoreView, now: Instant) {
        egui::Panel::top("top-bar").show(ui, |ui| {
            let (warn, error) = (self.look.skin.colors.warn, self.look.skin.colors.error);
            self.look.paint_surface(ui.painter(), ui.max_rect(), "top_bar", self.look.skin.colors.panel);
            ui.horizontal(|ui| {
                let text = badge(&view.conn, view.last_event, now);
                let colour = match text.as_str() {
                    "Live" => None,
                    "Not responding" => Some(error),
                    _ => Some(warn),
                };
                let badge = RichText::new(&text).strong();
                ui.label(match colour {
                    Some(c) => badge.color(c),
                    None => badge,
                });
                if let Some(s) = &view.status {
                    ui.separator();
                    ui.label(format!("Master {}", s.master));
                    ui.label(format!("{:.0} Hz", s.sample_rate));
                    ui.label(format!("Block {}", s.block));
                    let dsp = RichText::new(format!("DSP {:.0}%", s.dsp_load * 100.0));
                    ui.label(match self.look.dsp_color(s.dsp_load) {
                        Some(c) => dsp.color(c),
                        None => dsp,
                    });
                    self.xruns = track_xruns(self.xruns, s.xruns, now);
                    let flashing = self.xruns.1.is_some_and(|t| now.saturating_duration_since(t) < XRUN_FLASH);
                    if !flashing {
                        self.xruns.1 = None;
                    }
                    let xr = RichText::new(format!("Xruns {}", s.xruns));
                    ui.label(if flashing { xr.color(error).strong() } else { xr });
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.top_bar_buttons(ui);
                });
            });
            self.banner(ui, view, now);
        });
    }

    fn banner(&mut self, ui: &mut egui::Ui, view: &StoreView, now: Instant) {
        let (warn, error) = (self.look.skin.colors.warn, self.look.skin.colors.error);
        match view.conn {
            ConnState::Connecting => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Engine not running").color(warn));
                    self.start_button(ui, now);
                });
            }
            ConnState::Reconnecting { since } => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Reconnecting…").color(warn));
                    if now.saturating_duration_since(since) >= OFFER_START_AFTER {
                        self.start_button(ui, now);
                    }
                });
            }
            ConnState::Live => {
                if view.last_event.is_some_and(|t| now.saturating_duration_since(t) >= QUIET) {
                    ui.label(RichText::new("Engine not responding").color(error));
                }
            }
        }
    }

    fn start_button(&mut self, ui: &mut egui::Ui, now: Instant) {
        if ui.add_enabled(self.launcher.ready(now), Button::new("Start engine")).clicked() {
            if let Err(e) = self.launcher.start(now) {
                self.notes.error(e, now);
            }
        }
    }

    fn top_bar_buttons(&mut self, ui: &mut egui::Ui) {
        ui.toggle_value(&mut self.inspector_open, "Inspector");
        ui.toggle_value(&mut self.devices_open, "Devices…");
    }

    fn side_panels(&mut self, ui: &mut egui::Ui, view: &StoreView, _now: Instant) {
        if self.devices_open {
            let editable = self.live();
            let (list, slots) = match &view.state {
                Some(s) => (s.devices.clone(), s.slots.clone()),
                None => (Vec::new(), Vec::new()),
            };
            let devices = &mut self.devices;
            let look = &self.look;
            let edits = egui::Panel::left("devices")
                .resizable(true)
                .show(ui, |ui| {
                    look.paint_surface(ui.painter(), ui.max_rect(), "panel", look.skin.colors.panel);
                    crate::devices::show(ui, &list, &slots, devices, editable)
                })
                .inner;
            for e in edits {
                self.send(e);
            }
        }
        if !self.inspector_open {
            return;
        }
        let editable = self.live();
        let point = match self.selection {
            Selection::Cell { input, output } => self.point(input, output),
            _ => None,
        };
        let selection = self.selection;
        // Only the inspected slot's history is copied, once per frame.
        let history = match selection {
            Selection::Slot(id) => self.store.history_of(id),
            _ => None,
        };
        let look = &self.look;
        let actions = egui::Panel::right("inspector")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                look.paint_surface(ui.painter(), ui.max_rect(), "panel", look.skin.colors.panel);
                egui::ScrollArea::vertical()
                    .show(ui, |ui| {
                        let plugin_ui =
                            crate::inspector::PluginUi { pending: &self.param_pending, loading: self.plugin_loading };
                        crate::inspector::show(
                            ui,
                            view,
                            look,
                            &selection,
                            point,
                            history.as_ref(),
                            editable,
                            &plugin_ui,
                        )
                    })
                    .inner
            })
            .inner;
        for a in actions {
            match a {
                crate::inspector::Action::Edit(e) => self.send(e),
                crate::inspector::Action::RemoveSlot(id) => self.confirm_remove = Some(id),
                crate::inspector::Action::PickPlugin(bus) => {
                    self.picker = Some(crate::plugins::Picker { bus, filter: String::new() })
                }
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context, view: &StoreView) {
        if let (Some(picker), Some(state)) = (self.picker.as_mut(), view.state.as_ref()) {
            match crate::plugins::show(ctx, state, picker) {
                Some(crate::plugins::Choice::Load(edit)) => {
                    self.picker = None;
                    self.send(edit);
                }
                Some(crate::plugins::Choice::Close) => self.picker = None,
                None => {}
            }
        }
        let Some(id) = self.confirm_remove else { return };
        let Some(state) = &view.state else { return };
        let Some(slot) = state.slots.iter().find(|s| s.id == id) else {
            self.confirm_remove = None;
            return;
        };
        let routes = crate::inspector::routes_of(state, slot);
        let mut choice = None;
        let modal = egui::Modal::new(Id::new("confirm-remove")).show(ctx, |ui| {
            ui.label(format!("Remove {}? Its {routes} routes are removed too.", slot.name));
            ui.horizontal(|ui| {
                if ui.button("Remove").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => {
                self.confirm_remove = None;
                self.send(Edit::RemoveSlot { id });
            }
            Some(false) => self.confirm_remove = None,
            None => {}
        }
    }

    fn matrix(&mut self, ui: &mut egui::Ui, view: &StoreView) {
        let editable = self.live();
        egui::CentralPanel::default().show(ui, |ui| {
            self.look.paint_surface(ui.painter(), ui.max_rect(), "background", self.look.skin.colors.background);
            let Some(state) = &view.state else {
                ui.label("Waiting for the engine…");
                return;
            };
            if state.slots.is_empty() {
                ui.label("No slots yet: add a device with Devices…");
                return;
            }
            let layout = GridLayout::new(&state.slots, self.cell);
            let by_point: HashMap<(u32, u32), &PointState> =
                state.points.iter().map(|p| ((p.input, p.output), p)).collect();
            let (pending, unroute) = (&self.pending, &mut self.unroute);
            let lookup = |p: (u32, u32)| (pending.effective(p, by_point.get(&p).copied()), pending.is_pending(p));
            let selected = match self.selection {
                Selection::Cell { input, output } => Some((input, output)),
                _ => None,
            };
            let actions = grid_view::show(ui, &layout, &self.look, &lookup, selected, editable, unroute);
            if let Some(z) = actions.zoom {
                self.cell = z.clamp(crate::matrix::CELL_MIN, crate::matrix::CELL_MAX);
            }
            if let Some(s) = actions.select {
                self.selection = s;
            }
            for e in actions.edits {
                self.send(e);
            }
        });
    }

    fn keyboard(&mut self, ui: &mut egui::Ui, view: &StoreView) {
        if ui.ctx().memory(|m| m.focused().is_some()) {
            return; // a text field or slider has the keys
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.selection = Selection::None;
            return;
        }
        let (Selection::Cell { input, output }, Some(state), true) = (self.selection, &view.state, self.live()) else {
            return;
        };
        let layout = GridLayout::new(&state.slots, self.cell);
        let (fine, keys, moves) = ui.input(|i| {
            let pressed: Vec<Key> = CELL_KEYS.iter().map(|(k, _)| *k).filter(|k| i.key_pressed(*k)).collect();
            let (keys, fine) = cell_keys(&pressed, i.modifiers);
            let moves: Vec<(i32, i32)> = [
                (Key::ArrowUp, (-1, 0)),
                (Key::ArrowDown, (1, 0)),
                (Key::ArrowLeft, (0, -1)),
                (Key::ArrowRight, (0, 1)),
            ]
            .into_iter()
            .filter(|(k, _)| i.key_pressed(*k))
            .map(|(_, d)| d)
            .collect();
            (fine, keys, moves)
        });
        for key in keys {
            let cur = self.point(input, output);
            if let Some(e) = key_edit((input, output), cur.as_ref(), key, fine) {
                self.send(e);
            }
        }
        if let (Some(at), Some(&(dr, dc))) = (layout.cell_of(input, output), moves.last()) {
            if let Some((r, c)) = move_selection(&layout, at, dr, dc) {
                if let Some((i, o)) = layout.point(r, c) {
                    self.selection = Selection::Cell { input: i, output: o };
                }
            }
        }
    }

    fn notifications(&mut self, ctx: &egui::Context, view: &StoreView) {
        let (warn, error) = (self.look.skin.colors.warn, self.look.skin.colors.error);
        let mut dismiss = None;
        egui::Area::new(Id::new("notifications")).anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0]).show(ctx, |ui| {
            if let Some(state) = &view.state {
                for n in &state.notices {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new(n).color(warn));
                    });
                }
            }
            for note in self.notes.shown() {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let text = if note.count > 1 {
                            format!("{} (×{})", note.text, note.count)
                        } else {
                            note.text.clone()
                        };
                        ui.label(if note.error { RichText::new(text).color(error) } else { RichText::new(text) });
                        if note.error {
                            let b = ui.small_button("✕");
                            b.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Dismiss"));
                            if b.clicked() {
                                dismiss = Some(note.id);
                            }
                        }
                    });
                });
            }
        });
        if let Some(id) = dismiss {
            self.notes.dismiss(id);
        }
    }
}

impl eframe::App for ConfluenceApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(INSPECTOR_KEY, self.inspector_open.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_badge_follows_the_connection() {
        let now = Instant::now();
        assert_eq!(badge(&ConnState::Connecting, None, now), "Connecting…");
        assert_eq!(badge(&ConnState::Live, Some(now), now), "Live");
        let since = now - Duration::from_secs(12);
        assert_eq!(badge(&ConnState::Reconnecting { since }, Some(since), now), "Reconnecting (12 s)");
    }

    #[test]
    fn the_badge_reports_a_quiet_engine_as_not_responding() {
        let now = Instant::now();
        let quiet = now - Duration::from_millis(2500);
        assert_eq!(badge(&ConnState::Live, Some(quiet), now), "Not responding");
        assert_eq!(badge(&ConnState::Live, Some(now - Duration::from_millis(500)), now), "Live");
    }

    /// Without a repaint nothing would notice an engine going quiet: the window
    /// must look again when "Not responding" could become true.
    #[test]
    fn a_live_engine_is_rechecked_when_it_could_go_quiet() {
        let now = Instant::now();
        let heard = now - Duration::from_millis(500);
        let wait = next_check(&ConnState::Live, Some(heard), now).unwrap();
        assert!(wait >= Duration::from_millis(1500) && wait <= Duration::from_millis(1700), "{wait:?}");
        let quiet = now - Duration::from_secs(5);
        assert!(
            next_check(&ConnState::Live, Some(quiet), now).unwrap() <= Duration::from_millis(500),
            "keeps the badge's timer moving"
        );
        assert_eq!(next_check(&ConnState::Connecting, None, now), Some(Duration::from_millis(500)));
    }

    #[test]
    fn gain_keys_step_by_one_db_and_alt_makes_them_fine() {
        use egui::Modifiers;
        let shift = Modifiers { shift: true, ..Default::default() };
        let alt = Modifiers { alt: true, ..Default::default() };
        let ctrl = Modifiers { ctrl: true, command: true, ..Default::default() };
        assert_eq!(cell_keys(&[Key::Plus], shift), (vec![CellKey::Up], false), "typing + needs Shift: still 1 dB");
        assert_eq!(cell_keys(&[Key::Equals], Modifiers::NONE), (vec![CellKey::Up], false));
        assert_eq!(cell_keys(&[Key::Plus, Key::Equals], shift), (vec![CellKey::Up], false), "one step, not two");
        assert_eq!(cell_keys(&[Key::Minus], alt), (vec![CellKey::Down], true));
        assert_eq!(cell_keys(&[Key::Equals], ctrl), (vec![], false), "Ctrl+= is zoom, not gain");
        assert_eq!(cell_keys(&[Key::M, Key::Space], Modifiers::NONE), (vec![CellKey::Toggle, CellKey::Mute], false));
    }

    /// After a reconnect the engine's snapshot is the truth: edits pending
    /// against the old connection (or an engine that restarted at version 0)
    /// would otherwise stay outlined forever.
    #[test]
    fn a_fresh_snapshot_is_noticed_once() {
        let mut seen = 1;
        assert!(!fresh_snapshot(&mut seen, 1));
        assert!(fresh_snapshot(&mut seen, 2));
        assert!(!fresh_snapshot(&mut seen, 2));
    }

    #[test]
    fn a_startup_failure_explains_itself() {
        let text = startup_error_text(&"no suitable graphics adapter");
        assert!(text.contains("no suitable graphics adapter"), "{text}");
        assert!(text.contains("audio"), "says that audio is unaffected: {text}");
    }

    #[test]
    fn telemetry_repaints_slowly_unless_live_graphs_are_shown() {
        assert_eq!(telemetry_repaint(false), Some(TELEMETRY_REPAINT), "top-bar figures: a few times a second");
        assert_eq!(telemetry_repaint(true), None, "live graphs: every sample");
        assert!(TELEMETRY_REPAINT >= Duration::from_millis(200));
    }

    #[test]
    fn xruns_flash_again_after_an_engine_restart() {
        let t0 = Instant::now();
        let seen = track_xruns((50, None), 50, t0);
        assert_eq!(seen, (50, None), "no new xruns: no flash");
        let restarted = track_xruns(seen, 0, t0);
        assert_eq!(restarted, (0, None), "a restarted engine counts from 0 again");
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(track_xruns(restarted, 3, t1), (3, Some(t1)), "new xruns flash");
    }

    #[test]
    fn flags_come_from_the_command_line() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(flag(&args(&["--pipe", "lab"]), "--pipe"), Some("lab".into()));
        assert_eq!(flag(&args(&["--skin=C:/skins/x", "--pipe", "lab"]), "--skin"), Some("C:/skins/x".into()));
        assert_eq!(flag(&args(&["--pipe", "lab"]), "--skin"), None);
        assert_eq!(flag(&args(&["--pipe"]), "--pipe"), None);
    }
}
