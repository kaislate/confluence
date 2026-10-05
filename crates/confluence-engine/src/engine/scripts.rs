//! Luau scripts: run on MIDI events, changing routes as user edits.

use confluence_api::{Command, Response, ScriptInfo};
use confluence_core::gain::PointParams;
use confluence_script::{MidiMessage, Routes, ScriptHost};

use super::Engine;
use crate::midi::MidiEvent;

/// Routes as scripts see them: edits go through the usual checks, count as
/// user edits, and are collected for the journal.
struct EngineRoutes<'a> {
    engine: &'a mut Engine,
    journal: &'a mut Vec<Command>,
}

impl Routes for EngineRoutes<'_> {
    fn get(&self, input: u32, output: u32) -> Option<(f32, bool, bool)> {
        self.engine.matrix.point(input, output).map(|p| (p.gain_db, p.mute, p.invert))
    }

    fn set(&mut self, input: u32, output: u32, gain: f32, mute: bool, invert: bool) -> Result<(), String> {
        if !gain.is_finite() {
            return Err(format!("gain must be a number of dB, not {gain}"));
        }
        let p = PointParams { gain_db: gain, mute, invert };
        self.engine.set_point(input, output, p).map_err(|e| e.to_string())?;
        self.engine.scenes.route_changed(input, output);
        self.journal.push(Command::SetPoint { input, output, gain_db: gain, mute, invert });
        Ok(())
    }
}

impl Engine {
    /// Runs every script's `on_midi`; returns their route edits to journal.
    pub(super) fn scripts_on_midi(&mut self, ev: &MidiEvent) -> Vec<Command> {
        let mut journal = Vec::new();
        let mut host = std::mem::take(&mut self.scripts);
        let msg = MidiMessage { device: ev.device.clone(), bytes: ev.bytes.clone() };
        host.on_midi(&msg, &mut EngineRoutes { engine: self, journal: &mut journal });
        self.scripts = host;
        journal
    }

    pub fn script_infos(&self) -> Vec<ScriptInfo> {
        self.scripts.infos()
    }

    /// Every script, as journal records.
    pub fn script_commands(&self) -> Vec<Command> {
        self.scripts
            .infos()
            .into_iter()
            .map(|s| Command::SetScript { name: s.name, source: s.source, enabled: s.enabled })
            .collect()
    }

    pub(super) fn script_command(&mut self, cmd: &Command) -> Response {
        let r = match cmd {
            Command::SetScript { name, source, enabled } => self.scripts.set(name, source, *enabled),
            Command::DeleteScript { name } => self.scripts.delete(name),
            _ => return Response::Error("not a script command".into()),
        };
        match r {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e),
        }
    }
}

/// Keeps `ScriptHost` usable from the engine's threads.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<ScriptHost>();
};
