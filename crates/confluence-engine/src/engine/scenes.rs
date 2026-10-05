//! Scenes: saved route and plugin-parameter values, recalled with a morph.
//! Parameter-only: recalling creates or removes nothing; it changes routes and
//! parameters that exist now and leaves the rest alone.

use std::time::{Duration, Instant};

use confluence_api::{BusRef, Command, PointState, Response, Scene, SceneInfo, SceneParam, MAX_MORPH_MS};
use confluence_core::gain::PointParams;

use super::{BusPlugin, Engine, EngineError};

/// Gain a muted route glides through: treated as silence.
const SILENT_DB: f32 = -100.0;

struct RouteGlide {
    input: u32,
    output: u32,
    from_db: f32,
    /// The scene's gain (kept on the route even when it ends muted).
    to_db: f32,
    to_mute: bool,
    invert: bool,
}

struct ParamGlide {
    bus: u32,
    param: u32,
    from: f64,
    to: f64,
    /// The last value the plugin accepted: a value is sent only when it moves.
    sent: f64,
}

struct Morph {
    started: Instant,
    duration: Duration,
    routes: Vec<RouteGlide>,
    params: Vec<ParamGlide>,
}

/// The engine's scenes, the current one, and the running morph.
#[derive(Default)]
pub(super) struct Scenes {
    list: Vec<Scene>,
    current: Option<String>,
    morph: Option<Morph>,
}

impl Scenes {
    /// Where a parameter is going, if a morph moves it.
    pub(super) fn param_target(&self, bus: u32, param: u32) -> Option<f64> {
        self.morph.as_ref()?.params.iter().find(|g| (g.bus, g.param) == (bus, param)).map(|g| g.to)
    }

    /// The user changed a route: its glide stops, and the mix no longer
    /// matches the current scene.
    pub(super) fn route_changed(&mut self, input: u32, output: u32) {
        if let Some(m) = &mut self.morph {
            m.routes.retain(|g| (g.input, g.output) != (input, output));
        }
        self.current = None;
    }

    /// Whether a morph is moving this parameter.
    pub(super) fn is_gliding(&self, bus: u32, param: u32) -> bool {
        self.morph.as_ref().is_some_and(|m| m.params.iter().any(|g| (g.bus, g.param) == (bus, param)))
    }

    /// The bus's plugin was replaced, unloaded or given a new state: the
    /// morph no longer moves its parameters.
    pub(super) fn bus_changed(&mut self, bus: u32) {
        if let Some(m) = &mut self.morph {
            m.params.retain(|g| g.bus != bus);
        }
    }

    /// The mix changed by other means than a recall.
    pub(super) fn mix_changed(&mut self) {
        self.current = None;
    }

    /// The user changed a plugin parameter.
    pub(super) fn param_changed(&mut self, bus: u32, param: u32) {
        if let Some(m) = &mut self.morph {
            m.params.retain(|g| (g.bus, g.param) != (bus, param));
        }
        self.current = None;
    }
}

fn check(name: &str, morph_ms: u32) -> Result<String, EngineError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EngineError::SceneName);
    }
    if morph_ms > MAX_MORPH_MS {
        return Err(EngineError::MorphTime);
    }
    Ok(name.to_string())
}

impl Engine {
    /// Captures every route's values and every loaded plugin's settable
    /// parameters as scene `name` (replacing a scene with that name).
    pub fn save_scene(&mut self, name: &str, morph_ms: u32) -> Result<(), EngineError> {
        let name = check(name, morph_ms)?;
        let points = self
            .matrix
            .points()
            .into_iter()
            .map(|(input, output, p)| PointState { input, output, gain_db: p.gain_db, mute: p.mute, invert: p.invert })
            .collect();
        let mut params = Vec::new();
        for (bus, p) in &self.plugins {
            let BusPlugin::Loaded { control, .. } = p else { continue };
            let Some(at) = self.slots.iter().find(|s| s.state.id == *bus).map(|s| s.state.first_output) else {
                continue;
            };
            params.extend(control.params().into_iter().filter(|q| !q.read_only).map(|q| SceneParam {
                bus_at: at,
                param: q.id,
                value: q.value,
            }));
        }
        self.put_scene(Scene { name, morph_ms, points, params })
    }

    /// Stores a scene as given (replacing one with its name).
    pub fn put_scene(&mut self, mut scene: Scene) -> Result<(), EngineError> {
        scene.name = check(&scene.name, scene.morph_ms)?;
        match self.scenes.list.iter_mut().find(|s| s.name == scene.name) {
            Some(s) => *s = scene,
            None => self.scenes.list.push(scene),
        }
        Ok(())
    }

    pub fn delete_scene(&mut self, name: &str) -> Result<(), EngineError> {
        let before = self.scenes.list.len();
        self.scenes.list.retain(|s| s.name != name);
        if self.scenes.list.len() == before {
            return Err(EngineError::NoScene(name.to_string()));
        }
        if self.scenes.current.as_deref() == Some(name) {
            self.scenes.current = None;
        }
        Ok(())
    }

    pub fn set_scene_morph(&mut self, name: &str, morph_ms: u32) -> Result<(), EngineError> {
        check(name, morph_ms)?;
        let s =
            self.scenes.list.iter_mut().find(|s| s.name == name).ok_or_else(|| EngineError::NoScene(name.into()))?;
        s.morph_ms = morph_ms;
        Ok(())
    }

    pub fn scene(&self, name: &str) -> Option<&Scene> {
        self.scenes.list.iter().find(|s| s.name == name)
    }

    pub fn scene_infos(&self) -> Vec<SceneInfo> {
        self.scenes
            .list
            .iter()
            .map(|s| SceneInfo {
                name: s.name.clone(),
                morph_ms: s.morph_ms,
                routes: s.points.len() as u32,
                params: s.params.len() as u32,
            })
            .collect()
    }

    /// The scene last recalled, while the mix still matches it.
    pub fn current_scene(&self) -> Option<&str> {
        self.scenes.current.as_deref()
    }

    pub fn morphing(&self) -> bool {
        self.scenes.morph.is_some()
    }

    /// Recalls scene `name` starting at `now`: glides over its morph time, or
    /// at once if `instant` (start-up replay) or its morph time is 0.
    pub fn recall_scene_at(&mut self, name: &str, now: Instant, instant: bool) -> Result<(), EngineError> {
        let scene = self.scene(name).cloned().ok_or_else(|| EngineError::NoScene(name.into()))?;
        // What a running morph was moving and this scene does not cover is
        // finished at once, not left part-way.
        if let Some(old) = self.scenes.morph.take() {
            for g in old.routes {
                let covered = scene.points.iter().any(|p| (p.input, p.output) == (g.input, g.output));
                if !covered && self.matrix.point(g.input, g.output).is_some() {
                    let end = PointParams { gain_db: g.to_db, mute: g.to_mute, invert: g.invert };
                    let _ = self.matrix.set_point(g.input, g.output, end);
                }
            }
            for g in old.params {
                let at = self.slots.iter().find(|s| s.state.id == g.bus).map(|s| s.state.first_output);
                let covered = scene.params.iter().any(|sp| Some(sp.bus_at) == at && sp.param == g.param);
                if !covered {
                    if let Some(BusPlugin::Loaded { control, .. }) = self.plugins.get_mut(&g.bus) {
                        let _ = control.set_param(g.param, g.to);
                    }
                }
            }
        }
        // Editor changes waiting to be saved are superseded by the recall.
        for sp in &scene.params {
            self.edits_held.remove(&(sp.bus_at, sp.param));
        }
        let mut routes = Vec::new();
        for p in &scene.points {
            let Some(cur) = self.matrix.point(p.input, p.output) else { continue };
            if (cur.gain_db, cur.mute, cur.invert) == (p.gain_db, p.mute, p.invert) {
                continue; // already there
            }
            let from_db = if cur.mute { SILENT_DB } else { cur.gain_db.max(SILENT_DB) };
            if cur.mute || cur.invert != p.invert {
                // Unmute at silence (it glides up from there); phase switches now.
                let start = PointParams { gain_db: from_db, mute: false, invert: p.invert };
                let _ = self.matrix.set_point(p.input, p.output, start);
            }
            routes.push(RouteGlide {
                input: p.input,
                output: p.output,
                from_db,
                to_db: p.gain_db,
                to_mute: p.mute,
                invert: p.invert,
            });
        }
        let mut params = Vec::new();
        for sp in &scene.params {
            let Ok(bus) = self.resolve_bus(&BusRef::At(sp.bus_at)) else { continue };
            let Some(BusPlugin::Loaded { control, .. }) = self.plugins.get_mut(&bus) else { continue };
            let Some(q) = control.params().into_iter().find(|q| q.id == sp.param && !q.read_only) else { continue };
            if q.value == sp.value {
                continue; // already there
            }
            if q.stepped {
                let _ = control.set_param(sp.param, sp.value);
            } else {
                params.push(ParamGlide { bus, param: sp.param, from: q.value, to: sp.value, sent: q.value });
            }
        }
        let duration = if instant { Duration::ZERO } else { Duration::from_millis(u64::from(scene.morph_ms)) };
        self.scenes.morph = Some(Morph { started: now, duration, routes, params });
        self.scenes.current = Some(scene.name);
        self.advance_morph(now);
        Ok(())
    }

    /// Moves the running morph to where it should be at `now`.
    pub fn advance_morph(&mut self, now: Instant) {
        if self.scenes.morph.is_none() {
            return;
        }
        // Changes made in a plugin's own editor stop that parameter's glide.
        self.collect_plugin_edits();
        let Some(m) = &mut self.scenes.morph else { return };
        // A route removed meanwhile (by the user, a slot removal, a new bus)
        // stays removed.
        let matrix = &self.matrix;
        m.routes.retain(|g| matrix.point(g.input, g.output).is_some());
        let t = if m.duration.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(m.started).as_secs_f64() / m.duration.as_secs_f64()).min(1.0)
        };
        for g in &m.routes {
            let p = if t >= 1.0 {
                PointParams { gain_db: g.to_db, mute: g.to_mute, invert: g.invert }
            } else {
                let to = if g.to_mute { SILENT_DB } else { g.to_db.max(SILENT_DB) };
                let db = g.from_db + (to - g.from_db) * t as f32;
                PointParams { gain_db: db, mute: false, invert: g.invert }
            };
            let _ = self.matrix.set_point(g.input, g.output, p);
        }
        let mut unsent = false;
        for g in &mut m.params {
            let Some(BusPlugin::Loaded { control, .. }) = self.plugins.get_mut(&g.bus) else { continue };
            let v = if t >= 1.0 { g.to } else { g.from + (g.to - g.from) * t };
            if v != g.sent {
                match control.set_param(g.param, v) {
                    Ok(()) => g.sent = v,
                    Err(_) => unsent = true,
                }
            }
            unsent |= g.sent != g.to && t >= 1.0;
        }
        // Done once every final value went through (a full queue: next tick).
        if t >= 1.0 && !unsent {
            self.scenes.morph = None;
        }
    }

    /// The routes as they will be when the running morph ends (for saving).
    pub fn settled_points(&self) -> Vec<PointState> {
        self.matrix
            .points()
            .into_iter()
            .map(|(input, output, p)| {
                let glide = self
                    .scenes
                    .morph
                    .as_ref()
                    .and_then(|m| m.routes.iter().find(|g| (g.input, g.output) == (input, output)));
                match glide {
                    Some(g) => PointState { input, output, gain_db: g.to_db, mute: g.to_mute, invert: g.invert },
                    None => PointState { input, output, gain_db: p.gain_db, mute: p.mute, invert: p.invert },
                }
            })
            .collect()
    }

    /// Every scene, as journal records.
    pub fn scene_commands(&self) -> Vec<Command> {
        self.scenes.list.iter().map(|s| Command::PutScene { scene: s.clone() }).collect()
    }

    pub(super) fn scene_command(&mut self, cmd: &Command) -> Response {
        let r = match cmd {
            Command::SaveScene { name, morph_ms } => self.save_scene(name, *morph_ms),
            Command::PutScene { scene } => self.put_scene(scene.clone()),
            Command::DeleteScene { name } => self.delete_scene(name),
            Command::SetSceneMorph { name, morph_ms } => self.set_scene_morph(name, *morph_ms),
            Command::RecallScene { name } => self.recall_scene_at(name, Instant::now(), false),
            Command::ListScenes => return Response::Scenes(self.scene_infos()),
            _ => return Response::Error("not a scene command".into()),
        };
        match r {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e.to_string()),
        }
    }
}
