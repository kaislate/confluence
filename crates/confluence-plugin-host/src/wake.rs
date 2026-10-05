//! Waking the plugin thread: it waits on window messages and on this event
//! together, so requests are run at once even while it owns windows.

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};

    /// An auto-reset event, set whenever a request is queued.
    pub struct Wake(HANDLE);

    // SAFETY: an event handle may be used from any thread.
    unsafe impl Send for Wake {}
    // SAFETY: as above; SetEvent is thread-safe.
    unsafe impl Sync for Wake {}

    impl Wake {
        pub fn new() -> std::io::Result<Wake> {
            // SAFETY: no security attributes, unnamed event.
            let h = unsafe { CreateEventW(None, false, false, None) }.map_err(std::io::Error::other)?;
            Ok(Wake(h))
        }

        pub fn set(&self) {
            // SAFETY: the handle is valid for the lifetime of `self`.
            let _ = unsafe { SetEvent(self.0) };
        }

        pub fn handle(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for Wake {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Wake;

    impl Wake {
        pub fn new() -> std::io::Result<Wake> {
            Ok(Wake)
        }

        pub fn set(&self) {}
    }
}

pub use imp::Wake;
