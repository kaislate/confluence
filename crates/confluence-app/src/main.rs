//! `confluence.exe`: the Confluence desktop GUI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use confluence_app::app::{flag, startup_error_text, AppConfig, ConfluenceApp, INSPECTOR_KEY};
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
    let result = eframe::run_native(
        "Confluence",
        options,
        Box::new(move |cc| {
            let mut app = ConfluenceApp::new(config);
            if let Some(v) = cc.storage.and_then(|s| s.get_string(INSPECTOR_KEY)) {
                app.inspector_open = v != "false";
            }
            let saved = cc.storage.and_then(|s| s.get_string(confluence_app::settings::FINISH_KEY));
            if let Some(f) = saved.as_deref().and_then(confluence_app::gear::skins::Finish::from_name) {
                app.set_finish(f);
            }
            if let Some(v) = cc.storage.and_then(|s| s.get_string(confluence_app::settings::REDUCE_MOTION_KEY)) {
                app.set_reduce_motion(v == "true");
            }
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = &result {
        show_error(&startup_error_text(e));
    }
    result
}

/// A message box: a release build has no console to print to.
#[cfg(windows)]
fn show_error(text: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    // SAFETY: valid strings that outlive the call; no owner window.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Confluence"), MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
fn show_error(text: &str) {
    eprintln!("{text}");
}
