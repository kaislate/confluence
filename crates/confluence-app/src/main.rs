//! `confluence.exe`: the Confluence desktop GUI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use confluence_app::app::{flag, AppConfig, ConfluenceApp, INSPECTOR_KEY};
use confluence_app::engine_launch::{engine_args, engine_next_to, ENGINE_EXE};
use confluence_client::default_pipe_name;
use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let default_pipe = default_pipe_name();
    let pipe = flag(&args, "--pipe").unwrap_or_else(|| default_pipe.clone());
    let skin = flag(&args, "--skin").map(PathBuf::from);
    let engine_exe = std::env::current_exe().map(|p| engine_next_to(&p)).unwrap_or_else(|_| PathBuf::from(ENGINE_EXE));
    let config = AppConfig { engine_args: engine_args(&pipe, &default_pipe), pipe, engine_exe, skin };
    let options = eframe::NativeOptions {
        persist_window: true,
        viewport: egui::ViewportBuilder::default().with_title("Confluence").with_inner_size([1100.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Confluence",
        options,
        Box::new(move |cc| {
            let mut app = ConfluenceApp::new(config);
            if let Some(v) = cc.storage.and_then(|s| s.get_string(INSPECTOR_KEY)) {
                app.inspector_open = v != "false";
            }
            Ok(Box::new(app))
        }),
    )
}
