//! What Confluence tells a plugin about its host, and the callbacks a plugin
//! can make.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_host::prelude::*;

pub(crate) struct Host;

impl HostHandlers for Host {
    type Shared<'a> = Shared;
    type MainThread<'a> = Main;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostLog>().register::<HostParams>().register::<HostGui>();
    }
}

/// Thread-safe callbacks. `request_callback` only raises a flag: the plugin
/// thread polls it and calls the plugin back on its own thread.
pub(crate) struct Shared {
    pub callback: Arc<AtomicBool>,
    pub gui: Arc<GuiRequests>,
}

/// What a plugin's editor asked of us; the plugin thread acts on it.
#[derive(Default)]
pub(crate) struct GuiRequests {
    pub resize: Mutex<Option<(u32, u32)>>,
    pub show: AtomicBool,
    pub hide: AtomicBool,
    /// A floating editor was closed by the user: (asked, already destroyed).
    pub closed: Mutex<Option<bool>>,
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, size: GuiSize) -> Result<(), HostError> {
        if let Ok(mut r) = self.gui.resize.lock() {
            *r = Some((size.width, size.height));
        }
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        self.gui.show.store(true, Ordering::Release);
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        self.gui.hide.store(true, Ordering::Release);
        Ok(())
    }

    fn closed(&self, was_destroyed: bool) {
        if let Ok(mut c) = self.gui.closed.lock() {
            *c = Some(was_destroyed);
        }
    }
}

impl SharedHandler<'_> for Shared {
    fn request_restart(&self) {
        // Re-activation on request is not supported yet: the plugin keeps running as it is.
    }

    fn request_process(&self) {
        // The engine processes every bus every block anyway.
    }

    fn request_callback(&self) {
        self.callback.store(true, Ordering::Release);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if severity >= LogSeverity::Warning {
            eprintln!("confluence-engine: plugin {severity}: {message}");
        }
    }
}

impl HostParamsImplShared for Shared {
    fn request_flush(&self) {
        // Parameters are delivered with every block, so there is nothing to flush.
    }
}

pub(crate) struct Main;

impl MainThreadHandler<'_> for Main {}

impl HostParamsImplMainThread for Main {
    fn rescan(&self, _flags: ParamRescanFlags) {
        // A changed parameter list is picked up the next time the plugin is loaded.
    }

    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

pub(crate) fn host_info() -> Result<HostInfo, String> {
    HostInfo::new("Confluence", "Confluence", "https://github.com/kaislate/confluence", env!("CARGO_PKG_VERSION"))
        .map_err(|e| e.to_string())
}
