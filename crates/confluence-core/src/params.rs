//! Dense table of target gains shared between control and audio threads.

use std::sync::atomic::{AtomicU32, Ordering};

/// One `f32` target gain per (input, output) cell, stored as bits in an `AtomicU32`.
/// Each cell is independent, so `Relaxed` ordering is sufficient; the snapshot
/// mailbox provides the ordering needed when a point first becomes active.
pub struct ParamTable {
    cells: Box<[AtomicU32]>,
    max_inputs: u32,
    max_outputs: u32,
}

impl ParamTable {
    pub fn new(max_inputs: usize, max_outputs: usize) -> Self {
        let cells = (0..max_inputs * max_outputs).map(|_| AtomicU32::new(0)).collect();
        Self { cells, max_inputs: max_inputs as u32, max_outputs: max_outputs as u32 }
    }

    pub fn max_inputs(&self) -> u32 {
        self.max_inputs
    }

    pub fn max_outputs(&self) -> u32 {
        self.max_outputs
    }

    /// Number of cells (`max_inputs * max_outputs`).
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Cell index of (input, output). Callers must keep both in range.
    #[inline]
    pub fn cell(&self, input: u32, output: u32) -> u32 {
        input * self.max_outputs + output
    }

    #[inline]
    pub fn set(&self, cell: u32, gain: f32) {
        self.cells[cell as usize].store(gain.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self, cell: u32) -> f32 {
        f32::from_bits(self.cells[cell as usize].load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_start_silent_and_round_trip() {
        let t = ParamTable::new(3, 4);
        assert_eq!(t.len(), 12);
        let c = t.cell(2, 3);
        assert_eq!(c, 11);
        assert_eq!(t.get(c), 0.0);
        t.set(c, -0.5);
        assert_eq!(t.get(c), -0.5);
    }
}
