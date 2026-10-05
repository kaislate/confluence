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
        let mut routes = Vec::new();
        for p in &scene.points {
            let Some(cur) = self.matrix.point(p.input, p.output) else { continue };
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
            if q.stepped {
                let _ = control.set_param(sp.param, sp.value);
            } else {
                params.push(ParamGlide { bus, param: sp.param, from: q.value, to: sp.value });
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
        let Some(m) = &self.scenes.morph else { return };
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
        for g in &m.params {
            if let Some(BusPlugin::Loaded { control, .. }) = self.plugins.get_mut(&g.bus) {
                let _ = control.set_param(g.param, g.from + (g.to - g.from) * t);
            }
        }
        if t >= 1.0 {
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
