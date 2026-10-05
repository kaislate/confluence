//! The Confluence engine: real-time audio side, control side, simulation,
//! journal, and (on Windows) the internal clock and named-pipe server.

pub mod alloc;
pub mod audio;
#[cfg(windows)]
pub mod clock;
#[cfg(windows)]
pub mod devices;
pub mod engine;
#[cfg(windows)]
pub mod ipc;
pub mod journal;
pub mod midi;
#[cfg(windows)]
pub mod plugins;
#[cfg(windows)]
pub mod publish;
pub mod sim;

#[cfg(windows)]
pub use confluence_rt as rt;

pub use audio::{AudioEngine, StrictSide, MAX_BUSES};
pub use engine::{
    BusSpec, Engine, EngineConfig, EngineError, MasterChannels, MasterSlotSpec, OfflineSlotSpec, PluginControl,
    PluginParts, SoftSlotSpec, StrictSlotSpec, StrictStats,
};
