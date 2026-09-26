//! OS-independent DSP core of Confluence: gain, buffers, lock-free mailbox,
//! matrix routing, clock estimation and asynchronous resampling.

pub mod buffer;
pub mod gain;
pub mod mailbox;
pub mod matrix;
pub mod params;
mod sync;
