//! ASIO host for Confluence: hosts several ASIO drivers in one process, each on
//! its own STA control thread, with per-slot callback trampolines (spec §7.1).

pub mod convert;
#[cfg(windows)]
pub mod registry;
pub mod sys;
