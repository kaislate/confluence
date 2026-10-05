//! MIDI Learn and the bindings of hardware controls to route gains, with
//! feedback so motorized faders and LED rings follow the gain.

use std::collections::HashMap;

use confluence_api::taper::{cc_to_db, db_to_cc};
use confluence_api::{Command, MidiBinding, Response};
use confluence_core::gain::PointParams;

use super::{Engine, EngineError};
use crate::midi::MidiEvent;

/// A control: (device, channel 1-16, cc).
type Control = (String, u8, u8);

#[derive(Default)]
pub(super) struct Midi {
    bindings: Vec<MidiBinding>,
    learning: Option<(u32, u32)>,
    /// The value each control last had, sent or received: feedback only
    /// sends what differs, and never echoes a control's own move.
    last: HashMap<Control, u8>,
    inputs: Vec<String>,
}

fn key(b: &MidiBinding) -> Control {
    (b.device.clone(), b.channel, b.cc)
}

impl Engine {
    /// Arms MIDI Learn: the next CC that arrives binds to this route's gain.
    pub fn learn_midi(&mut self, input: u32, output: u32) -> Result<(), EngineError> {
        if self.matrix.point(input, output).is_none() {
            return Err(EngineError::NoRoute(input, output));
        }
        self.midi.learning = Some((input, output));
        Ok(())
    }

    pub fn cancel_midi_learn(&mut self) {
        self.midi.learning = None;
    }

    pub fn midi_learning(&self) -> Option<(u32, u32)> {
        self.midi.learning
    }

    /// Binds a control, replacing a binding of the same control.
    pub fn set_midi_binding(&mut self, b: MidiBinding) {
        let k = key(&b);
        self.midi.bindings.retain(|x| key(x) != k);
        self.midi.last.remove(&k); // its route's value is sent on the next feedback
        self.midi.bindings.push(b);
    }

    pub fn remove_midi_binding(&mut self, device: &str, channel: u8, cc: u8) -> Result<(), EngineError> {
        let before = self.midi.bindings.len();
        self.midi.bindings.retain(|b| !(b.device == device && b.channel == channel && b.cc == cc));
        if self.midi.bindings.len() == before {
            return Err(EngineError::NoMidiBinding(device.to_string(), channel, cc));
        }
        Ok(())
    }

    pub fn midi_bindings(&self) -> &[MidiBinding] {
        &self.midi.bindings
    }

    /// The MIDI inputs open now (set by whoever owns the devices).
    pub fn set_midi_inputs(&mut self, inputs: Vec<String>) {
        self.midi.inputs = inputs;
    }

    pub fn midi_inputs(&self) -> &[String] {
        &self.midi.inputs
    }

    /// Handles a message from a MIDI input; returns what to journal. Only
    /// control changes matter: while learning, the first one binds; otherwise
    /// each binding of that control sets its route's gain (mute and phase
    /// kept; a route that is gone is left alone).
    pub fn midi_event(&mut self, ev: &MidiEvent) -> Vec<Command> {
        let [status, number, value] = match ev.bytes[..] {
            [s, n, v] if s & 0xF0 == 0xB0 => [s, n, v & 0x7F],
            _ => return Vec::new(),
        };
        let channel = (status & 0x0F) + 1;
        let control: Control = (ev.device.clone(), channel, number);
        if let Some((input, output)) = self.midi.learning.take() {
            let binding = MidiBinding { device: ev.device.clone(), channel, cc: number, input, output };
            self.set_midi_binding(binding.clone());
            return vec![Command::SetMidiBinding { binding }];
        }
        let routes: Vec<(u32, u32)> =
            self.midi.bindings.iter().filter(|b| key(b) == control).map(|b| (b.input, b.output)).collect();
        if routes.is_empty() {
            return Vec::new();
        }
        self.midi.last.insert(control, value);
        let mut out = Vec::new();
        for (input, output) in routes {
            let Some(cur) = self.matrix.point(input, output) else { continue };
            let p = PointParams { gain_db: cc_to_db(value), mute: cur.mute, invert: cur.invert };
            if self.matrix.set_point(input, output, p).is_ok() {
                self.scenes.route_changed(input, output);
                out.push(Command::SetPoint { input, output, gain_db: p.gain_db, mute: p.mute, invert: p.invert });
            }
        }
        out
    }

    /// The control changes to send so each bound control shows its route's
    /// gain: only values that differ from what the control last had.
    pub fn midi_feedback(&mut self) -> Vec<(String, [u8; 3])> {
        let mut out = Vec::new();
        for b in &self.midi.bindings {
            let Some(cur) = self.matrix.point(b.input, b.output) else { continue };
            let v = db_to_cc(cur.gain_db, cur.mute);
            let k = key(b);
            if self.midi.last.get(&k) != Some(&v) {
                self.midi.last.insert(k, v);
                out.push((b.device.clone(), [0xB0 | (b.channel.clamp(1, 16) - 1), b.cc & 0x7F, v]));
            }
        }
        out
    }

    /// Every binding, as journal records.
    pub fn midi_commands(&self) -> Vec<Command> {
        self.midi.bindings.iter().map(|b| Command::SetMidiBinding { binding: b.clone() }).collect()
    }

    pub(super) fn midi_command(&mut self, cmd: &Command) -> Response {
        let r = match cmd {
            Command::LearnMidi { input, output } => self.learn_midi(*input, *output),
            Command::CancelMidiLearn => {
                self.cancel_midi_learn();
                Ok(())
            }
            Command::SetMidiBinding { binding } => {
                self.set_midi_binding(binding.clone());
                Ok(())
            }
            Command::RemoveMidiBinding { device, channel, cc } => self.remove_midi_binding(device, *channel, *cc),
            Command::InjectMidi { device, bytes } => {
                if bytes.is_empty() || bytes.len() > 3 {
                    Err(EngineError::MidiLength)
                } else {
                    self.midi_event(&MidiEvent { device: device.clone(), bytes: bytes.clone() });
                    Ok(())
                }
            }
            _ => return Response::Error("not a MIDI command".into()),
        };
        match r {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e.to_string()),
        }
    }
}
