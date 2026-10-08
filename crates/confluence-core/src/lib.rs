//! OS-independent DSP core of Confluence: gain, buffers, lock-free mailbox,
//! matrix routing, clock estimation and asynchronous resampling.

pub mod asrc;
pub mod bridge;
pub mod buffer;
pub mod clock;
pub mod gain;
pub mod mailbox;
pub mod matrix;
pub mod meter;
pub mod params;
pub mod plan;
pub mod processor;
mod sync;
