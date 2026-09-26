//! The Confluence engine: real-time audio side, control side, simulation,
//! journal, and (on Windows) the internal clock and named-pipe server.

pub mod audio;
#[cfg(windows)]
pub mod clock;
pub mod engine;
#[cfg(windows)]
pub mod ipc;
pub mod journal;
#[cfg(windows)]
pub mod rt;
pub mod sim;

pub use audio::AudioEngine;
pub use engine::{Engine, EngineConfig, EngineError, SoftSlotSpec};
