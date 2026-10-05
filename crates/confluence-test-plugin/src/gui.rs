//! The gain plugin's editor: a fixed-size window embedded in the host's. A
//! click in it sets Gain to [`EDITOR_CLICK_GAIN`] dB, as if a knob was moved,
//! and the plugin reports the change like any edit made in its own editor.

use std::cell::Cell;
use std::sync::OnceLock;

use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGuiImpl, Window};
use clack_plugin::prelude::*;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DrawTextW, EndPaint, DT_CENTER, DT_SINGLELINE, DT_VCENTER, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, RegisterClassW,
    SetWindowLongPtrW, ShowWindow, GWLP_USERDATA, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_LBUTTONDOWN, WM_PAINT,
    WNDCLASSW, WS_CHILD, WS_VISIBLE,
};

use crate::{GainMain, GainShared};

/// The editor's fixed size.
pub const EDITOR_SIZE: (u32, u32) = (320, 180);
/// What a click in the editor sets Gain to.
pub const EDITOR_CLICK_GAIN: f64 = -12.0;
/// The text the editor shows (tests look for its window by it).
pub const EDITOR_TEXT: &str = "Confluence Test Gain";

/// The editor window while it exists.
#[derive(Default)]
pub struct Editor {
    hwnd: Cell<isize>,
}

fn class() -> PCWSTR {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    let name = w!("ConfluenceTestGainEditor");
    REGISTERED.get_or_init(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc_),
            // SAFETY: this module's handle.
            hInstance: unsafe { GetModuleHandleW(None) }.map(Into::into).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        // SAFETY: the class struct is valid for the call.
        unsafe { RegisterClassW(&wc) };
    });
    name
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            // SAFETY: set to the plugin's shared state, which outlives the window.
            let shared = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const GainShared;
            if let Some(shared) = unsafe { shared.as_ref() } {
                shared.edit_gain(EDITOR_CLICK_GAIN);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: standard paint sequence on our own window.
            unsafe {
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = Default::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let mut text: Vec<u16> = EDITOR_TEXT.encode_utf16().collect();
                DrawTextW(dc, &mut text, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

impl PluginGuiImpl for GainMain<'_> {
    fn is_api_supported(&self, configuration: GuiConfiguration) -> bool {
        configuration.api_type == GuiApiType::WIN32 && !configuration.is_floating
    }

    fn get_preferred_api(&self) -> Option<GuiConfiguration<'_>> {
        Some(GuiConfiguration { api_type: GuiApiType::WIN32, is_floating: false })
    }

    fn create(&self, configuration: GuiConfiguration) -> Result<(), PluginError> {
        if self.is_api_supported(configuration) {
            Ok(())
        } else {
            Err(PluginError::Message("only an embedded Win32 editor"))
        }
    }

    fn destroy(&self) {
        let h = self.editor.hwnd.replace(0);
        if h != 0 {
            // SAFETY: our own child window.
            let _ = unsafe { DestroyWindow(HWND(h as *mut _)) };
        }
    }

    fn set_scale(&self, _scale: f64) -> Result<(), PluginError> {
        Ok(())
    }

    fn get_size(&self) -> Option<GuiSize> {
        Some(GuiSize { width: EDITOR_SIZE.0, height: EDITOR_SIZE.1 })
    }

    fn set_size(&self, _size: GuiSize) -> Result<(), PluginError> {
        Ok(())
    }

    fn set_parent(&self, window: Window) -> Result<(), PluginError> {
        let parent = window.as_win32_hwnd().ok_or(PluginError::Message("not a Win32 window"))?;
        // SAFETY: the host's window outlives this child (destroy comes first).
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class(),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                EDITOR_SIZE.0 as i32,
                EDITOR_SIZE.1 as i32,
                Some(HWND(parent)),
                None,
                GetModuleHandleW(None).ok().map(Into::into),
                None,
            )
        }
        .map_err(|_| PluginError::Message("could not create the editor"))?;
        // SAFETY: the shared state outlives the window (destroyed in `destroy`).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, self.shared as *const GainShared as isize) };
        self.editor.hwnd.set(hwnd.0 as isize);
        Ok(())
    }

    fn set_transient(&self, _window: Window) -> Result<(), PluginError> {
        Ok(())
    }

    fn suggest_title(&self, _title: &str) {}

    fn show(&self) -> Result<(), PluginError> {
        let h = self.editor.hwnd.get();
        // SAFETY: our own child window, if any.
        if h != 0 {
            let _ = unsafe { ShowWindow(HWND(h as *mut _), SW_SHOW) };
        }
        Ok(())
    }

    fn hide(&self) -> Result<(), PluginError> {
        let h = self.editor.hwnd.get();
        if h != 0 {
            // SAFETY: as above.
            let _ = unsafe { ShowWindow(HWND(h as *mut _), SW_HIDE) };
        }
        Ok(())
    }
}
