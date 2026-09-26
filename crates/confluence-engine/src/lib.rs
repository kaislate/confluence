//! The Confluence engine: real-time audio side, control side, simulation,
//! journal, and (on Windows) the internal clock and named-pipe server.

pub mod audio;
pub mod engine;
pub mod journal;
pub mod sim;

pub use audio::AudioEngine;
pub use engine::{Engine, EngineConfig, EngineError, SoftSlotSpec};
