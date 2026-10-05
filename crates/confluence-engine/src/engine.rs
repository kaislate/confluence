//! The control half of the engine: slot registry, channel allocation, matrix
//! control and Control API command handling. Not real-time; call [`Engine::tick`]
//! every 10–20 ms from the control thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use confluence_api::{ClockRole, Command, PointState, Response, SlotHealth, SlotState, BUS_DEVICE};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{soft_input, soft_output, BridgeConfig, BridgeStats, InputDeviceSide, OutputDeviceSide};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::mailbox::{self, Receiver, Sender};
use confluence_core::matrix::{matrix, MatrixController};
use confluence_core::plan::{compile, order, plan, BusSpan, PlanController};
use confluence_core::processor::Processor;

use crate::alloc::ChannelAllocator;
use crate::audio::{
    AudioEngine, AudioMsg, BusEntry, InputEntry, LoadMeter, OutputEntry, Returned, StrictEntry, StrictSide, MAX_BUSES,
    MAX_SLOTS,
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
    #[error("this route would feed an insert bus back into itself")]
    BusLoop,
    #[error("an insert bus has 1 to 64 channels")]
    BusChannels,
    #[error("too many insert buses")]
    TooManyBuses,
    #[error("slot {0} is not an insert bus")]
    NotABus(u32),
}

/// Most channels an insert bus can have.
pub const MAX_BUS_CHANNELS: u32 = 64;

/// Parameters of an insert bus.
#[derive(Clone, Debug)]
pub struct BusSpec {
    pub name: String,
    pub channels: u32,
    /// Place the returns at this first input channel (restoring a saved layout); `None` = first fit.
    pub first_input: Option<u32>,
    /// Place the sends at this first output channel; `None` = first fit.
    pub first_output: Option<u32>,
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
    /// An insert bus: its processor's caught panics.
    Bus(Arc<AtomicU64>),
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
    buses: usize,
    plan: PlanController,
    /// Buses or the routes between them changed: compile a new plan on `tick`.
    plan_dirty: bool,
    /// Processors the audio side gave back, for their owner to dispose of.
    returned_processors: Vec<Box<dyn Processor>>,
    blocks: Arc<AtomicU64>,
    master_ppm: Arc<AtomicU64>,
    dsp_load: Arc<AtomicU32>,
}

impl Engine {
    /// Builds a connected control/audio pair.
    pub fn new(cfg: EngineConfig) -> (Engine, AudioEngine) {
        let (matrix_ctl, router) = matrix(cfg.max_inputs, cfg.max_outputs, cfg.ramp, cfg.sample_rate as f32);
        let (plan_ctl, plan_run) = plan(cfg.max_outputs as u32);
        let (to_audio, inbox) = mailbox::channel(4 * (MAX_SLOTS + MAX_BUSES));
        let (returns_tx, returns) = mailbox::channel(4 * (MAX_SLOTS + MAX_BUSES));
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
            buses: Vec::with_capacity(MAX_BUSES),
            plan: plan_run,
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
            buses: 0,
            plan: plan_ctl,
            plan_dirty: false,
            returned_processors: Vec::new(),
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
    /// `AudioEngine::process_master_block`. A saved placement that is now
    /// taken (by a bus, or because the driver reports more channels) gives way
    /// to first fit: the engine must start.
    pub fn add_master_slot(&mut self, spec: &MasterSlotSpec) -> Result<(u32, MasterChannels), EngineError> {
        if self.slots.iter().any(|s| s.state.role == ClockRole::Master) {
            return Err(EngineError::MasterExists);
        }
        let first_input = claim_or_fit(&mut self.inputs, spec.first_input, spec.inputs as u32, "input")?;
        let first_output = match claim_or_fit(&mut self.outputs, spec.first_output, spec.outputs as u32, "output") {
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

    /// Adds a summing insert bus (its returns carry what its sends receive).
    pub fn add_bus(&mut self, spec: &BusSpec) -> Result<u32, EngineError> {
        self.add_bus_with(spec, None)
    }

    /// Adds an insert bus running `processor` (`None`: a summing bus). Its
    /// sends are output columns and its returns input rows, `spec.channels`
    /// each; returns reach outputs in the same block.
    pub fn add_bus_with(&mut self, spec: &BusSpec, processor: Option<Box<dyn Processor>>) -> Result<u32, EngineError> {
        if !(1..=MAX_BUS_CHANNELS).contains(&spec.channels) {
            return Err(EngineError::BusChannels);
        }
        if self.buses >= MAX_BUSES {
            return Err(EngineError::TooManyBuses);
        }
        let ch = spec.channels;
        let first_input = claim(&mut self.inputs, spec.first_input, ch, "input")?;
        let first_output = match claim(&mut self.outputs, spec.first_output, ch, "output") {
            Ok(f) => f,
            Err(e) => {
                self.inputs.free(first_input, ch);
                return Err(e);
            }
        };
        let faults = Arc::new(AtomicU64::new(0));
        let id = self.next_id;
        let entry = Box::new(BusEntry {
            id,
            first_send: first_output as usize,
            first_return: first_input as usize,
            channels: ch as usize,
            processor,
            faults: faults.clone(),
            faulted: false,
            silent: false,
        });
        if self.to_audio.try_send(AudioMsg::AddBus(entry)).is_err() {
            self.inputs.free(first_input, ch);
            self.outputs.free(first_output, ch);
            return Err(EngineError::Busy);
        }
        // Routes left on these (free) channels would otherwise become the
        // bus's routes, possibly a loop: a new bus starts unrouted.
        let (ins, outs) = (first_input..first_input + ch, first_output..first_output + ch);
        for (input, output, _) in self.matrix.points() {
            if ins.contains(&input) || outs.contains(&output) {
                // In range by construction; removal cannot fail.
                let _ = self.matrix.remove_point(input, output);
            }
        }
        self.next_id += 1;
        self.buses += 1;
        self.plan_dirty = true;
        let state = self.state(id, &spec.name, BUS_DEVICE, ClockRole::Strict, (first_input, ch), (first_output, ch));
        self.slots.push(SlotRecord { state, stats: SlotStats::Bus(faults) });
        Ok(id)
    }

    /// Replaces a bus's processor (`None`: a summing bus). The old one is
    /// stopped on the audio side and comes back through
    /// [`take_returned_processors`](Self::take_returned_processors). Clears a fault.
    pub fn set_bus_processor(&mut self, bus: u32, processor: Option<Box<dyn Processor>>) -> Result<(), EngineError> {
        self.check_bus(bus)?;
        self.to_audio.try_send(AudioMsg::SetProcessor { bus, processor }).map_err(|_| EngineError::Busy)
    }

    /// A silent bus passes nothing (a plugin that could not be loaded must not
    /// send the dry signal on).
    pub fn set_bus_silent(&mut self, bus: u32, silent: bool) -> Result<(), EngineError> {
        self.check_bus(bus)?;
        self.to_audio.try_send(AudioMsg::SetSilent { bus, silent }).map_err(|_| EngineError::Busy)
    }

    /// Processors the audio side has given back since the last call (replaced,
    /// or from removed buses). Call after `tick`.
    pub fn take_returned_processors(&mut self) -> Vec<Box<dyn Processor>> {
        std::mem::take(&mut self.returned_processors)
    }

    fn check_bus(&self, bus: u32) -> Result<(), EngineError> {
        match self.slots.iter().find(|s| s.state.id == bus) {
            Some(s) if matches!(s.stats, SlotStats::Bus(_)) => Ok(()),
            Some(_) => Err(EngineError::NotABus(bus)),
            None => Err(EngineError::NoSuchSlot(bus)),
        }
    }

    fn bus_spans(&self) -> Vec<BusSpan> {
        self.slots
            .iter()
            .filter(|s| matches!(s.stats, SlotStats::Bus(_)))
            .map(|s| BusSpan {
                id: s.state.id,
                sends: s.state.first_output..s.state.first_output + s.state.outputs,
                returns: s.state.first_input..s.state.first_input + s.state.inputs,
            })
            .collect()
    }

    /// Adds or updates a point; a new point that would loop a bus is refused.
    fn set_point(&mut self, input: u32, output: u32, p: PointParams) -> Result<(), EngineError> {
        let new = self.matrix.point(input, output).is_none();
        if new && self.buses > 0 {
            let spans = self.bus_spans();
            let points = self.matrix.points().into_iter().map(|(i, o, _)| (i, o)).chain([(input, output)]);
            if order(&spans, points).is_err() {
                return Err(EngineError::BusLoop);
            }
        }
        self.matrix.set_point(input, output, p).map_err(|_| EngineError::OutOfRange)?;
        if new && self.buses > 0 {
            self.plan_dirty = true;
        }
        Ok(())
    }

    /// Compiles the plan for the current buses and routes.
    fn replan(&mut self) {
        let spans = self.bus_spans();
        let points = self.matrix.points().into_iter().map(|(i, o, _)| (i, o));
        // `set_point` never lets a loop in; if one appeared anyway, run the
        // buses by id: one stale block somewhere, never a hang.
        let ids = order(&spans, points).unwrap_or_else(|_| spans.iter().map(|b| b.id).collect());
        self.plan.set(compile(&spans, &ids, self.cfg.max_outputs as u32));
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
        if matches!(rec.stats, SlotStats::Bus(_)) {
            self.to_audio.try_send(AudioMsg::Remove(id)).map_err(|_| EngineError::Busy)?;
            self.buses -= 1;
            self.plan_dirty = true;
        }
        // Routes on the slot's channels go: edges between buses may change.
        self.plan_dirty |= self.buses > 0;
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
        // The plan goes first, so a new bus route's plan and routing snapshot
        // normally reach the audio thread at the same block start.
        if self.plan_dirty {
            self.replan();
            self.plan_dirty = false;
        }
        self.plan.tick();
        self.matrix.tick();
        while let Some(r) = self.returns.try_recv() {
            match r {
                Returned::Input(entry) => drop(entry),
                Returned::Output(entry) => drop(entry),
                Returned::Strict(entry) => drop(entry),
                Returned::Bus(mut entry) => {
                    if let Some(p) = entry.processor.take() {
                        self.returned_processors.push(p);
                    }
                }
                Returned::Processor(p) => self.returned_processors.push(p),
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
                match self.set_point(input, output, PointParams { gain_db, mute, invert }) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::RemovePoint { input, output } => match self.matrix.remove_point(input, output) {
                Ok(()) => {
                    self.plan_dirty |= self.buses > 0;
                    Response::Ok
                }
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
            Command::AddBus { ref name, channels, first_input, first_output } => {
                let spec = BusSpec { name: name.clone(), channels, first_input, first_output };
                match self.add_bus(&spec) {
                    Ok(id) => Response::SlotsAdded(vec![id]),
                    Err(e) => Response::Error(e.to_string()),
                }
            }
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
            SlotStats::Bus(faults) => Some(SlotHealth {
                id,
                underruns: 0,
                overruns: 0,
                fill_frames: 0.0,
                target_frames: 0.0,
                device_ppm: 0.0,
                correction_ppm: 0.0,
                device_lost: false,
                device_faults: faults.load(Ordering::Relaxed),
                driver_requests: 0,
                attached: None,
                idle_note: None,
            }),
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

/// As [`claim_maybe`], but a placement that is taken falls back to first fit.
fn claim_or_fit(a: &mut ChannelAllocator, at: Option<u32>, len: u32, what: &'static str) -> Result<u32, EngineError> {
    match claim_maybe(a, at, len, what) {
        Err(EngineError::ChannelsTaken(..)) => claim_maybe(a, None, len, what),
        other => other,
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
    use confluence_core::processor::{BusIo, ProcessError, Processor};

    fn small() -> (Engine, AudioEngine) {
        let mut cfg = EngineConfig::new(48_000.0, 64);
        cfg.max_inputs = 16;
        cfg.max_outputs = 16;
        Engine::new(cfg)
    }

    fn bus(name: &str, channels: u32) -> BusSpec {
        BusSpec { name: name.into(), channels, first_input: None, first_output: None }
    }

    fn bus_at(name: &str, channels: u32, at: u32) -> BusSpec {
        BusSpec { name: name.into(), channels, first_input: Some(at), first_output: Some(at) }
    }

    fn route(e: &mut Engine, input: u32, output: u32) -> Response {
        e.handle(&Command::SetPoint { input, output, gain_db: 0.0, mute: false, invert: false })
    }

    /// Runs one block with `x` on every frame of input 0; returns output `o`'s first sample.
    fn block(e: &mut Engine, a: &mut AudioEngine, x: f32, o: usize) -> f32 {
        e.tick();
        a.inputs_mut().channel_mut(0).fill(x);
        a.process_block(0.0);
        a.outputs().channel(o)[0]
    }

    /// The journal replays buses before the ASIO master is placed. A master
    /// whose saved placement now overlaps a bus (its driver reports more
    /// channels than last time) must still start, elsewhere, not fail the
    /// engine on every restart.
    #[test]
    fn a_master_whose_saved_place_is_taken_by_a_bus_still_starts() {
        let (mut e, _a) = Engine::new(EngineConfig::new(48_000.0, 64));
        e.add_bus(&bus_at("Verb", 2, 8)).unwrap();
        let spec = MasterSlotSpec { first_input: Some(0), first_output: Some(0), ..master(10, 10) };
        let (_, ch) = e.add_master_slot(&spec).expect("the master starts");
        assert_eq!((ch.inputs, ch.outputs), (10, 10));
        let overlaps = |first: usize, n: usize| first < 10 && 8 < first + n;
        assert!(!overlaps(ch.first_input, ch.inputs) && !overlaps(ch.first_output, ch.outputs), "{ch:?}");
        // A placement that fits is still honoured.
        let (mut e, _a) = small();
        let spec = MasterSlotSpec { first_input: Some(2), first_output: Some(3), ..master(2, 2) };
        let (_, ch) = e.add_master_slot(&spec).unwrap();
        assert_eq!((ch.first_input, ch.first_output), (2, 3));
    }

    #[test]
    fn a_bus_return_reaches_an_output_in_the_same_block() {
        let (mut e, mut a) = small();
        let b = e.add_bus(&bus_at("Verb", 2, 8)).unwrap();
        assert_eq!(route(&mut e, 0, 8), Response::Ok, "input 0 to send 1");
        assert_eq!(route(&mut e, 8, 3), Response::Ok, "return 1 to output 3");
        // Settle the 10 ms fade-ins with silence, then a step must arrive at once.
        for _ in 0..20 {
            block(&mut e, &mut a, 0.0, 3);
        }
        assert_eq!(block(&mut e, &mut a, 1.0, 3), 1.0, "same block, no added latency");
        let s = e.slots().into_iter().find(|s| s.id == b).unwrap();
        assert!(s.is_bus());
        assert_eq!((s.first_input, s.inputs, s.first_output, s.outputs), (8, 2, 8, 2));
    }

    #[test]
    fn chained_buses_run_in_dependency_order() {
        let (mut e, mut a) = small();
        // B is created first (lower id) but is fed by A.
        e.add_bus(&bus_at("B", 1, 4)).unwrap();
        e.add_bus(&bus_at("A", 1, 6)).unwrap();
        assert_eq!(route(&mut e, 0, 6), Response::Ok, "input to A send");
        assert_eq!(route(&mut e, 6, 4), Response::Ok, "A return to B send");
        assert_eq!(route(&mut e, 4, 2), Response::Ok, "B return to output 2");
        for _ in 0..20 {
            block(&mut e, &mut a, 0.0, 2);
        }
        assert_eq!(block(&mut e, &mut a, 1.0, 2), 1.0);
    }

    #[test]
    fn a_route_that_loops_a_bus_is_refused_and_changes_nothing() {
        let (mut e, _a) = small();
        e.add_bus(&bus_at("A", 1, 4)).unwrap();
        e.add_bus(&bus_at("B", 1, 6)).unwrap();
        let err = Response::Error("this route would feed an insert bus back into itself".into());
        assert_eq!(route(&mut e, 4, 4), err, "into itself");
        assert_eq!(route(&mut e, 4, 6), Response::Ok, "A to B");
        assert_eq!(route(&mut e, 6, 4), err, "B to A closes the loop");
        let Response::Points(p) = e.handle(&Command::ListPoints) else { panic!() };
        assert_eq!(p.len(), 1);
        // Changing the existing route's gain is not a new edge.
        let gain = Command::SetPoint { input: 4, output: 6, gain_db: -6.0, mute: false, invert: false };
        assert_eq!(e.handle(&gain), Response::Ok);
    }

    #[test]
    fn reversing_a_bus_chain_is_accepted_at_once() {
        let (mut e, _a) = small();
        e.add_bus(&bus_at("A", 1, 4)).unwrap();
        e.add_bus(&bus_at("B", 1, 6)).unwrap();
        assert_eq!(route(&mut e, 4, 6), Response::Ok);
        assert_eq!(e.handle(&Command::RemovePoint { input: 4, output: 6 }), Response::Ok);
        assert_eq!(route(&mut e, 6, 4), Response::Ok, "the fading route no longer counts");
    }

    #[test]
    fn removing_a_bus_drops_its_routes_and_silences_its_returns() {
        let (mut e, mut a) = small();
        let b = e.add_bus(&bus_at("Verb", 1, 8)).unwrap();
        route(&mut e, 0, 8);
        route(&mut e, 8, 3);
        for _ in 0..20 {
            block(&mut e, &mut a, 1.0, 3);
        }
        assert_eq!(e.handle(&Command::RemoveSlot { id: b }), Response::Ok);
        let Response::Points(p) = e.handle(&Command::ListPoints) else { panic!() };
        assert!(p.is_empty());
        block(&mut e, &mut a, 1.0, 3);
        assert_eq!(a.inputs_mut().channel(8)[0], 0.0, "returns silent once the bus is gone");
        for _ in 0..40 {
            block(&mut e, &mut a, 1.0, 3);
        }
        assert_eq!(block(&mut e, &mut a, 1.0, 3), 0.0);
        assert!(!e.slots().iter().any(|s| s.id == b));
    }

    #[test]
    fn a_new_bus_does_not_inherit_routes_left_on_its_channels() {
        let (mut e, _a) = small();
        route(&mut e, 8, 8); // a leftover route on free channels
        route(&mut e, 0, 1); // unrelated
        e.add_bus(&bus_at("Verb", 1, 8)).unwrap();
        let Response::Points(p) = e.handle(&Command::ListPoints) else { panic!() };
        assert_eq!(p.iter().map(|p| (p.input, p.output)).collect::<Vec<_>>(), vec![(0, 1)]);
    }

    #[test]
    fn bus_sizes_and_counts_are_limited() {
        let (mut e, _a) = Engine::new(EngineConfig::new(48_000.0, 64));
        assert_eq!(e.add_bus(&bus("x", 0)), Err(EngineError::BusChannels));
        assert_eq!(e.add_bus(&bus("x", 65)), Err(EngineError::BusChannels));
        for _ in 0..MAX_BUSES {
            e.add_bus(&bus("x", 1)).unwrap();
        }
        assert_eq!(e.add_bus(&bus("x", 1)), Err(EngineError::TooManyBuses));
        assert_eq!(EngineError::BusChannels.to_string(), "an insert bus has 1 to 64 channels");
        assert_eq!(EngineError::TooManyBuses.to_string(), "too many insert buses");
    }

    #[test]
    fn add_bus_over_the_api_replies_with_its_slot() {
        let (mut e, _a) = small();
        let cmd = Command::AddBus { name: "Verb".into(), channels: 2, first_input: None, first_output: None };
        let Response::SlotsAdded(ids) = e.handle(&cmd) else { panic!() };
        assert_eq!(e.slots().iter().find(|s| s.id == ids[0]).map(|s| s.name.as_str()), Some("Verb"));
    }

    struct Panics;
    impl Processor for Panics {
        fn process(&mut self, _io: BusIo<'_>) -> Result<(), ProcessError> {
            panic!("test plugin crash");
        }
    }

    /// Counts its calls; fails the first `fail` of them; multiplies by `gain`.
    #[derive(Default)]
    struct Counter {
        calls: Arc<AtomicU64>,
        stopped: Arc<AtomicU64>,
        fail: u64,
        gain: f32,
    }

    impl Processor for Counter {
        fn process(&mut self, mut io: BusIo<'_>) -> Result<(), ProcessError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n < self.fail {
                return Err(ProcessError);
            }
            io.passthrough();
            for c in 0..io.channels() {
                for x in io.ret(c) {
                    *x *= self.gain;
                }
            }
            Ok(())
        }

        fn stop(&mut self) {
            self.stopped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_faulted_processor_is_not_called_again() {
        let (mut e, mut a) = small();
        let calls = Arc::new(AtomicU64::new(0));
        let p = Counter { calls: calls.clone(), fail: 1, gain: 1.0, ..Counter::default() };
        let b = e.add_bus_with(&bus_at("Bad", 1, 8), Some(Box::new(p))).unwrap();
        route(&mut e, 0, 8);
        route(&mut e, 8, 3);
        for _ in 0..30 {
            assert_eq!(block(&mut e, &mut a, 1.0, 3), 0.0, "silent after the fault");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1, "never called again");
        let Response::Health { slots, .. } = e.handle(&Command::Health) else { panic!() };
        assert_eq!(slots.iter().find(|h| h.id == b).unwrap().device_faults, 1);
    }

    #[test]
    fn a_new_processor_replaces_the_old_and_the_old_comes_back() {
        let (mut e, mut a) = small();
        let stopped = Arc::new(AtomicU64::new(0));
        let first = Counter { stopped: stopped.clone(), fail: 1, gain: 1.0, ..Counter::default() };
        let b = e.add_bus_with(&bus_at("Verb", 1, 8), Some(Box::new(first))).unwrap();
        route(&mut e, 0, 8);
        route(&mut e, 8, 3);
        for _ in 0..30 {
            block(&mut e, &mut a, 1.0, 3); // faults on the first block
        }
        e.set_bus_processor(b, Some(Box::new(Counter { gain: 0.5, ..Counter::default() }))).unwrap();
        block(&mut e, &mut a, 1.0, 3);
        assert_eq!(block(&mut e, &mut a, 1.0, 3), 0.5, "the new processor runs: the fault was cleared");
        let back = e.take_returned_processors();
        assert_eq!(back.len(), 1);
        assert_eq!(stopped.load(Ordering::Relaxed), 1, "stopped on the audio side before coming back");
        e.set_bus_processor(b, None).unwrap();
        block(&mut e, &mut a, 1.0, 3);
        assert_eq!(block(&mut e, &mut a, 1.0, 3), 1.0, "no processor: a summing bus again");
        assert_eq!(e.take_returned_processors().len(), 1);
    }

    #[test]
    fn a_silent_bus_passes_nothing() {
        let (mut e, mut a) = small();
        let b = e.add_bus(&bus_at("Missing", 1, 8)).unwrap();
        route(&mut e, 0, 8);
        route(&mut e, 8, 3);
        e.set_bus_silent(b, true).unwrap();
        for _ in 0..30 {
            assert_eq!(block(&mut e, &mut a, 1.0, 3), 0.0);
        }
        e.set_bus_silent(b, false).unwrap();
        for _ in 0..30 {
            block(&mut e, &mut a, 1.0, 3);
        }
        assert_eq!(block(&mut e, &mut a, 1.0, 3), 1.0);
    }

    #[test]
    fn only_buses_take_processors() {
        let (mut e, _a) = small();
        let (id, _) = e.add_soft_input(&spec("in", 2)).unwrap();
        assert_eq!(e.set_bus_processor(id, None), Err(EngineError::NotABus(id)));
        assert_eq!(EngineError::NotABus(7).to_string(), "slot 7 is not an insert bus");
    }

    #[test]
    fn a_panicking_processor_silences_its_bus_and_counts_a_fault() {
        let (mut e, mut a) = small();
        let b = e.add_bus_with(&bus_at("Bad", 1, 8), Some(Box::new(Panics))).unwrap();
        route(&mut e, 0, 8);
        route(&mut e, 8, 3);
        for _ in 0..3 {
            assert_eq!(block(&mut e, &mut a, 1.0, 3), 0.0);
        }
        let Response::Health { slots, .. } = e.handle(&Command::Health) else { panic!() };
        let h = slots.iter().find(|h| h.id == b).expect("a bus reports health");
        assert_eq!(h.device_faults, 1, "a fault stops the processor: one panic, not one per block ({h:?})");
    }

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
