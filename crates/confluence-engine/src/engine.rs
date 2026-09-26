//! The control half of the engine: slot registry, channel allocation, matrix
//! control and Control API command handling. Not real-time; call [`Engine::tick`]
//! every 10–20 ms from the control thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use confluence_api::{ClockRole, Command, PointState, Response, SlotHealth, SlotState};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, soft_output, BridgeConfig, BridgeStats, InputDeviceSide, OutputDeviceSide};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::mailbox::{self, Receiver, Sender};
use confluence_core::matrix::{matrix, MatrixController};

use crate::audio::{AudioEngine, AudioMsg, InputEntry, OutputEntry, Returned, MAX_SLOTS};

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub sample_rate: f64,
    pub block: usize,
    pub max_inputs: usize,
    pub max_outputs: usize,
    pub ramp: Duration,
    /// Safety margin for soft-slot rings, in frames (spec default 0.5 ms).
    pub margin_frames: usize,
}

impl EngineConfig {
    /// Spec defaults: 1024 × 1024 channels, 10 ms ramps, 0.5 ms margin.
    pub fn new(sample_rate: f64, block: usize) -> Self {
        Self {
            sample_rate,
            block,
            max_inputs: 1024,
            max_outputs: 1024,
            ramp: Duration::from_millis(10),
            margin_frames: (sample_rate * 0.0005).round() as usize,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EngineError {
    #[error("not enough free {0} channels")]
    ChannelsExhausted(&'static str),
    #[error("too many soft slots")]
    TooManySlots,
    #[error("no slot with id {0}")]
    NoSuchSlot(u32),
    #[error("point outside the matrix")]
    OutOfRange,
    #[error("resampler: {0}")]
    Asrc(String),
    #[error("audio side is not accepting messages")]
    Busy,
}

/// Parameters of a soft-clocked device slot.
#[derive(Clone, Debug)]
pub struct SoftSlotSpec {
    pub name: String,
    pub channels: usize,
    pub device_rate: f64,
    pub device_block: usize,
    pub quality: AsrcQuality,
}

struct SlotRecord {
    state: SlotState,
    stats: Arc<BridgeStats>,
}

pub struct Engine {
    cfg: EngineConfig,
    matrix: MatrixController,
    to_audio: Sender<AudioMsg>,
    returns: Receiver<Returned>,
    slots: Vec<SlotRecord>,
    next_id: u32,
    next_input: u32,
    next_output: u32,
    soft_inputs: usize,
    soft_outputs: usize,
    blocks: Arc<AtomicU64>,
}

impl Engine {
    /// Builds a connected control/audio pair.
    pub fn new(cfg: EngineConfig) -> (Engine, AudioEngine) {
        let (matrix_ctl, router) = matrix(cfg.max_inputs, cfg.max_outputs, cfg.ramp, cfg.sample_rate as f32);
        let (to_audio, inbox) = mailbox::channel(4 * MAX_SLOTS);
        let (returns_tx, returns) = mailbox::channel(4 * MAX_SLOTS);
        let blocks = Arc::new(AtomicU64::new(0));
        let mut inputs = PlanarBuffer::new(cfg.max_inputs, cfg.block);
        let mut outputs = PlanarBuffer::new(cfg.max_outputs, cfg.block);
        inputs.set_frames(cfg.block);
        outputs.set_frames(cfg.block);
        let audio = AudioEngine {
            router,
            inputs,
            outputs,
            soft_inputs: Vec::with_capacity(MAX_SLOTS),
            soft_outputs: Vec::with_capacity(MAX_SLOTS),
            inbox,
            returns: returns_tx,
            blocks: blocks.clone(),
        };
        let engine = Engine {
            cfg,
            matrix: matrix_ctl,
            to_audio,
            returns,
            slots: Vec::new(),
            next_id: 1,
            next_input: 0,
            next_output: 0,
            soft_inputs: 0,
            soft_outputs: 0,
            blocks,
        };
        (engine, audio)
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// Master blocks processed so far.
    pub fn blocks(&self) -> u64 {
        self.blocks.load(Ordering::Relaxed)
    }

    /// Adds a soft-clocked capture slot. The returned device side belongs in the
    /// device's callback; its channels appear as matrix inputs.
    pub fn add_soft_input(&mut self, spec: &SoftSlotSpec) -> Result<(u32, InputDeviceSide), EngineError> {
        if self.soft_inputs >= MAX_SLOTS {
            return Err(EngineError::TooManySlots);
        }
        let first = self.alloc(spec.channels, true)?;
        let (device, side, stats) =
            soft_input(self.bridge_config(spec)).map_err(|e| EngineError::Asrc(e.to_string()))?;
        let id = self.next_id;
        let entry = Box::new(InputEntry { id, first_channel: first as usize, channels: spec.channels, side });
        self.to_audio.try_send(AudioMsg::AddInput(entry)).map_err(|_| EngineError::Busy)?;
        self.next_id += 1;
        self.next_input += spec.channels as u32;
        self.soft_inputs += 1;
        self.slots.push(SlotRecord {
            state: SlotState {
                id,
                name: spec.name.clone(),
                role: ClockRole::Soft,
                first_input: first,
                inputs: spec.channels as u32,
                first_output: 0,
                outputs: 0,
            },
            stats,
        });
        Ok((id, device))
    }

    /// Adds a soft-clocked playback slot; its channels appear as matrix outputs.
    pub fn add_soft_output(&mut self, spec: &SoftSlotSpec) -> Result<(u32, OutputDeviceSide), EngineError> {
        if self.soft_outputs >= MAX_SLOTS {
            return Err(EngineError::TooManySlots);
        }
        let first = self.alloc(spec.channels, false)?;
        let (side, device, stats) =
            soft_output(self.bridge_config(spec)).map_err(|e| EngineError::Asrc(e.to_string()))?;
        let id = self.next_id;
        let entry = Box::new(OutputEntry { id, first_channel: first as usize, side });
        self.to_audio.try_send(AudioMsg::AddOutput(entry)).map_err(|_| EngineError::Busy)?;
        self.next_id += 1;
        self.next_output += spec.channels as u32;
        self.soft_outputs += 1;
        self.slots.push(SlotRecord {
            state: SlotState {
                id,
                name: spec.name.clone(),
                role: ClockRole::Soft,
                first_input: 0,
                inputs: 0,
                first_output: first,
                outputs: spec.channels as u32,
            },
            stats,
        });
        Ok((id, device))
    }

    /// Detaches a slot. Its channels go silent; its matrix points are kept.
    pub fn remove_slot(&mut self, id: u32) -> Result<(), EngineError> {
        let idx = self.slots.iter().position(|s| s.state.id == id).ok_or(EngineError::NoSuchSlot(id))?;
        self.to_audio.try_send(AudioMsg::Remove(id)).map_err(|_| EngineError::Busy)?;
        let rec = self.slots.remove(idx);
        if rec.state.inputs > 0 {
            self.soft_inputs -= 1;
        }
        if rec.state.outputs > 0 {
            self.soft_outputs -= 1;
        }
        Ok(())
    }

    /// Housekeeping: publishes matrix changes and frees state returned by the audio side.
    pub fn tick(&mut self) {
        self.matrix.tick();
        while let Some(r) = self.returns.try_recv() {
            match r {
                Returned::Input(entry) => drop(entry),
                Returned::Output(entry) => drop(entry),
            }
        }
    }

    /// Executes one Control API command.
    pub fn handle(&mut self, cmd: &Command) -> Response {
        match *cmd {
            Command::SetPoint { input, output, gain_db, mute, invert } => {
                match self.matrix.set_point(input, output, PointParams { gain_db, mute, invert }) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::Error(EngineError::OutOfRange.to_string()),
                }
            }
            Command::RemovePoint { input, output } => match self.matrix.remove_point(input, output) {
                Ok(()) => Response::Ok,
                Err(_) => Response::Error(EngineError::OutOfRange.to_string()),
            },
            Command::ListPoints => Response::Points(
                self.matrix
                    .points()
                    .into_iter()
                    .map(|(input, output, p)| PointState {
                        input,
                        output,
                        gain_db: p.gain_db,
                        mute: p.mute,
                        invert: p.invert,
                    })
                    .collect(),
            ),
            Command::ListSlots => Response::Slots(self.slots.iter().map(|s| s.state.clone()).collect()),
            Command::Health => Response::Health {
                blocks: self.blocks(),
                slots: self
                    .slots
                    .iter()
                    .map(|s| {
                        let h = s.stats.snapshot();
                        SlotHealth {
                            id: s.state.id,
                            underruns: h.underruns,
                            overruns: h.overruns,
                            fill_frames: h.fill_frames,
                            target_frames: h.target_frames,
                            device_ppm: h.device_ppm,
                            correction_ppm: h.correction_ppm,
                        }
                    })
                    .collect(),
            },
            Command::Shutdown => Response::Ok,
        }
    }

    fn bridge_config(&self, spec: &SoftSlotSpec) -> BridgeConfig {
        BridgeConfig {
            channels: spec.channels,
            device_rate: spec.device_rate,
            device_block: spec.device_block,
            master_rate: self.cfg.sample_rate,
            master_block: self.cfg.block,
            quality: spec.quality,
            margin_frames: self.cfg.margin_frames,
        }
    }

    fn alloc(&self, channels: usize, input: bool) -> Result<u32, EngineError> {
        let (next, max, what) = if input {
            (self.next_input, self.cfg.max_inputs, "input")
        } else {
            (self.next_output, self.cfg.max_outputs, "output")
        };
        if channels == 0 || next as usize + channels > max {
            return Err(EngineError::ChannelsExhausted(what));
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, channels: usize) -> SoftSlotSpec {
        SoftSlotSpec {
            name: name.into(),
            channels,
            device_rate: 48_000.0,
            device_block: 128,
            quality: AsrcQuality::Sinc64,
        }
    }

    #[test]
    fn slots_get_contiguous_channel_ranges() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (a, _) = e.add_soft_input(&spec("a", 2)).unwrap();
        let (b, _) = e.add_soft_input(&spec("b", 8)).unwrap();
        let (c, _) = e.add_soft_output(&spec("c", 4)).unwrap();
        let Response::Slots(slots) = e.handle(&Command::ListSlots) else { panic!() };
        let get = |id| slots.iter().find(|s| s.id == id).unwrap().clone();
        assert_eq!((get(a).first_input, get(a).inputs), (0, 2));
        assert_eq!((get(b).first_input, get(b).inputs), (2, 8));
        assert_eq!((get(c).first_output, get(c).outputs), (0, 4));
    }

    #[test]
    fn channel_space_exhaustion_is_an_error() {
        let mut cfg = EngineConfig::new(48_000.0, 256);
        cfg.max_inputs = 4;
        let (mut e, _audio) = Engine::new(cfg);
        e.add_soft_input(&spec("a", 4)).unwrap();
        assert_eq!(e.add_soft_input(&spec("b", 1)).err(), Some(EngineError::ChannelsExhausted("input")));
    }

    #[test]
    fn points_round_trip_through_commands() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let set = Command::SetPoint { input: 3, output: 5, gain_db: -6.0, mute: false, invert: true };
        assert_eq!(e.handle(&set), Response::Ok);
        let Response::Points(p) = e.handle(&Command::ListPoints) else { panic!() };
        assert_eq!(p, vec![PointState { input: 3, output: 5, gain_db: -6.0, mute: false, invert: true }]);
        let bad = Command::SetPoint { input: 5000, output: 0, gain_db: 0.0, mute: false, invert: false };
        assert!(matches!(e.handle(&bad), Response::Error(_)));
    }

    #[test]
    fn removed_slot_state_comes_back_to_control_side() {
        let (mut e, mut audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (id, _dev) = e.add_soft_input(&spec("a", 2)).unwrap();
        audio.process_block(0.0);
        assert_eq!(audio.soft_inputs.len(), 1);
        e.remove_slot(id).unwrap();
        audio.process_block(0.005);
        assert!(audio.soft_inputs.is_empty());
        e.tick();
        assert!(e.returns.try_recv().is_none(), "returned entry was drained by tick");
        assert_eq!(e.remove_slot(id), Err(EngineError::NoSuchSlot(id)));
    }

    #[test]
    fn removed_input_slot_goes_silent_instead_of_looping_its_last_block() {
        let (mut e, mut audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (_, _dev_a) = e.add_soft_input(&spec("a", 2)).unwrap();
        let (b, _dev_b) = e.add_soft_input(&spec("b", 2)).unwrap();
        e.handle(&Command::SetPoint { input: 2, output: 0, gain_db: 0.0, mute: false, invert: false });
        e.tick();
        audio.process_block(0.0);
        // The last block slot `b` delivered before it was unplugged.
        audio.inputs.channel_mut(2).fill(0.5);
        audio.inputs.channel_mut(3).fill(0.5);
        e.remove_slot(b).unwrap();
        for n in 1..40 {
            audio.process_block(n as f64 * 0.005);
        }
        assert!(audio.inputs.channel(2).iter().chain(audio.inputs.channel(3)).all(|&s| s == 0.0));
        assert!(audio.outputs.channel(0).iter().all(|&s| s == 0.0), "no stale audio reaches outputs");
    }
}
