//! The window: one `StoreView` per frame drives the rail, the matrix or the
//! Devices screen, the rack panel (inspector) and the scene rail; edits go
//! to the worker. One `Motion` owns every animation and asks for frames
//! only while something moves.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_api::PointState;
use confluence_client::{ConnState, StateStore, StoreView, Update};
use eframe::egui::{self, Align, Align2, Id, Key, Layout, Pos2, Rect, Vec2};

use crate::commands::{Edit, Outcome, Worker};
use crate::engine_launch::Launcher;
use crate::gear::motion::Motion;
use crate::gear::paint;
use crate::gear::skins::{Finish, GearSkin};
use crate::grid_view;
use crate::matrix::{key_edit, move_selection, selection_valid, CellKey, GridLayout, Selection};
use crate::notify::Notes;
use crate::pending::Pending;
use crate::shell;
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

/// What a store update asks of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repaint {
    Now,
    After(Duration),
    Skip,
}

/// How to repaint for `what`: meter frames only while meters are on screen
/// (the Devices screen), telemetry slowly unless live graphs are shown.
pub fn repaint_for(what: Update, graphs_shown: bool, meters_shown: bool) -> Repaint {
    match what {
        Update::Meters if !meters_shown => Repaint::Skip,
        Update::Telemetry => telemetry_repaint(graphs_shown).map_or(Repaint::Now, Repaint::After),
        _ => Repaint::Now,
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

/// The cell size at which `cols` × `rows` cells fit `area` (the cell area,
/// headers excluded), within the zoom range.
pub fn fit_cell(area: Vec2, rows: usize, cols: usize) -> f32 {
    if rows == 0 || cols == 0 {
        return crate::matrix::CELL_DEFAULT;
    }
    let by_w = area.x / cols as f32;
    let by_h = area.y / rows as f32;
    by_w.min(by_h).floor().clamp(crate::matrix::CELL_MIN, crate::matrix::CELL_MAX)
}

/// The two screens the central panel switches between.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Screen {
    Matrix,
    Devices,
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
    /// Whether meters are on screen (meter frames repaint only then).
    meters_live: Arc<AtomicBool>,
    look: Look,
    skin_dir: Option<PathBuf>,
    /// A slot waiting for "Remove ‹name›?" to be confirmed.
    confirm_remove: Option<u32>,
    /// Which screen the central panel shows.
    screen: Screen,
    screen_state: crate::devices_screen::ScreenState,
    /// The plugin picker, while open.
    picker: Option<crate::plugins::Picker>,
    /// Parameter values sent and not yet confirmed: (bus, param) → value.
    param_pending: HashMap<(u32, u32), f64>,
    /// A bus whose plugin is being loaded.
    plugin_loading: Option<u32>,
    scene_bar: crate::scenes::SceneBar,
    scripts_open: bool,
    scripts: crate::scripts::ScriptsUi,
    /// The gear finish (Settings), and the one egui's widgets were last styled for.
    finish: Finish,
    styled_for: Option<Finish>,
    settings_open: bool,
    motion: Motion,
    /// Engine notices and when they were first seen (they toast once).
    notices_seen: HashMap<String, Instant>,
    /// The matrix's routes last frame (a new one pops in).
    routes_seen: HashSet<(u32, u32)>,
    grid_state: grid_view::GridState,
    /// The cell area's size last frame, for Ctrl+0.
    matrix_area: Vec2,
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
        let meters_live = Arc::new(AtomicBool::new(false));
        // Always with meters: the Devices screen shows them.
        let store = StateStore::spawn_with_meters(config.pipe.clone(), {
            let (repaint, graphs_live, meters_live) = (repaint.clone(), graphs_live.clone(), meters_live.clone());
            Box::new(move |what| {
                if let Ok(ctx) = repaint.lock() {
                    if let Some(ctx) = ctx.as_ref() {
                        let (graphs, meters) =
                            (graphs_live.load(Ordering::Relaxed), meters_live.load(Ordering::Relaxed));
                        match repaint_for(what, graphs, meters) {
                            Repaint::Now => ctx.request_repaint(),
                            Repaint::After(after) => ctx.request_repaint_after(after),
                            Repaint::Skip => {}
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
            meters_live,
            look: Look::builtin(),
            skin_dir: config.skin,
            confirm_remove: None,
            screen: Screen::Matrix,
            screen_state: crate::devices_screen::ScreenState::default(),
            picker: None,
            param_pending: HashMap::new(),
            plugin_loading: None,
            scene_bar: crate::scenes::SceneBar::default(),
            scripts_open: false,
            scripts: crate::scripts::ScriptsUi::default(),
            finish: Finish::default(),
            styled_for: None,
            settings_open: false,
            motion: Motion::default(),
            notices_seen: HashMap::new(),
            routes_seen: HashSet::new(),
            grid_state: grid_view::GridState::default(),
            matrix_area: Vec2::new(800.0, 600.0),
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

    /// The gear skin for the finish chosen in Settings.
    pub fn skin(&self) -> GearSkin {
        GearSkin::preset(self.finish)
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
            self.screen_state.adding_bus = false;
            self.screen_state.bus_name.clear();
            self.notes.info(format!("Added insert bus {name}"), now);
            if let Some(id) = ids.first() {
                self.selection = Selection::Slot(*id);
            }
        }
        if let Edit::AddDevice { kind, name } = edit {
            let base = crate::devices::base_name(*kind, name);
            self.notes.info(format!("Added {} {}", crate::devices::kind_title(*kind), base), now);
            if let Some(id) = ids.first() {
                self.selection = Selection::Slot(*id);
            }
        }
        if let Edit::FillPosition { pos, name, .. } = edit {
            self.screen_state.filling.remove(pos);
            self.notes.info(format!("{} now holds {name}", pos.label()), now);
            if let Some(id) = ids.first() {
                self.selection = Selection::Slot(*id);
            }
        }
        if let Edit::SetVirtual { pos, on, .. } = edit {
            let what = if *on { "Turned on" } else { "Turned off" };
            self.notes.info(format!("{what} {}", pos.label()), now);
        }
        if let Edit::ClearPosition { pos } = edit {
            self.notes.info(format!("Cleared {}", pos.label()), now);
        }
        if let Edit::SetMaster { pos } = edit {
            let what = pos.map_or("the internal clock".to_string(), |p| p.label());
            self.notes.info(format!("The master clock will be {what} from the next start"), now);
        }
    }

    fn on_failed(&mut self, edit: &Edit, _now: Instant) {
        self.param_answered(edit);
        if let Edit::AddBus { .. } = edit {
            self.screen_state.adding_bus = false;
        }
        if let Edit::FillPosition { pos, .. } = edit {
            self.screen_state.filling.remove(pos);
            self.screen_state.just_filled.remove(pos);
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
                crate::gear::install_fonts(&ctx);
                ctx.options_mut(|o| o.input_options.max_click_dist = CLICK_DIST);
                *r = Some(ctx.clone());
            }
        }
        if self.skin_dir.is_none() && self.styled_for != Some(self.finish) {
            crate::gear::apply_visuals(&ctx, &self.skin());
            self.styled_for = Some(self.finish);
        }
        self.motion.begin_frame(&ctx);
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
        self.shortcuts(ui);

        let graphs = self.inspector_open && matches!(self.selection, Selection::Slot(_));
        self.graphs_live.store(graphs, Ordering::Relaxed);
        self.meters_live.store(self.screen == Screen::Devices, Ordering::Relaxed);
        self.rail(ui, &view, now);
        self.scene_rail(ui, &view);
        self.side_panels(ui, &view, now);
        if matches!(view.conn, ConnState::Connecting) && view.state.is_none() {
            self.powered_off(ui, now);
        } else {
            match self.screen {
                Screen::Matrix => {
                    self.matrix(ui, &view);
                    self.keyboard(ui, &view);
                }
                Screen::Devices => {
                    self.devices_screen(ui, &view);
                }
            }
        }
        self.notifications(&ctx, &view, now);
        self.dialogs(&ctx, &view);

        if let Some(wait) = next_check(&view.conn, view.last_event, now) {
            ctx.request_repaint_after(wait);
        }
        if self.notes.has_info() || self.xruns.1.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.motion.end_frame(&ctx);
    }

    /// Ctrl+1 / Ctrl+2 switch screens; Ctrl+0 fits the matrix.
    fn shortcuts(&mut self, ui: &egui::Ui) {
        if ui.ctx().memory(|m| m.focused().is_some()) {
            return;
        }
        let (one, two, zero) = ui.input(|i| {
            let c = i.modifiers.command;
            (c && i.key_pressed(Key::Num1), c && i.key_pressed(Key::Num2), c && i.key_pressed(Key::Num0))
        });
        if one {
            self.screen = Screen::Matrix;
        }
        if two {
            self.screen = Screen::Devices;
        }
        if zero {
            if let Some(state) = &self.view.state {
                let l = GridLayout::new(&state.slots, self.cell);
                self.cell = fit_cell(self.matrix_area, l.rows.len, l.cols.len);
            }
        }
    }

    fn scene_rail(&mut self, ui: &mut egui::Ui, view: &StoreView) {
        let Some(state) = &view.state else { return };
        let editable = self.live();
        let skin = self.skin();
        let (bar, motion) = (&mut self.scene_bar, &mut self.motion);
        let edits = egui::Panel::bottom("scene-bar")
            .frame(egui::Frame::NONE)
            .exact_size(shell::SCENE_RAIL_H)
            .show_separator_line(false)
            .show(ui, |ui| {
                let r = ui.max_rect();
                shell::rail_face(ui.painter(), r, &skin);
                paint::seam(ui.painter(), Pos2::new(r.left(), r.top()), Pos2::new(r.right(), r.top()), &skin);
                let inner = Rect::from_min_max(r.min + Vec2::new(12.0, 8.0), r.max - Vec2::new(12.0, 8.0));
                let mut child =
                    ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
                crate::scenes::show(&mut child, state, bar, &skin, motion, editable)
            })
            .inner;
        for e in edits {
            self.send(e);
        }
    }

    fn rail(&mut self, ui: &mut egui::Ui, view: &StoreView, now: Instant) {
        let skin = self.skin();
        let text = badge(&view.conn, view.last_event, now);
        let (status, dsp_warn) = match &view.status {
            Some(s) => {
                self.xruns = track_xruns(self.xruns, s.xruns, now);
                (Some(s.clone()), self.look.dsp_color(s.dsp_load))
            }
            None => (None, None),
        };
        let flashing = self.xruns.1.is_some_and(|t| now.saturating_duration_since(t) < XRUN_FLASH);
        if !flashing {
            self.xruns.1 = None;
        }
        let motion = &mut self.motion;
        let screen = &mut self.screen;
        let (inspector, scripts, settings) =
            (&mut self.inspector_open, &mut self.scripts_open, &mut self.settings_open);
        egui::Panel::top("top-bar").frame(egui::Frame::NONE).exact_size(shell::RAIL_H).show_separator_line(false).show(
            ui,
            |ui| {
                let r = ui.max_rect();
                shell::rail_face(ui.painter(), r, &skin);
                let inner = Rect::from_min_max(r.min + Vec2::new(10.0, 6.0), r.max - Vec2::new(10.0, 8.0));
                let mut row =
                    ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
                row.spacing_mut().item_spacing.x = 6.0;
                shell::wordmark(&mut row, &skin);
                shell::segmented(&mut row, &skin, screen);
                row.add_space(14.0);
                shell::engine_cluster(&mut row, &skin, motion, &text, status.as_ref(), flashing, dsp_warn);
                row.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (open, label) in [(settings, "Settings…"), (scripts, "Scripts…"), (inspector, "Inspector")]
                    {
                        let resp = paint::pill_lit(ui, label, label, *open, &skin);
                        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, *open, label));
                        if resp.clicked() {
                            *open = !*open;
                        }
                    }
                });
            },
        );
        self.banner(ui, view, now);
    }

    /// A thin strip under the rail while the engine is away or quiet.
    fn banner(&mut self, ui: &mut egui::Ui, view: &StoreView, now: Instant) {
        let skin = self.skin();
        let (text, offer_start) = match view.conn {
            ConnState::Connecting if view.state.is_some() => ("Engine not running".to_string(), true),
            ConnState::Connecting => return, // the powered-off face says it
            ConnState::Reconnecting { since } => {
                ("Reconnecting…".to_string(), now.saturating_duration_since(since) >= OFFER_START_AFTER)
            }
            ConnState::Live => {
                if view.last_event.is_some_and(|t| now.saturating_duration_since(t) >= QUIET) {
                    ("Engine not responding".to_string(), false)
                } else {
                    return;
                }
            }
        };
        let ready = self.launcher.ready(now);
        let mut start = false;
        egui::Panel::top("banner").frame(egui::Frame::NONE).exact_size(30.0).show_separator_line(false).show(
            ui,
            |ui| {
                let r = ui.max_rect();
                ui.painter().rect_filled(r, egui::CornerRadius::ZERO, skin.ground);
                ui.painter().rect_filled(r, egui::CornerRadius::ZERO, paint::alpha(crate::gear::skins::AMBER, 0.12));
                let inner = Rect::from_min_max(r.min + Vec2::new(16.0, 3.0), r.max - Vec2::new(16.0, 3.0));
                let mut row =
                    ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
                let (tr, label) = row.allocate_exact_size(Vec2::new(170.0, 24.0), egui::Sense::hover());
                label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
                paint::etched_text(
                    row.painter(),
                    tr.left_center(),
                    Align2::LEFT_CENTER,
                    &text,
                    &skin,
                    crate::gear::skins::AMBER,
                    11.5,
                    true,
                    0.04,
                    1.0,
                );
                if offer_start {
                    start = row
                        .add_enabled_ui(ready, |ui| paint::pill_labeled(ui, "Start engine", "Start engine", &skin))
                        .inner
                        .clicked()
                        && ready;
                }
            },
        );
        if start {
            if let Err(e) = self.launcher.start(now) {
                self.notes.error(e, now);
            }
        }
    }

    /// No engine has ever answered: the rack is switched off.
    fn powered_off(&mut self, ui: &mut egui::Ui, now: Instant) {
        let skin = self.skin();
        let ready = self.launcher.ready(now);
        let motion = &mut self.motion;
        let start = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| shell::powered_off(ui, &skin, motion, ready, "Engine not running"))
            .inner;
        if start {
            if let Err(e) = self.launcher.start(now) {
                self.notes.error(e, now);
            }
        }
    }

    /// Shows the matrix or the Devices screen.
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// The gear finish chosen in Settings.
    pub fn finish(&self) -> Finish {
        self.finish
    }

    /// Restores the finish saved last time (see [`crate::settings::FINISH_KEY`]).
    pub fn set_finish(&mut self, finish: Finish) {
        self.finish = finish;
    }

    /// Reduce motion (Settings): tweens land at once.
    pub fn set_reduce_motion(&mut self, reduce: bool) {
        self.motion.reduce = reduce;
    }

    pub fn reduce_motion(&self) -> bool {
        self.motion.reduce
    }

    /// The Devices screen in the central panel.
    fn devices_screen(&mut self, ui: &mut egui::Ui, view: &StoreView) {
        let editable = self.live();
        let skin = self.skin();
        let palette = self.look.skin.slot_colors.clone();
        let list = view.state.as_ref().map(|s| s.devices.clone()).unwrap_or_default();
        let (screen_state, motion) = (&mut self.screen_state, &mut self.motion);
        let actions = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                paint::ground(ui.painter(), ui.max_rect(), &skin);
                let inner = ui.max_rect().shrink2(Vec2::new(0.0, 0.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
                crate::devices_screen::show(&mut child, view, &list, &skin, &palette, screen_state, motion, editable)
            })
            .inner;
        for a in actions {
            match a {
                crate::devices_screen::ScreenAction::Edit(e) => self.send(e),
                crate::devices_screen::ScreenAction::Select(id) => self.selection = Selection::Slot(id),
            }
        }
    }

    fn side_panels(&mut self, ui: &mut egui::Ui, view: &StoreView, _now: Instant) {
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
        let skin = self.skin();
        let motion = &mut self.motion;
        let actions = egui::Panel::right("inspector")
            .frame(egui::Frame::NONE)
            .resizable(true)
            .default_size(320.0)
            .show_separator_line(false)
            .show(ui, |ui| {
                let r = ui.max_rect();
                // The panel keeps its width: its content is laid out in a child.
                ui.expand_to_include_rect(r);
                paint::ground(ui.painter(), r, &skin);
                paint::seam(
                    ui.painter(),
                    Pos2::new(r.left() + 0.5, r.top()),
                    Pos2::new(r.left() + 0.5, r.bottom()),
                    &skin,
                );
                let reveal = shell::reveal(motion, Id::new("inspector-reveal"));
                let (l1, l2) = view
                    .state
                    .as_ref()
                    .map(|s| crate::inspector::header(s, &selection, point.as_ref()))
                    .unwrap_or_default();
                let oled = Rect::from_min_size(r.min + Vec2::new(16.0, 14.0), Vec2::new(r.width() - 32.0, 52.0));
                paint::oled(ui.painter(), oled, &skin, &l1, &l2, crate::gear::skins::OLED_CYAN);
                let body =
                    Rect::from_min_max(Pos2::new(r.left() + 16.0, oled.bottom() + 16.0), r.max - Vec2::new(12.0, 8.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
                child.set_opacity(reveal);
                egui::ScrollArea::vertical()
                    .show(&mut child, |ui| {
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
        if self.settings_open {
            let mut reduce = self.motion.reduce;
            crate::settings::show(ctx, &mut self.settings_open, &mut self.finish, &mut reduce);
            self.motion.reduce = reduce;
        }
        if let (true, Some(state)) = (self.scripts_open, view.state.as_ref()) {
            let editable = self.live();
            let scripts = &mut self.scripts;
            let edits = egui::Window::new("Scripts")
                .open(&mut self.scripts_open)
                .default_width(520.0)
                .show(ctx, |ui| crate::scripts::show(ui, state, scripts, editable))
                .and_then(|r| r.inner)
                .unwrap_or_default();
            for e in edits {
                self.send(e);
            }
        }
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
        let skin = self.skin();
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            if self.look.has_image("background") {
                self.look.paint_surface(ui.painter(), ui.max_rect(), "background", self.look.skin.colors.background);
            } else {
                paint::ground(ui.painter(), ui.max_rect(), &skin);
            }
            let reveal = shell::reveal(&mut self.motion, Id::new("matrix-reveal"));
            let Some(state) = &view.state else {
                paint::etched_text(
                    ui.painter(),
                    ui.max_rect().center(),
                    Align2::CENTER_CENTER,
                    "Waiting for the engine…",
                    &skin,
                    skin.ground_ink,
                    13.0,
                    false,
                    0.0,
                    0.7,
                );
                return;
            };
            if state.slots.is_empty() {
                paint::etched_text(
                    ui.painter(),
                    ui.max_rect().center(),
                    Align2::CENTER_CENTER,
                    "No slots yet: add a device on the Devices screen",
                    &skin,
                    skin.ground_ink,
                    13.0,
                    false,
                    0.0,
                    0.7,
                );
                return;
            }
            let layout = GridLayout::new(&state.slots, self.cell);
            let by_point: HashMap<(u32, u32), &PointState> =
                state.points.iter().map(|p| ((p.input, p.output), p)).collect();
            let pending = &self.pending;
            let lookup = |p: (u32, u32)| (pending.effective(p, by_point.get(&p).copied()), pending.is_pending(p));
            let selected = match self.selection {
                Selection::Cell { input, output } => Some((input, output)),
                _ => None,
            };
            // Routes that appeared since last frame pop in.
            let routed: HashSet<(u32, u32)> = state.points.iter().map(|p| (p.input, p.output)).collect();
            let fresh: HashSet<(u32, u32)> = routed.difference(&self.routes_seen).copied().collect();
            self.routes_seen = routed;
            let inner = ui.max_rect().shrink(12.0);
            self.matrix_area = inner.size() - Vec2::new(grid_view::HEADER_W, grid_view::HEADER_H);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
            child.set_opacity(reveal);
            let actions = grid_view::show(
                &mut child,
                &layout,
                &self.look,
                &skin,
                &mut self.motion,
                &mut self.grid_state,
                &lookup,
                selected,
                &fresh,
                editable,
            );
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

    fn notifications(&mut self, ctx: &egui::Context, view: &StoreView, now: Instant) {
        let skin = self.skin();
        let notices = view.state.as_ref().map(|s| s.notices.clone()).unwrap_or_default();
        let shown = self.notes.shown();
        let dismiss = shell::toasts(ctx, &skin, &mut self.motion, &shown, &notices, &mut self.notices_seen, now);
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
        storage.set_string(crate::settings::FINISH_KEY, self.finish.name().to_string());
        storage.set_string(crate::settings::REDUCE_MOTION_KEY, self.motion.reduce.to_string());
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
    fn meters_repaint_only_while_they_are_on_screen() {
        assert_eq!(repaint_for(Update::Meters, false, false), Repaint::Skip, "the matrix shows no meters");
        assert_eq!(repaint_for(Update::Meters, false, true), Repaint::Now, "the Devices screen does");
        assert_eq!(repaint_for(Update::State, false, false), Repaint::Now);
        assert_eq!(repaint_for(Update::Telemetry, false, false), Repaint::After(TELEMETRY_REPAINT));
        assert_eq!(repaint_for(Update::Telemetry, true, false), Repaint::Now);
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

    #[test]
    fn fit_picks_the_cell_that_shows_the_whole_grid() {
        assert_eq!(fit_cell(Vec2::new(800.0, 600.0), 20, 30), 26.0, "limited by the width: 800 / 30");
        assert_eq!(fit_cell(Vec2::new(800.0, 200.0), 20, 10), 12.0, "clamped at the minimum");
        assert_eq!(fit_cell(Vec2::new(8000.0, 6000.0), 2, 2), crate::matrix::CELL_MAX);
        assert_eq!(fit_cell(Vec2::new(800.0, 600.0), 0, 5), crate::matrix::CELL_DEFAULT);
    }
}
