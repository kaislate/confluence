//! The Confluence engine: real-time audio side, control side, simulation,
//! journal, and (on Windows) the internal clock and named-pipe server.

pub mod audio;
pub mod engine;

pub use audio::AudioEngine;
pub use engine::{Engine, EngineConfig, EngineError, SoftSlotSpec};
