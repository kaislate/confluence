//! Hosts CLAP plugins on Confluence's insert buses.
//!
//! - [`PluginThread`] owns every plugin instance: CLAP wants all main-thread
//!   calls on one thread.
//! - Each loaded plugin is a [`Processor`](confluence_core::processor::Processor)
//!   for its bus (the audio side) and a [`PluginLink`] for the engine (its
//!   parameters, its state, and the rings between the two).
//! - [`describe`] and [`check`] are what the engine runs in a separate process
//!   before it loads a plugin file itself.

mod host;
mod processor;
mod thread;

use std::path::Path;

pub use confluence_api::{ParamState, PluginInfo};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::processor::BusIo;
pub use thread::{PluginLink, PluginThread, Source};

/// Lists the plugins in a CLAP file.
pub fn describe(path: &Path) -> Result<Vec<PluginInfo>, String> {
    // SAFETY: runs the file's code; callers run this in a throwaway process.
    let entry = unsafe { clack_host::entry::PluginEntry::load(path) }
        .map_err(|e| format!("{} could not be loaded: {e}", path.display()))?;
    let factory = entry.get_plugin_factory().ok_or_else(|| format!("{} has no plugins", path.display()))?;
    let text = |s: Option<&std::ffi::CStr>| s.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(factory
        .plugin_descriptors()
        .filter_map(|d| {
            Some(PluginInfo {
                path: path.display().to_string(),
                id: d.id()?.to_string_lossy().into_owned(),
                name: text(d.name()),
                vendor: text(d.vendor()),
                version: text(d.version()),
            })
        })
        .collect())
}

/// Loads, activates and runs plugin `id` for one silent block, then destroys
/// it: the load check, run in a throwaway process so a crash cannot reach the
/// engine.
pub fn check(path: &Path, id: &str, rate: f64, block: u32) -> Result<(), String> {
    let thread = PluginThread::start().map_err(|e| e.to_string())?;
    let (link, mut processor) = thread.load(Source::File(path.to_path_buf()), id, rate, block, 2)?;
    let sends = PlanarBuffer::new(2, block as usize);
    let mut returns = PlanarBuffer::new(2, block as usize);
    let ran = processor.process(BusIo::new(&sends, 0, &mut returns, 0, 2));
    processor.stop();
    thread.reclaim(processor);
    drop(link);
    ran.map_err(|_| format!("{id} reported an error while processing"))
}
