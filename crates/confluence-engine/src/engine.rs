//! The control half of the engine: slot registry, channel allocation, matrix
//! control and Control API command handling. Not real-time; call [`Engine::tick`]
//! every 10–20 ms from the control thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use confluence_api::{ClockRole, Command, PointState, Response, SlotHealth, SlotState};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, soft_output, BridgeConfig, BridgeStats, InputDeviceSide, OutputDeviceSide};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::mailbox::{self, Receiver, Sender};
use confluence_core::matrix::{matrix, MatrixController};

use crate::alloc::ChannelAllocator;
use crate::audio::{
    AudioEngine, AudioMsg, InputEntry, LoadMeter, OutputEntry, Returned, StrictEntry, StrictSide, MAX_SLOTS,
};

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub sample_rate: f64,
    pub block: usize,
    pub max_inputs: usize,
    pub max_outputs: usize,
    pub ramp: Duration,
    /// Safety margin for soft-slot rings, in frames (spec default 2 ms).
    pub margin_frames: usize,
}

impl EngineConfig {
    /// Spec defaults: 1024 × 1024 channels, 10 ms ramps, 2 ms margin.
    pub fn new(sample_rate: f64, block: usize) -> Self {
        Self {
            sample_rate,
            block,
            max_inputs: 1024,
            max_outputs: 1024,
            ramp: Duration::from_millis(10),
            margin_frames: (sample_rate * 0.002).round() as usize,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EngineError {
    #[error("not enough free {0} channels")]
    ChannelsExhausted(&'static str),
    #[error("{0} channels {1}..{2} are already in use")]
    ChannelsTaken(&'static str, u32, u32),
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
    #[error("the engine already has a master slot")]
    MasterExists,
    #[error("the master slot cannot be removed while the engine runs")]
    MasterInUse,
    #[error("slot {0} is not offline")]
    NotOffline(u32),
    #[error("{0}")]
    Device(String),
}

/// Parameters of a soft-clocked device slot.
#[derive(Clone, Debug)]
pub struct SoftSlotSpec {
    pub name: String,
    /// Device binding, e.g. `asio:GoXLR ASIO Driver`.
    pub device: String,
    pub channels: usize,
    pub device_rate: f64,
    pub device_block: usize,
    pub quality: AsrcQuality,
    /// Place the slot at this first channel (restoring a saved layout); `None` = first fit.
    pub first_channel: Option<u32>,
}

/// Parameters of the master slot: the device whose callback drives the engine.
#[derive(Clone, Debug)]
pub struct MasterSlotSpec {
    pub name: String,
    pub device: String,
    pub inputs: usize,
    pub outputs: usize,
    pub first_input: Option<u32>,
    pub first_output: Option<u32>,
}

/// Where a master slot's channels live in the engine's channel space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterChannels {
    pub first_input: usize,
    pub inputs: usize,
    pub first_output: usize,
    pub outputs: usize,
}

/// A slot whose device is missing: its channels stay reserved so every other
/// slot (and every route) keeps its channel numbers.
#[derive(Clone, Debug)]
pub struct OfflineSlotSpec {
    pub name: String,
    pub device: String,
    pub role: ClockRole,
    pub first_input: u32,
    pub inputs: u32,
    pub first_output: u32,
    pub outputs: u32,
}

/// Counters a strict slot's device exposes to the control side.
pub trait StrictStats: Send + Sync {
    /// (blocks the device delivered late, blocks it did not take).
    fn xruns(&self) -> (u64, u64);

    /// Whether the program this slot serves (e.g. a DAW) is attached, if that applies.
    fn attached(&self) -> Option<bool> {
        None
    }

    /// What to tell the user while nothing is attached (e.g. "no app playing").
    fn idle_note(&self) -> Option<&'static str> {
        None
    }
}

/// Parameters of a strict slot (a device on the engine's own clock).
#[derive(Clone, Debug)]
pub struct StrictSlotSpec {
    pub name: String,
    pub device: String,
    pub inputs: usize,
    pub outputs: usize,
    pub first_input: Option<u32>,
    pub first_output: Option<u32>,
}

enum SlotStats {
    None,
    Bridge(Arc<BridgeStats>),
    Master,
    Strict(Arc<dyn StrictStats>),
}

struct SlotRecord {
    state: SlotState,
    stats: SlotStats,
}

pub struct Engine {
    cfg: EngineConfig,
    matrix: MatrixController,
    to_audio: Sender<AudioMsg>,
    returns: Receiver<Returned>,
    slots: Vec<SlotRecord>,
    next_id: u32,
    inputs: ChannelAllocator,
    outputs: ChannelAllocator,
    soft_inputs: usize,
    soft_outputs: usize,
    strict: usize,
    blocks: Arc<AtomicU64>,
    master_ppm: Arc<AtomicU64>,
    dsp_load: Arc<AtomicU32>,
}

impl Engine {
    /// Builds a connected control/audio pair.
    pub fn new(cfg: EngineConfig) -> (Engine, AudioEngine) {
        let (matrix_ctl, router) = matrix(cfg.max_inputs, cfg.max_outputs, cfg.ramp, cfg.sample_rate as f32);
        let (to_audio, inbox) = mailbox::channel(4 * MAX_SLOTS);
        let (returns_tx, returns) = mailbox::channel(4 * MAX_SLOTS);
        let blocks = Arc::new(AtomicU64::new(0));
        let master_ppm = Arc::new(AtomicU64::new(0f64.to_bits()));
        let dsp_load = Arc::new(AtomicU32::new(0));
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
            strict: Vec::with_capacity(MAX_SLOTS),
            inbox,
            returns: returns_tx,
            blocks: blocks.clone(),
            sample_rate: cfg.sample_rate,
            master_est: None,
            master_ppm: master_ppm.clone(),
            load: LoadMeter::new(dsp_load.clone()),
        };
        let engine = Engine {
            cfg,
            matrix: matrix_ctl,
            to_audio,
            returns,
            slots: Vec::new(),
            next_id: 1,
            inputs: ChannelAllocator::new(cfg.max_inputs as u32),
            outputs: ChannelAllocator::new(cfg.max_outputs as u32),
            soft_inputs: 0,
            soft_outputs: 0,
            strict: 0,
            blocks,
            master_ppm,
            dsp_load,
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

    /// Smoothed fraction (0..=1) of the block period the audio thread spends processing.
    pub fn dsp_load(&self) -> f32 {
        f32::from_bits(self.dsp_load.load(Ordering::Relaxed))
    }

    /// The hardware master's measured deviation from nominal (0 on the internal clock).
    pub fn master_ppm(&self) -> f64 {
        f64::from_bits(self.master_ppm.load(Ordering::Relaxed))
    }

    /// Adds a soft-clocked capture slot. The returned device side belongs in the
    /// device's callback; its channels appear as matrix inputs.
    pub fn add_soft_input(&mut self, spec: &SoftSlotSpec) -> Result<(u32, InputDeviceSide), EngineError> {
        if self.soft_inputs >= MAX_SLOTS {
            return Err(EngineError::TooManySlots);
        }
        let first = claim(&mut self.inputs, spec.first_channel, spec.channels as u32, "input")?;
        let built = soft_input(self.bridge_config(spec));
        let (device, side, stats) = match built {
            Ok(parts) => parts,
            Err(e) => {
                self.inputs.free(first, spec.channels as u32);
                return Err(EngineError::Asrc(e.to_string()));
            }
        };
        let id = self.next_id;
        let entry = Box::new(InputEntry { id, first_channel: first as usize, channels: spec.channels, side });
        if self.to_audio.try_send(AudioMsg::AddInput(entry)).is_err() {
            self.inputs.free(first, spec.channels as u32);
            return Err(EngineError::Busy);
        }
        self.next_id += 1;
        self.soft_inputs += 1;
        let state = self.state(id, &spec.name, &spec.device, ClockRole::Soft, (first, spec.channels as u32), (0, 0));
        self.slots.push(SlotRecord { state, stats: SlotStats::Bridge(stats) });
        Ok((id, device))
    }

    /// Adds a soft-clocked playback slot; its channels appear as matrix outputs.
    pub fn add_soft_output(&mut self, spec: &SoftSlotSpec) -> Result<(u32, OutputDeviceSide), EngineError> {
        if self.soft_outputs >= MAX_SLOTS {
            return Err(EngineError::TooManySlots);
        }
        let first = claim(&mut self.outputs, spec.first_channel, spec.channels as u32, "output")?;
        let built = soft_output(self.bridge_config(spec));
        let (side, device, stats) = match built {
            Ok(parts) => parts,
            Err(e) => {
                self.outputs.free(first, spec.channels as u32);
                return Err(EngineError::Asrc(e.to_string()));
            }
        };
        let id = self.next_id;
        let entry = Box::new(OutputEntry { id, first_channel: first as usize, side });
        if self.to_audio.try_send(AudioMsg::AddOutput(entry)).is_err() {
            self.outputs.free(first, spec.channels as u32);
            return Err(EngineError::Busy);
        }
        self.next_id += 1;
        self.soft_outputs += 1;
        let state = self.state(id, &spec.name, &spec.device, ClockRole::Soft, (0, 0), (first, spec.channels as u32));
        self.slots.push(SlotRecord { state, stats: SlotStats::Bridge(stats) });
        Ok((id, device))
    }

    /// Registers the master slot: the device whose callback will copy its inputs
    /// into, and its outputs out of, the returned channel ranges around
    /// `AudioEngine::process_master_block`.
    pub fn add_master_slot(&mut self, spec: &MasterSlotSpec) -> Result<(u32, MasterChannels), EngineError> {
        if self.slots.iter().any(|s| s.state.role == ClockRole::Master) {
            return Err(EngineError::MasterExists);
        }
        let first_input = claim_maybe(&mut self.inputs, spec.first_input, spec.inputs as u32, "input")?;
        let first_output = match claim_maybe(&mut self.outputs, spec.first_output, spec.outputs as u32, "output") {
            Ok(f) => f,
            Err(e) => {
                self.inputs.free(first_input, spec.inputs as u32);
                return Err(e);
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let state = self.state(
            id,
            &spec.name,
            &spec.device,
            ClockRole::Master,
            (first_input, spec.inputs as u32),
            (first_output, spec.outputs as u32),
        );
        self.slots.push(SlotRecord { state, stats: SlotStats::Master });
        let ch = MasterChannels {
            first_input: first_input as usize,
            inputs: spec.inputs,
            first_output: first_output as usize,
            outputs: spec.outputs,
        };
        Ok((id, ch))
    }

    /// Adds a strict slot. `make` receives the channels it was given and
    /// builds the audio-thread side for them.
    pub fn add_strict_slot<F>(&mut self, spec: &StrictSlotSpec, make: F) -> Result<(u32, MasterChannels), EngineError>
    where
        F: FnOnce(MasterChannels) -> Result<(Box<dyn StrictSide>, Arc<dyn StrictStats>), String>,
    {
        if self.strict >= MAX_SLOTS {
            return Err(EngineError::TooManySlots);
        }
        let first_input = claim_maybe(&mut self.inputs, spec.first_input, spec.inputs as u32, "input")?;
        let first_output = match claim_maybe(&mut self.outputs, spec.first_output, spec.outputs as u32, "output") {
            Ok(f) => f,
            Err(e) => {
                self.inputs.free(first_input, spec.inputs as u32);
                return Err(e);
            }
        };
        let release = |e: &mut Engine| {
            e.inputs.free(first_input, spec.inputs as u32);
            e.outputs.free(first_output, spec.outputs as u32);
        };
        let ch = MasterChannels {
            first_input: first_input as usize,
            inputs: spec.inputs,
            first_output: first_output as usize,
            outputs: spec.outputs,
        };
        let (side, stats) = match make(ch) {
            Ok(parts) => parts,
            Err(e) => {
                release(self);
                return Err(EngineError::Device(e));
            }
        };
        let id = self.next_id;
        let entry = Box::new(StrictEntry { id, first_input: ch.first_input, inputs: ch.inputs, side });
        if self.to_audio.try_send(AudioMsg::AddStrict(entry)).is_err() {
            release(self);
            return Err(EngineError::Busy);
        }
        self.next_id += 1;
        self.strict += 1;
        let state = self.state(
            id,
            &spec.name,
            &spec.device,
            ClockRole::Strict,
            (first_input, spec.inputs as u32),
            (first_output, spec.outputs as u32),
        );
        self.slots.push(SlotRecord { state, stats: SlotStats::Strict(stats) });
        Ok((id, ch))
    }

    /// Reserves the channels of a slot whose device is currently missing.
    pub fn add_offline_slot(&mut self, spec: &OfflineSlotSpec) -> Result<u32, EngineError> {
        let first_input = claim_maybe(&mut self.inputs, Some(spec.first_input), spec.inputs, "input")?;
        if let Err(e) = claim_maybe(&mut self.outputs, Some(spec.first_output), spec.outputs, "output") {
            self.inputs.free(first_input, spec.inputs);
            return Err(e);
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut state = self.state(
            id,
            &spec.name,
            &spec.device,
            spec.role,
            (spec.first_input, spec.inputs),
            (spec.first_output, spec.outputs),
        );
        state.online = false;
        self.slots.push(SlotRecord { state, stats: SlotStats::None });
        Ok(id)
    }

    /// Frees an offline slot's channels without touching the routes on them,
    /// so the device can come back online on the same channels (routes intact).
    pub fn release_offline_slot(&mut self, id: u32) -> Result<(), EngineError> {
        let idx = self.slots.iter().position(|s| s.state.id == id).ok_or(EngineError::NoSuchSlot(id))?;
        if self.slots[idx].state.online {
            return Err(EngineError::NotOffline(id));
        }
        let s = self.slots.remove(idx).state;
        self.inputs.free(s.first_input, s.inputs);
        self.outputs.free(s.first_output, s.outputs);
        Ok(())
    }

    /// Detaches a slot: its channels go silent, routes touching them fade out
    /// and are removed, and the channels become free for reuse.
    pub fn remove_slot(&mut self, id: u32) -> Result<(), EngineError> {
        self.detach(id, true)
    }

    /// Detaches a slot but keeps the routes on its channels (a device that
    /// failed to come back hands its channels, routes and all, back to its
    /// offline slot).
    pub fn detach_slot(&mut self, id: u32) -> Result<(), EngineError> {
        self.detach(id, false)
    }

    fn detach(&mut self, id: u32, drop_routes: bool) -> Result<(), EngineError> {
        let idx = self.slots.iter().position(|s| s.state.id == id).ok_or(EngineError::NoSuchSlot(id))?;
        let rec = &self.slots[idx];
        if rec.state.role == ClockRole::Master && rec.state.online {
            return Err(EngineError::MasterInUse);
        }
        if matches!(rec.stats, SlotStats::Bridge(_)) {
            self.to_audio.try_send(AudioMsg::Remove(id)).map_err(|_| EngineError::Busy)?;
            if rec.state.inputs > 0 {
                self.soft_inputs -= 1;
            }
            if rec.state.outputs > 0 {
                self.soft_outputs -= 1;
            }
        }
        if matches!(rec.stats, SlotStats::Strict(_)) {
            self.to_audio.try_send(AudioMsg::Remove(id)).map_err(|_| EngineError::Busy)?;
            self.strict -= 1;
        }
        let rec = self.slots.remove(idx);
        let s = &rec.state;
        let ins = s.first_input..s.first_input + s.inputs;
        let outs = s.first_output..s.first_output + s.outputs;
        for (input, output, _) in self.matrix.points() {
            if drop_routes && (ins.contains(&input) || outs.contains(&output)) {
                // In range by construction; removal cannot fail.
                let _ = self.matrix.remove_point(input, output);
            }
        }
        self.inputs.free(s.first_input, s.inputs);
        self.outputs.free(s.first_output, s.outputs);
        Ok(())
    }

    /// Housekeeping: publishes matrix changes and frees state returned by the audio side.
    pub fn tick(&mut self) {
        self.matrix.tick();
        while let Some(r) = self.returns.try_recv() {
            match r {
                Returned::Input(entry) => drop(entry),
                Returned::Output(entry) => drop(entry),
                Returned::Strict(entry) => drop(entry),
            }
        }
    }

    /// Current slot list (same data as `Command::ListSlots`).
    pub fn slots(&self) -> Vec<SlotState> {
        self.slots.iter().map(|s| s.state.clone()).collect()
    }

    /// Executes one Control API command. Device commands (`ListDevices`,
    /// `AddDevice`, `RemoveSlot` of a device) are handled by the process that
    /// owns the device providers; here `RemoveSlot` only detaches the slot.
    pub fn handle(&mut self, cmd: &Command) -> Response {
        match *cmd {
            Command::SetPoint { gain_db, .. } if !gain_db.is_finite() => {
                Response::Error(format!("gain must be a number of dB, not {gain_db}"))
            }
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
            Command::ListSlots => Response::Slots(self.slots()),
            Command::Health => Response::Health {
                blocks: self.blocks(),
                slots: self.slots.iter().filter_map(|s| self.health(s)).collect(),
                notices: Vec::new(),
            },
            Command::RemoveSlot { id } => match self.remove_slot(id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::ListDevices | Command::AddDevice { .. } => {
                Response::Error("device commands are handled by the engine process".into())
            }
            Command::Subscribe | Command::Status => {
                Response::Error("subscriptions and status are served by the engine process".into())
            }
            Command::Shutdown => Response::Ok,
        }
    }

    fn health(&self, s: &SlotRecord) -> Option<SlotHealth> {
        let id = s.state.id;
        match &s.stats {
            SlotStats::Bridge(stats) => {
                let h = stats.snapshot();
                Some(SlotHealth {
                    id,
                    underruns: h.underruns,
                    overruns: h.overruns,
                    fill_frames: h.fill_frames,
                    target_frames: h.target_frames,
                    device_ppm: h.device_ppm,
                    correction_ppm: h.correction_ppm,
                    device_lost: false,
                    device_faults: 0,
                    driver_requests: 0,
                    attached: None,
                    idle_note: None,
                })
            }
            SlotStats::Master => Some(SlotHealth {
                id,
                underruns: 0,
                overruns: 0,
                fill_frames: 0.0,
                target_frames: 0.0,
                device_ppm: self.master_ppm(),
                correction_ppm: 0.0,
                device_lost: false,
                device_faults: 0,
                driver_requests: 0,
                attached: None,
                idle_note: None,
            }),
            SlotStats::Strict(stats) => {
                let (underruns, overruns) = stats.xruns();
                let attached = stats.attached();
                Some(SlotHealth {
                    id,
                    underruns,
                    overruns,
                    fill_frames: 0.0,
                    target_frames: 0.0,
                    device_ppm: 0.0,
                    correction_ppm: 0.0,
                    device_lost: false,
                    device_faults: 0,
                    driver_requests: 0,
                    attached,
                    idle_note: (attached == Some(false)).then(|| stats.idle_note()).flatten().map(String::from),
                })
            }
            SlotStats::None => None,
        }
    }

    fn state(
        &self,
        id: u32,
        name: &str,
        device: &str,
        role: ClockRole,
        (first_input, inputs): (u32, u32),
        (first_output, outputs): (u32, u32),
    ) -> SlotState {
        SlotState {
            id,
            name: name.to_string(),
            device: device.to_string(),
            role,
            online: true,
            first_input,
            inputs,
            first_output,
            outputs,
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
}

/// Claims `len > 0` channels, at `at` if given, else first fit.
fn claim(a: &mut ChannelAllocator, at: Option<u32>, len: u32, what: &'static str) -> Result<u32, EngineError> {
    if len == 0 {
        return Err(EngineError::ChannelsExhausted(what));
    }
    match at {
        Some(start) if a.reserve(start, len) => Ok(start),
        Some(start) => Err(EngineError::ChannelsTaken(what, start, start + len)),
        None => a.alloc(len).ok_or(EngineError::ChannelsExhausted(what)),
    }
}

/// As [`claim`], but a zero-length request succeeds and reserves nothing.
fn claim_maybe(a: &mut ChannelAllocator, at: Option<u32>, len: u32, what: &'static str) -> Result<u32, EngineError> {
    if len == 0 {
        Ok(at.unwrap_or(0))
    } else {
        claim(a, at, len, what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NaN gain never equals itself: it would look changed in every state
    /// diff (a new version every 100 ms) and come back from the journal.
    #[test]
    fn a_gain_that_is_not_a_number_is_refused() {
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        for gain_db in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cmd = Command::SetPoint { input: 0, output: 0, gain_db, mute: false, invert: false };
            assert!(matches!(engine.handle(&cmd), Response::Error(_)), "{gain_db}");
        }
        assert_eq!(engine.handle(&Command::ListPoints), Response::Points(Vec::new()));
    }

    fn spec(name: &str, channels: usize) -> SoftSlotSpec {
        SoftSlotSpec {
            name: name.into(),
            device: String::new(),
            channels,
            device_rate: 48_000.0,
            device_block: 128,
            quality: AsrcQuality::Sinc64,
            first_channel: None,
        }
    }

    fn master(inputs: usize, outputs: usize) -> MasterSlotSpec {
        MasterSlotSpec {
            name: "master".into(),
            device: "asio:test".into(),
            inputs,
            outputs,
            first_input: None,
            first_output: None,
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

    /// A strict device that plays `level` into the engine and records the last
    /// sample the engine sent it; `dropped` flips when the engine lets go of it.
    struct Probe {
        ch: MasterChannels,
        level: f32,
        heard: Arc<std::sync::Mutex<f32>>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl StrictSide for Probe {
        fn receive(&mut self, inputs: &mut PlanarBuffer) {
            for c in 0..self.ch.inputs {
                inputs.channel_mut(self.ch.first_input + c).fill(self.level);
            }
        }
        fn send(&mut self, outputs: &PlanarBuffer) {
            *self.heard.lock().unwrap() = outputs.channel(self.ch.first_output)[0];
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct NoXruns;
    impl StrictStats for NoXruns {
        fn xruns(&self) -> (u64, u64) {
            (0, 0)
        }
    }

    #[test]
    fn a_strict_slot_exchanges_a_block_each_way_and_is_released_on_removal() {
        let (mut e, mut audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (_, _dev) = e.add_soft_input(&spec("a", 2)).unwrap();
        let heard = Arc::new(std::sync::Mutex::new(0.0));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spec = StrictSlotSpec {
            name: "VASIO 1".into(),
            device: "vasio:1".into(),
            inputs: 2,
            outputs: 2,
            first_input: None,
            first_output: None,
        };
        let (h, d) = (heard.clone(), dropped.clone());
        let (id, ch) = e
            .add_strict_slot(&spec, move |ch| {
                let side = Probe { ch, level: 0.5, heard: h, dropped: d };
                Ok((Box::new(side) as Box<dyn StrictSide>, Arc::new(NoXruns) as Arc<dyn StrictStats>))
            })
            .unwrap();
        assert_eq!((ch.first_input, ch.first_output), (2, 0), "strict slots share the channel space");
        let slot = e.slots().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(slot.role, ClockRole::Strict);
        // Its own input 1 routed to its own output 1: a loop through the matrix.
        e.handle(&Command::SetPoint { input: 2, output: 0, gain_db: 0.0, mute: false, invert: false });
        e.tick();
        for n in 0..10 {
            audio.process_block(n as f64 * 0.005);
        }
        assert_eq!(*heard.lock().unwrap(), 0.5, "what it played came back through the matrix");
        let Response::Health { slots, .. } = e.handle(&Command::Health) else { panic!() };
        assert!(slots.iter().any(|h| h.id == id));
        e.remove_slot(id).unwrap();
        audio.process_block(1.0);
        assert!(audio.inputs.channel(2).iter().all(|&s| s == 0.0), "its inputs go silent");
        e.tick();
        assert!(dropped.load(Ordering::Acquire), "released on the control side");
        let err = e.add_strict_slot(&StrictSlotSpec { name: "bad".into(), ..spec.clone() }, |_| Err("no".into()));
        assert_eq!(err.err(), Some(EngineError::Device("no".into())));
        assert_eq!(e.slots().len(), 1, "a failed strict slot leaves nothing behind");
    }

    #[test]
    fn removing_a_slot_frees_its_channels_and_its_routes() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (a, _) = e.add_soft_input(&spec("a", 2)).unwrap();
        let (_b, _) = e.add_soft_input(&spec("b", 2)).unwrap();
        e.handle(&Command::SetPoint { input: 1, output: 0, gain_db: 0.0, mute: false, invert: false });
        e.handle(&Command::SetPoint { input: 2, output: 0, gain_db: 0.0, mute: false, invert: false });
        e.remove_slot(a).unwrap();
        let Response::Points(p) = e.handle(&Command::ListPoints) else { panic!() };
        assert_eq!(p.len(), 1, "only the route on the removed slot's channels is gone: {p:?}");
        assert_eq!(p[0].input, 2);
        let (c, _) = e.add_soft_input(&spec("c", 2)).unwrap();
        let slot = e.slots().into_iter().find(|s| s.id == c).unwrap();
        assert_eq!(slot.first_input, 0, "freed channels are reused");
    }

    #[test]
    fn fixed_placement_restores_a_layout_and_rejects_clashes() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let mut s = spec("late", 2);
        s.first_channel = Some(10);
        let (id, _) = e.add_soft_input(&s).unwrap();
        assert_eq!(e.slots().into_iter().find(|x| x.id == id).unwrap().first_input, 10);
        let mut clash = spec("clash", 4);
        clash.first_channel = Some(8);
        assert_eq!(e.add_soft_input(&clash).err(), Some(EngineError::ChannelsTaken("input", 8, 12)));
        assert_eq!(e.slots().len(), 1, "a failed add leaves nothing behind");
    }

    #[test]
    fn offline_slots_hold_their_channels_and_report_offline() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let off = OfflineSlotSpec {
            name: "unplugged".into(),
            device: "asio:gone".into(),
            role: ClockRole::Soft,
            first_input: 0,
            inputs: 4,
            first_output: 0,
            outputs: 0,
        };
        let id = e.add_offline_slot(&off).unwrap();
        let (b, _) = e.add_soft_input(&spec("b", 2)).unwrap();
        let slots = e.slots();
        assert!(!slots.iter().find(|s| s.id == id).unwrap().online);
        assert_eq!(slots.iter().find(|s| s.id == b).unwrap().first_input, 4, "reserved range skipped");
        let Response::Health { slots: health, .. } = e.handle(&Command::Health) else { panic!() };
        assert!(health.iter().all(|h| h.id != id), "offline slots have no stream health");
        assert_eq!(e.handle(&Command::RemoveSlot { id }), Response::Ok);
    }

    #[test]
    fn one_master_slot_whose_channels_come_from_the_shared_space() {
        let (mut e, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (_, _) = e.add_soft_input(&spec("a", 2)).unwrap();
        let (m, ch) = e.add_master_slot(&master(8, 6)).unwrap();
        assert_eq!(ch, MasterChannels { first_input: 2, inputs: 8, first_output: 0, outputs: 6 });
        assert_eq!(e.add_master_slot(&master(2, 2)).err(), Some(EngineError::MasterExists));
        assert_eq!(e.remove_slot(m), Err(EngineError::MasterInUse));
        let slot = e.slots().into_iter().find(|s| s.id == m).unwrap();
        assert_eq!(slot.role, ClockRole::Master);
    }
}
