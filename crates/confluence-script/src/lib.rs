//! Luau scripts for Confluence: each sandboxed in its own state, with a memory
//! limit and a time budget per call, reacting to MIDI and controlling routes.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use confluence_api::{ScriptInfo, ScriptStatus, MAX_SCRIPT_BYTES};
use mlua::{Function, Lua, Table, Value, VmState};

/// Memory a script may use.
pub const MEMORY_LIMIT: usize = 16 << 20;
/// Time a script may run per call (loading, or one event).
pub const CALL_BUDGET: Duration = Duration::from_millis(20);
/// Log lines kept per script.
pub const LOG_LINES: usize = 50;

/// A MIDI message as scripts receive it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiMessage {
    pub device: String,
    pub bytes: Vec<u8>,
}

/// What scripts may do to routes, during an event.
pub trait Routes {
    /// (gain dB, mute, invert) of a route, if it exists.
    fn get(&self, input: u32, output: u32) -> Option<(f32, bool, bool)>;
    /// Creates or updates a route; `Err` becomes a Lua error.
    fn set(&mut self, input: u32, output: u32, gain: f32, mute: bool, invert: bool) -> Result<(), String>;
}

type Log = Arc<Mutex<VecDeque<String>>>;

fn push_log(log: &Log, line: String) {
    if let Ok(mut l) = log.lock() {
        if l.len() == LOG_LINES {
            l.pop_front();
        }
        l.push_back(line);
    }
}

struct Script {
    source: String,
    enabled: bool,
    status: ScriptStatus,
    log: Log,
    lua: Option<Lua>,
    /// When the current call must stop: nanoseconds after `epoch`.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
}

impl Script {
    fn arm(&self) {
        let until = self.epoch.elapsed() + CALL_BUDGET;
        self.deadline.store(until.as_nanos() as u64, Ordering::Relaxed);
    }

    fn stop(&mut self, why: String) {
        self.lua = None;
        self.status = ScriptStatus::Stopped(why);
    }

    /// Creates the script's state and runs its top level.
    fn start(&mut self) {
        match self.load() {
            Ok(lua) => {
                self.lua = Some(lua);
                self.status = ScriptStatus::Running;
            }
            Err(e) => self.stop(e.to_string()),
        }
    }

    fn load(&self) -> mlua::Result<Lua> {
        let lua = Lua::new();
        lua.set_memory_limit(MEMORY_LIMIT)?;
        let (deadline, epoch) = (self.deadline.clone(), self.epoch);
        lua.set_interrupt(move |_| {
            if epoch.elapsed().as_nanos() as u64 > deadline.load(Ordering::Relaxed) {
                Err(mlua::Error::runtime("the script took too long"))
            } else {
                Ok(VmState::Continue)
            }
        });
        // Sandboxing freezes the tables that exist now; the API goes in after,
        // so its functions can be swapped in for each event.
        lua.sandbox(true)?;
        let log = self.log.clone();
        let say = lua.create_function(move |lua, args: mlua::MultiValue| {
            // Like print: anything goes, shown as tostring shows it.
            let tostring: Function = lua.globals().get("tostring")?;
            let parts: Vec<String> = args
                .into_iter()
                .map(|v| match &v {
                    Value::String(s) => s.to_string_lossy(),
                    _ => tostring.call::<String>(v).unwrap_or_else(|_| "?".into()),
                })
                .collect();
            push_log(&log, parts.join("\t"));
            Ok(())
        })?;
        let confluence = lua.create_table()?;
        confluence.set("log", say.clone())?;
        lua.globals().set("print", say)?;
        lua.globals().set("confluence", confluence)?;
        self.arm();
        lua.load(&self.source).set_name("script").exec()?;
        Ok(lua)
    }
}

/// The scripts, by name.
#[derive(Default)]
pub struct ScriptHost {
    scripts: BTreeMap<String, Script>,
}

impl ScriptHost {
    pub fn new() -> ScriptHost {
        ScriptHost::default()
    }

    /// Stores a script (replacing one with that name); starts it if enabled.
    pub fn set(&mut self, name: &str, source: &str, enabled: bool) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a script needs a name".into());
        }
        if source.len() > MAX_SCRIPT_BYTES {
            return Err("scripts are at most 256 KB".into());
        }
        let log = self.scripts.get(name).map(|s| s.log.clone()).unwrap_or_default();
        let mut s = Script {
            source: source.to_string(),
            enabled,
            status: ScriptStatus::Disabled,
            log,
            lua: None,
            deadline: Arc::new(AtomicU64::new(0)),
            epoch: Instant::now(),
        };
        if enabled {
            s.start();
        }
        self.scripts.insert(name.to_string(), s);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        self.scripts.remove(name).map(|_| ()).ok_or_else(|| format!("no script named {name}"))
    }

    /// Every script, sorted by name.
    pub fn infos(&self) -> Vec<ScriptInfo> {
        self.scripts
            .iter()
            .map(|(name, s)| ScriptInfo {
                name: name.clone(),
                source: s.source.clone(),
                enabled: s.enabled,
                status: s.status.clone(),
                log: s.log.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default(),
            })
            .collect()
    }

    /// Calls every running script's `on_midi(m)`. A script that errors or runs
    /// out of time is stopped; the others carry on.
    pub fn on_midi(&mut self, msg: &MidiMessage, routes: &mut dyn Routes) {
        let routes = RefCell::new(routes);
        for s in self.scripts.values_mut() {
            let Some(lua) = &s.lua else { continue };
            s.arm();
            let r = call_on_midi(lua, msg, &routes);
            if let Err(e) = r {
                s.stop(e.to_string());
            }
        }
    }
}

fn message(lua: &Lua, msg: &MidiMessage) -> mlua::Result<Table> {
    let m = lua.create_table()?;
    m.set("device", msg.device.as_str())?;
    m.set("bytes", lua.create_sequence_from(msg.bytes.iter().copied())?)?;
    let b = |i: usize| msg.bytes.get(i).copied();
    match (b(0).map(|s| s & 0xF0), b(1), b(2)) {
        (Some(kind @ (0x80 | 0x90 | 0xB0)), Some(d1), Some(d2)) => {
            m.set("channel", (b(0).unwrap_or(0) & 0x0F) + 1)?;
            match kind {
                0xB0 => {
                    m.set("kind", "cc")?;
                    m.set("cc", d1)?;
                    m.set("value", d2)?;
                }
                _ => {
                    m.set("kind", if kind == 0x90 && d2 > 0 { "note_on" } else { "note_off" })?;
                    m.set("note", d1)?;
                    m.set("velocity", d2)?;
                }
            }
        }
        _ => m.set("kind", "other")?,
    }
    Ok(m)
}

fn call_on_midi(lua: &Lua, msg: &MidiMessage, routes: &RefCell<&mut dyn Routes>) -> mlua::Result<()> {
    let Ok(on_midi) = lua.globals().get::<Function>("on_midi") else { return Ok(()) };
    let m = message(lua, msg)?;
    lua.scope(|scope| {
        let route = scope.create_function(|lua, (input, output): (u32, u32)| {
            let Some((gain, mute, invert)) = routes.borrow().get(input, output) else { return Ok(Value::Nil) };
            let t = lua.create_table()?;
            t.set("gain", gain)?;
            t.set("mute", mute)?;
            t.set("invert", invert)?;
            Ok(Value::Table(t))
        })?;
        let set_route = scope.create_function(
            |_, (input, output, gain, mute, invert): (u32, u32, f32, Option<bool>, Option<bool>)| {
                let mut r = routes.borrow_mut();
                let cur = r.get(input, output);
                let mute = mute.or(cur.map(|c| c.1)).unwrap_or(false);
                let invert = invert.or(cur.map(|c| c.2)).unwrap_or(false);
                r.set(input, output, gain, mute, invert).map_err(mlua::Error::runtime)
            },
        )?;
        let confluence: Table = lua.globals().get("confluence")?;
        confluence.raw_set("route", route)?;
        confluence.raw_set("set_route", set_route)?;
        on_midi.call::<()>(m)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Routes as a map: (input, output) → (gain, mute, invert).
    #[derive(Default)]
    struct Map(HashMap<(u32, u32), (f32, bool, bool)>, Vec<(u32, u32)>);

    impl Routes for Map {
        fn get(&self, input: u32, output: u32) -> Option<(f32, bool, bool)> {
            self.0.get(&(input, output)).copied()
        }
        fn set(&mut self, input: u32, output: u32, gain: f32, mute: bool, invert: bool) -> Result<(), String> {
            if self.1.contains(&(input, output)) {
                return Err("this route would feed an insert bus back into itself".into());
            }
            self.0.insert((input, output), (gain, mute, invert));
            Ok(())
        }
    }

    fn cc(cc: u8, value: u8) -> MidiMessage {
        MidiMessage { device: "Pad".into(), bytes: vec![0xB0, cc, value] }
    }

    fn status(h: &ScriptHost, name: &str) -> ScriptStatus {
        h.infos().into_iter().find(|s| s.name == name).unwrap().status
    }

    #[test]
    fn a_script_reacts_to_midi_by_setting_a_route() {
        let mut h = ScriptHost::new();
        h.set("fader", "function on_midi(m) if m.kind == 'cc' and m.cc == 20 then confluence.set_route(0, 4, -60 + m.value / 127 * 60) end end", true).unwrap();
        assert_eq!(status(&h, "fader"), ScriptStatus::Running);
        let mut routes = Map::default();
        h.on_midi(&cc(20, 127), &mut routes);
        assert_eq!(routes.0[&(0, 4)], (0.0, false, false));
        h.on_midi(&cc(21, 0), &mut routes);
        assert_eq!(routes.0[&(0, 4)].0, 0.0, "another control: nothing");
    }

    #[test]
    fn a_script_reads_routes_and_keeps_mute_and_phase() {
        let mut h = ScriptHost::new();
        let src = "function on_midi(m)
            local r = confluence.route(0, 4)
            if r then confluence.set_route(0, 4, r.gain, not r.mute) end
            if confluence.route(9, 9) == nil then confluence.log('no 9 9') end
        end";
        h.set("toggle", src, true).unwrap();
        let mut routes = Map::default();
        routes.0.insert((0, 4), (-6.0, false, true));
        h.on_midi(&cc(1, 1), &mut routes);
        assert_eq!(routes.0[&(0, 4)], (-6.0, true, true), "mute toggled, phase kept");
        assert_eq!(h.infos()[0].log, ["no 9 9"]);
        h.set("gain", "function on_midi(m) confluence.set_route(0, 4, -3) end", true).unwrap();
        h.on_midi(&cc(1, 1), &mut routes);
        assert!(routes.0[&(0, 4)].2, "set_route without mute/invert keeps them");
    }

    #[test]
    fn a_script_that_fails_to_load_is_stopped_with_the_reason() {
        let mut h = ScriptHost::new();
        h.set("bad", "function on_midi(m", true).unwrap();
        assert!(matches!(status(&h, "bad"), ScriptStatus::Stopped(why) if !why.is_empty()));
    }

    #[test]
    fn an_error_stops_only_that_script() {
        let mut h = ScriptHost::new();
        h.set("a_boom", "function on_midi(m) error('boom') end", true).unwrap();
        h.set("b_ok", "function on_midi(m) confluence.set_route(0, 1, -1) end", true).unwrap();
        let mut routes = Map::default();
        h.on_midi(&cc(1, 1), &mut routes);
        assert!(matches!(status(&h, "a_boom"), ScriptStatus::Stopped(why) if why.contains("boom")));
        assert_eq!(status(&h, "b_ok"), ScriptStatus::Running);
        assert_eq!(routes.0[&(0, 1)].0, -1.0, "the other script ran");
        routes.0.clear();
        h.on_midi(&cc(1, 1), &mut routes);
        assert_eq!(routes.0.len(), 1, "the stopped script is not called again");
        h.set("a_boom", "function on_midi(m) confluence.set_route(0, 2, 0) end", true).unwrap();
        assert_eq!(status(&h, "a_boom"), ScriptStatus::Running, "saving starts it again");
    }

    #[test]
    fn a_runaway_script_is_stopped() {
        let mut h = ScriptHost::new();
        h.set("loop", "function on_midi(m) while true do end end", true).unwrap();
        let started = std::time::Instant::now();
        h.on_midi(&cc(1, 1), &mut Map::default());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(matches!(status(&h, "loop"), ScriptStatus::Stopped(why) if why.contains("too long")));
        h.set("loader", "while true do end", true).unwrap();
        assert!(matches!(status(&h, "loader"), ScriptStatus::Stopped(_)), "also while loading");
        h.set("hog", "function on_midi(m) local t = {} for i = 1, 1e8 do t[i] = i end end", true).unwrap();
        h.on_midi(&cc(1, 1), &mut Map::default());
        assert!(matches!(status(&h, "hog"), ScriptStatus::Stopped(_)), "memory or time: stopped");
    }

    #[test]
    fn logs_keep_the_last_fifty_lines() {
        let mut h = ScriptHost::new();
        h.set(
            "talk",
            "function on_midi(m) for i = 1, 60 do print('line', i) end confluence.log('done', m.cc) end",
            true,
        )
        .unwrap();
        h.on_midi(&cc(7, 1), &mut Map::default());
        let log = &h.infos()[0].log;
        assert_eq!(log.len(), 50);
        assert_eq!(log.last().map(String::as_str), Some("done\t7"));
        assert_eq!(log.first().map(String::as_str), Some("line\t12"));
    }

    #[test]
    fn a_disabled_script_is_not_run() {
        let mut h = ScriptHost::new();
        h.set("off", "function on_midi(m) confluence.set_route(0, 1, 0) end", false).unwrap();
        assert_eq!(status(&h, "off"), ScriptStatus::Disabled);
        let mut routes = Map::default();
        h.on_midi(&cc(1, 1), &mut routes);
        assert!(routes.0.is_empty());
    }

    #[test]
    fn a_refused_route_is_a_lua_error_the_script_can_catch() {
        let mut h = ScriptHost::new();
        let src = "function on_midi(m)
            local ok, err = pcall(confluence.set_route, 5, 5, 0)
            if not ok then confluence.log('refused:', err, {}, nil) end
        end";
        h.set("careful", src, true).unwrap();
        let mut routes = Map::default();
        routes.1.push((5, 5));
        h.on_midi(&cc(1, 1), &mut routes);
        assert_eq!(status(&h, "careful"), ScriptStatus::Running);
        let line = &h.infos()[0].log[0];
        assert!(line.contains("back into itself") && line.ends_with("\tnil"), "{line}");
        assert!(line.contains("\ttable: "), "{line}");
    }

    #[test]
    fn messages_are_described_by_kind() {
        let mut h = ScriptHost::new();
        h.set("show", "function on_midi(m) confluence.log(m.kind, m.channel or '-', m.note or m.cc or '-', m.velocity or m.value or '-', m.device) end", true).unwrap();
        let msgs = [vec![0x91, 36, 100], vec![0x91, 36, 0], vec![0x80, 36, 64], vec![0xB2, 7, 99], vec![0xF8]];
        for b in msgs {
            h.on_midi(&MidiMessage { device: "Pad".into(), bytes: b }, &mut Map::default());
        }
        assert_eq!(
            h.infos()[0].log,
            [
                "note_on\t2\t36\t100\tPad",
                "note_off\t2\t36\t0\tPad",
                "note_off\t1\t36\t64\tPad",
                "cc\t3\t7\t99\tPad",
                "other\t-\t-\t-\tPad",
            ]
        );
    }

    #[test]
    fn names_and_sizes_are_checked() {
        let mut h = ScriptHost::new();
        assert_eq!(h.set("  ", "", true), Err("a script needs a name".into()));
        let big = "-".repeat(confluence_api::MAX_SCRIPT_BYTES + 1);
        assert_eq!(h.set("big", &big, true), Err("scripts are at most 256 KB".into()));
        assert_eq!(h.delete("nope"), Err("no script named nope".into()));
        h.set("x", "", true).unwrap();
        h.delete("x").unwrap();
        assert!(h.infos().is_empty());
    }
}
