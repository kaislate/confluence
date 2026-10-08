//! Confluence's desktop GUI: the matrix, devices and clock health of a running
//! engine, drawn from a live `StateStore` copy; edits go through a worker
//! thread so the window never waits on the engine.

pub mod app;
pub mod bays;
pub mod bridge;
pub mod commands;
pub mod devices;
pub mod devices_screen;
pub mod engine_launch;
pub mod gear;
pub mod graph;
pub mod grid_view;
pub mod inspector;
pub mod matrix;
pub mod names;
pub mod notify;
pub mod pending;
pub mod plugins;
pub mod prefs;
pub mod scenes;
pub mod scripts;
pub mod settings;
pub mod shell;
pub mod skin;
pub mod theme;
