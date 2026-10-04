//! The real-time half of the engine: one call per master block, either
//! [`AudioEngine::process_block`] (internal clock) or
//! [`AudioEngine::process_master_block`] (hardware master). Slots are added and
//! removed through a mailbox; removed slot state goes back to the control side
//! to be dropped there.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use confluence_core::bridge::{InputEngineSide, OutputEngineSide};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::clock::{RateEstimator, DEFAULT_RATE_BANDWIDTH_HZ};
use confluence_core::mailbox::{Receiver, Sender};
use confluence_core::matrix::MatrixRouter;

/// Maximum soft input plus soft output slots the audio side can hold.
pub const MAX_SLOTS: usize = 64;

pub(crate) struct InputEntry {
    pub id: u32,
    pub first_channel: usize,
    pub channels: usize,
    pub side: InputEngineSide,
}

pub(crate) struct OutputEntry {
    pub id: u32,
    pub first_channel: usize,
    pub side: OutputEngineSide,
}

/// The audio-thread end of a *strict* slot: a device on the engine's own
/// clock (no resampling), such as a VASIO driver, exchanging exactly one
/// block each way per master block. Both calls must be real-time safe.
pub trait StrictSide: Send {
    /// Before routing: writes the slot's input channels of `inputs`.
    fn receive(&mut self, inputs: &mut PlanarBuffer);
    /// After routing: takes the slot's output channels of `outputs`.
    fn send(&mut self, outputs: &PlanarBuffer);
}

pub(crate) struct StrictEntry {
    pub id: u32,
    pub first_input: usize,
    pub inputs: usize,
    pub side: Box<dyn StrictSide>,
}

pub(crate) enum AudioMsg {
    AddInput(Box<InputEntry>),
    AddOutput(Box<OutputEntry>),
    AddStrict(Box<StrictEntry>),
    Remove(u32),
}

/// Slot state handed back to the control side (never dropped on the audio thread).
pub(crate) enum Returned {
    Input(Box<InputEntry>),
    Output(Box<OutputEntry>),
    Strict(Box<StrictEntry>),
}

pub struct AudioEngine {
    pub(crate) router: MatrixRouter,
    pub(crate) inputs: PlanarBuffer,
    pub(crate) outputs: PlanarBuffer,
    // Entries stay boxed: they arrive in a Box, and unboxing them would free
    // that allocation here on the audio thread.
    #[allow(clippy::vec_box)]
    pub(crate) soft_inputs: Vec<Box<InputEntry>>,
    #[allow(clippy::vec_box)]
    pub(crate) soft_outputs: Vec<Box<OutputEntry>>,
    #[allow(clippy::vec_box)]
    pub(crate) strict: Vec<Box<StrictEntry>>,
    pub(crate) inbox: Receiver<AudioMsg>,
    pub(crate) returns: Sender<Returned>,
    pub(crate) blocks: Arc<AtomicU64>,
    pub(crate) sample_rate: f64,
    /// Drift of a hardware master against the engine time base; `None` on the internal clock.
    pub(crate) master_est: Option<RateEstimator>,
    pub(crate) master_ppm: Arc<AtomicU64>,
    pub(crate) load: LoadMeter,
}

/// Smoothing time constant of the DSP load, in seconds.
const LOAD_TIME_CONSTANT_S: f64 = 1.0;

/// Smoothed fraction of the block period the audio thread spends processing.
/// Written on the audio thread (atomics only), read by the control side.
pub(crate) struct LoadMeter {
    smoothed: f64,
    shared: Arc<AtomicU32>,
}

impl LoadMeter {
    pub(crate) fn new(shared: Arc<AtomicU32>) -> Self {
        LoadMeter { smoothed: 0.0, shared }
    }

    pub(crate) fn record(&mut self, busy_s: f64, period_s: f64) {
        if period_s <= 0.0 {
            return;
        }
        let x = (busy_s / period_s).clamp(0.0, 1.0);
        let alpha = (period_s / LOAD_TIME_CONSTANT_S).min(1.0);
        self.smoothed += alpha * (x - self.smoothed);
        self.shared.store((self.smoothed as f32).to_bits(), Ordering::Relaxed);
    }
}

impl AudioEngine {
    /// Runs one master block. `now` is the block time in seconds on the clock
    /// shared with device timestamps. Never allocates, locks or frees.
    pub fn process_block(&mut self, now: f64) {
        self.run(now, 0.0);
    }

    /// Runs one block clocked by a hardware master whose callback delivered
    /// `frames` frames since its previous callback, ending at `now` (engine time
    /// base). The master's own drift is measured here and passed to every soft
    /// slot's resampler. The caller copies the master's inputs into
    /// [`inputs_mut`](Self::inputs_mut) before, and its outputs out of
    /// [`outputs`](Self::outputs) after, this call.
    pub fn process_master_block(&mut self, now: f64, frames: u32) {
        let rate = self.sample_rate;
        let est = self.master_est.get_or_insert_with(|| RateEstimator::new(rate, DEFAULT_RATE_BANDWIDTH_HZ));
        est.update(frames, now);
        let ppm = if est.updates() > 1 { est.ppm() } else { 0.0 };
        self.master_ppm.store(ppm.to_bits(), Ordering::Relaxed);
        self.run(now, ppm);
    }

    /// The engine's input channel space, for a master device to write into.
    pub fn inputs_mut(&mut self) -> &mut PlanarBuffer {
        &mut self.inputs
    }

    /// The engine's output channel space, for a master device to read from.
    pub fn outputs(&self) -> &PlanarBuffer {
        &self.outputs
    }

    fn run(&mut self, now: f64, master_ppm: f64) {
        let start = std::time::Instant::now();
        self.apply_messages();
        for e in self.strict.iter_mut() {
            e.side.receive(&mut self.inputs);
        }
        for e in self.soft_inputs.iter_mut() {
            e.side.read(&mut self.inputs, e.first_channel, now, master_ppm);
        }
        self.router.process(&self.inputs, &mut self.outputs);
        for e in self.soft_outputs.iter_mut() {
            e.side.write(&self.outputs, e.first_channel, now, master_ppm);
        }
        for e in self.strict.iter_mut() {
            e.side.send(&self.outputs);
        }
        self.blocks.fetch_add(1, Ordering::Relaxed);
        let period = self.outputs.frames() as f64 / self.sample_rate;
        self.load.record(start.elapsed().as_secs_f64(), period);
    }

    /// Master block size in frames.
    pub fn block(&self) -> usize {
        self.outputs.frames()
    }

    fn apply_messages(&mut self) {
        while let Some(msg) = self.inbox.try_recv() {
            match msg {
                AudioMsg::AddInput(e) => {
                    if self.soft_inputs.len() < self.soft_inputs.capacity() {
                        self.soft_inputs.push(e);
                    } else {
                        self.give_back(Returned::Input(e));
                    }
                }
                AudioMsg::AddOutput(e) => {
                    if self.soft_outputs.len() < self.soft_outputs.capacity() {
                        self.soft_outputs.push(e);
                    } else {
                        self.give_back(Returned::Output(e));
                    }
                }
                AudioMsg::AddStrict(e) => {
                    if self.strict.len() < self.strict.capacity() {
                        self.strict.push(e);
                    } else {
                        self.give_back(Returned::Strict(e));
                    }
                }
                AudioMsg::Remove(id) => {
                    if let Some(i) = self.soft_inputs.iter().position(|e| e.id == id) {
                        let e = self.soft_inputs.swap_remove(i);
                        // Nothing writes these channels any more: silence them so
                        // the router does not keep mixing the last delivered block.
                        for ch in e.first_channel..e.first_channel + e.channels {
                            self.inputs.channel_mut(ch).fill(0.0);
                        }
                        self.give_back(Returned::Input(e));
                    }
                    if let Some(i) = self.soft_outputs.iter().position(|e| e.id == id) {
                        let e = self.soft_outputs.swap_remove(i);
                        self.give_back(Returned::Output(e));
                    }
                    if let Some(i) = self.strict.iter().position(|e| e.id == id) {
                        let e = self.strict.swap_remove(i);
                        for ch in e.first_input..e.first_input + e.inputs {
                            self.inputs.channel_mut(ch).fill(0.0);
                        }
                        self.give_back(Returned::Strict(e));
                    }
                }
            }
        }
    }

    fn give_back(&mut self, r: Returned) {
        if let Err(r) = self.returns.try_send(r) {
            // The return queue is sized for every slot plus every message in
            // flight, so this cannot happen; leaking beats freeing here.
            std::mem::forget(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_load_is_the_smoothed_busy_fraction() {
        let shared = Arc::new(AtomicU32::new(0));
        let mut m = LoadMeter::new(shared.clone());
        let period = 256.0 / 48_000.0;
        for _ in 0..2000 {
            m.record(period * 0.25, period); // a quarter of every block
        }
        let v = f32::from_bits(shared.load(Ordering::Relaxed));
        assert!((v - 0.25).abs() < 0.01, "{v}");
        m.record(period * 10.0, period); // one overlong block: a bump, clamped
        let v = f32::from_bits(shared.load(Ordering::Relaxed));
        assert!(v > 0.25 && v <= 1.0, "{v}");
    }
}
