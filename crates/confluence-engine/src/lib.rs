//! The Confluence engine: real-time audio side, control side, simulation,
//! journal, and (on Windows) the internal clock and named-pipe server.

pub mod audio;
#[cfg(windows)]
pub mod clock;
pub mod engine;
#[cfg(windows)]
pub mod ipc;
pub mod journal;
pub mod sim;

#[cfg(windows)]
pub use confluence_rt as rt;

pub use audio::AudioEngine;
pub use engine::{Engine, EngineConfig, EngineError, SoftSlotSpec};
