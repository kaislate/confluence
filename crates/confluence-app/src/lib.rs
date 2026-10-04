//! Confluence's desktop GUI: the matrix, devices and clock health of a running
//! engine, drawn from a live `StateStore` copy; edits go through a worker
//! thread so the window never waits on the engine.

pub mod engine_launch;
pub mod theme;
