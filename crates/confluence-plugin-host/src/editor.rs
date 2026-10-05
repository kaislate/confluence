//! Plugin editor windows, on the plugin thread. The plugin embeds its GUI in a
//! window of ours; one that only floats manages its own window.

pub(crate) use imp::*;

/// Something the user did to an editor window, reported by its window
/// procedure and handled by the plugin thread's loop.
pub(crate) enum WinEvent {
    /// The close button: (plugin).
    Close(u64),
    /// The user resized it: (plugin, client width, client height).
    Resize(u64, u32, u32),
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::sync::OnceLock;

    use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui, Window};
    use clack_host::prelude::PluginInstance;
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, RegisterClassW, SetForegroundWindow,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, GWL_STYLE, SIZE_MINIMIZED,
        SWP_NOMOVE, SWP_NOZORDER, SW_HIDE, SW_SHOW, SW_SHOWNORMAL, WINDOW_STYLE, WM_CLOSE, WM_DPICHANGED, WM_SIZE,
        WNDCLASSW, WS_CAPTION, WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED,
        WS_SYSMENU, WS_THICKFRAME,
    };

    use super::WinEvent;
    use crate::host::Host;

    /// A size a plugin may ask for, at least.
    const MIN_SIZE: (u32, u32) = (64, 48);
    /// Used when a plugin does not say how big its editor is.
    const DEFAULT_SIZE: (u32, u32) = (480, 320);

    thread_local! {
        static EVENTS: RefCell<Vec<WinEvent>> = const { RefCell::new(Vec::new()) };
    }

    /// What the window procedures reported since the last call.
    pub(crate) fn take_events() -> Vec<WinEvent> {
        EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()))
    }

    /// An open editor.
    pub(crate) struct Editor {
        /// Our window, when the plugin is embedded in it.
        hwnd: Option<HWND>,
    }

    fn win32(is_floating: bool) -> GuiConfiguration<'static> {
        GuiConfiguration { api_type: GuiApiType::WIN32, is_floating }
    }

    /// Whether the plugin has a Win32 editor, embedded or floating.
    pub(crate) fn has_editor(instance: &mut PluginInstance<Host>) -> bool {
        let handle = instance.plugin_handle();
        handle
            .get_extension::<PluginGui>()
            .is_some_and(|g| g.is_api_supported(&handle, win32(false)) || g.is_api_supported(&handle, win32(true)))
    }

    fn class() -> PCWSTR {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        let name = w!("ConfluencePluginEditor");
        REGISTERED.get_or_init(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
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

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: set to the plugin's id when the window was made.
        let plugin = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as u64;
        match msg {
            WM_CLOSE => {
                EVENTS.with(|e| e.borrow_mut().push(WinEvent::Close(plugin)));
                LRESULT(0)
            }
            WM_SIZE if wp.0 as u32 != SIZE_MINIMIZED => {
                let (w, h) = ((lp.0 & 0xffff) as u32, ((lp.0 >> 16) & 0xffff) as u32);
                EVENTS.with(|e| e.borrow_mut().push(WinEvent::Resize(plugin, w, h)));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // Windows suggests the new outer rectangle for the new scale.
                // SAFETY: lParam points to a RECT for this message.
                if let Some(r) = unsafe { (lp.0 as *const RECT).as_ref() } {
                    let _ = unsafe {
                        SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER)
                    };
                }
                LRESULT(0)
            }
            // SAFETY: default handling.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    /// Sizes the window so its client area is `w` × `h`.
    fn set_client_size(hwnd: HWND, w: u32, h: u32) {
        let (w, h) = (w.max(MIN_SIZE.0) as i32, h.max(MIN_SIZE.1) as i32);
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        // SAFETY: valid window; read-only style query.
        unsafe {
            let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
            let _ = AdjustWindowRectExForDpi(&mut r, style, false, WS_EX_APPWINDOW, GetDpiForWindow(hwnd));
            let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
        }
    }

    /// Opens the plugin's editor for plugin `id`, titled `title`.
    pub(crate) fn open(
        instance: &mut PluginInstance<Host>,
        id: u64,
        title: &str,
        name: &str,
    ) -> Result<Editor, String> {
        let handle = instance.plugin_handle();
        let gui = handle.get_extension::<PluginGui>().ok_or_else(|| format!("{name} has no editor"))?;
        let refused = || format!("{name}'s editor could not be opened");
        if gui.is_api_supported(&handle, win32(false)) {
            // SAFETY: a new top-level window on this thread.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_APPWINDOW,
                    class(),
                    &HSTRING::from(title),
                    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    DEFAULT_SIZE.0 as i32,
                    DEFAULT_SIZE.1 as i32,
                    None,
                    None,
                    GetModuleHandleW(None).ok().map(Into::into),
                    None,
                )
            }
            .map_err(|_| refused())?;
            // SAFETY: our own window.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, id as isize) };
            let fail = |why: String| {
                // SAFETY: our own window.
                let _ = unsafe { DestroyWindow(hwnd) };
                why
            };
            gui.create(&handle, win32(false)).map_err(|_| fail(refused()))?;
            // SAFETY: valid window.
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            let _ = gui.set_scale(&handle, f64::from(dpi.max(96)) / 96.0);
            if gui.can_resize(&handle) {
                // SAFETY: our own window.
                unsafe {
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 | (WS_THICKFRAME | WS_MAXIMIZEBOX).0;
                    SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
                }
            }
            let size = gui.get_size(&handle).unwrap_or(GuiSize { width: DEFAULT_SIZE.0, height: DEFAULT_SIZE.1 });
            // SAFETY: our window outlives the plugin's GUI (destroy comes first).
            if unsafe { gui.set_parent(&handle, Window::from_win32_hwnd(hwnd.0)) }.is_err() {
                gui.destroy(&handle);
                return Err(fail(refused()));
            }
            set_client_size(hwnd, size.width, size.height);
            // SAFETY: our own window.
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let _ = gui.show(&handle);
            // SAFETY: as above.
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
            Ok(Editor { hwnd: Some(hwnd) })
        } else if gui.is_api_supported(&handle, win32(true)) {
            gui.create(&handle, win32(true)).map_err(|_| refused())?;
            if let Ok(t) = CString::new(title) {
                gui.suggest_title(&handle, &t);
            }
            if gui.show(&handle).is_err() {
                gui.destroy(&handle);
                return Err(refused());
            }
            Ok(Editor { hwnd: None })
        } else {
            Err(format!("{name} has no editor"))
        }
    }

    /// Brings an open editor to the front.
    pub(crate) fn front(instance: &mut PluginInstance<Host>, editor: &Editor) {
        match editor.hwnd {
            // SAFETY: our own window.
            Some(hwnd) => unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                let _ = SetForegroundWindow(hwnd);
            },
            None => {
                let handle = instance.plugin_handle();
                if let Some(gui) = handle.get_extension::<PluginGui>() {
                    let _ = gui.show(&handle);
                }
            }
        }
    }

    /// Closes an editor: the plugin's GUI first, then our window.
    pub(crate) fn close(instance: &mut PluginInstance<Host>, editor: Editor) {
        let handle = instance.plugin_handle();
        if let Some(gui) = handle.get_extension::<PluginGui>() {
            gui.destroy(&handle);
        }
        if let Some(hwnd) = editor.hwnd {
            // SAFETY: our own window, no longer used by the plugin.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }

    /// The plugin asked for a new size.
    pub(crate) fn plugin_resized(editor: &Editor, w: u32, h: u32) {
        if let Some(hwnd) = editor.hwnd {
            set_client_size(hwnd, w, h);
        }
    }

    /// The plugin asked to be shown or hidden.
    pub(crate) fn plugin_visibility(editor: &Editor, show: bool) {
        if let Some(hwnd) = editor.hwnd {
            // SAFETY: our own window.
            let _ = unsafe { ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE }) };
        }
    }

    /// The user resized the window: the plugin follows if it can.
    pub(crate) fn user_resized(instance: &mut PluginInstance<Host>, editor: &Editor, w: u32, h: u32) {
        let handle = instance.plugin_handle();
        let Some(gui) = handle.get_extension::<PluginGui>() else { return };
        if editor.hwnd.is_none() || !gui.can_resize(&handle) {
            return;
        }
        let want = gui.adjust_size(&handle, GuiSize { width: w, height: h }).unwrap_or(GuiSize { width: w, height: h });
        let _ = gui.set_size(&handle, want);
        if (want.width, want.height) != (w, h) {
            plugin_resized(editor, want.width, want.height);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use clack_host::prelude::PluginInstance;

    use super::WinEvent;
    use crate::host::Host;

    pub(crate) struct Editor;

    pub(crate) fn take_events() -> Vec<WinEvent> {
        Vec::new()
    }
    pub(crate) fn has_editor(_: &mut PluginInstance<Host>) -> bool {
        false
    }
    pub(crate) fn open(_: &mut PluginInstance<Host>, _: u64, _: &str, name: &str) -> Result<Editor, String> {
        Err(format!("{name} has no editor"))
    }
    pub(crate) fn front(_: &mut PluginInstance<Host>, _: &Editor) {}
    pub(crate) fn close(_: &mut PluginInstance<Host>, _: Editor) {}
    pub(crate) fn plugin_resized(_: &Editor, _: u32, _: u32) {}
    pub(crate) fn plugin_visibility(_: &Editor, _: bool) {}
    pub(crate) fn user_resized(_: &mut PluginInstance<Host>, _: &Editor, _: u32, _: u32) {}
}
