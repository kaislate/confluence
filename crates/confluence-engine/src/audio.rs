//! The real-time half of the engine: one call to [`AudioEngine::process_block`]
//! per master block. Slots are added and removed through a mailbox; removed
//! slot state goes back to the control side to be dropped there.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use confluence_core::bridge::{InputEngineSide, OutputEngineSide};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::mailbox::{Receiver, Sender};
use confluence_core::matrix::MatrixRouter;

/// Maximum soft input plus soft output slots the audio side can hold.
pub const MAX_SLOTS: usize = 64;

pub(crate) struct InputEntry {
    pub id: u32,
    pub first_channel: usize,
    pub side: InputEngineSide,
}

pub(crate) struct OutputEntry {
    pub id: u32,
    pub first_channel: usize,
    pub side: OutputEngineSide,
}

pub(crate) enum AudioMsg {
    AddInput(Box<InputEntry>),
    AddOutput(Box<OutputEntry>),
    Remove(u32),
}

/// Slot state handed back to the control side (never dropped on the audio thread).
pub(crate) enum Returned {
    Input(Box<InputEntry>),
    Output(Box<OutputEntry>),
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
    pub(crate) inbox: Receiver<AudioMsg>,
    pub(crate) returns: Sender<Returned>,
    pub(crate) blocks: Arc<AtomicU64>,
}

impl AudioEngine {
    /// Runs one master block. `now` is the block time in seconds on the clock
    /// shared with device timestamps. Never allocates, locks or frees.
    pub fn process_block(&mut self, now: f64) {
        self.apply_messages();
        for e in self.soft_inputs.iter_mut() {
            e.side.read(&mut self.inputs, e.first_channel, now, 0.0);
        }
        self.router.process(&self.inputs, &mut self.outputs);
        for e in self.soft_outputs.iter_mut() {
            e.side.write(&self.outputs, e.first_channel, now, 0.0);
        }
        self.blocks.fetch_add(1, Ordering::Relaxed);
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
                AudioMsg::Remove(id) => {
                    if let Some(i) = self.soft_inputs.iter().position(|e| e.id == id) {
                        let e = self.soft_inputs.swap_remove(i);
                        self.give_back(Returned::Input(e));
                    }
                    if let Some(i) = self.soft_outputs.iter().position(|e| e.id == id) {
                        let e = self.soft_outputs.swap_remove(i);
                        self.give_back(Returned::Output(e));
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
