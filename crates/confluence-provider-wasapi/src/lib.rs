//! WASAPI provider for Confluence: shared-mode render/capture endpoints and
//! per-application capture via process loopback (spec §7.2, §7.3). All COM work
//! happens on dedicated MTA threads.

#[cfg(windows)]
mod endpoints;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod stream;

#[cfg(windows)]
pub use endpoints::{default_endpoint, endpoints, find_endpoint, Direction, Endpoint};
#[cfg(windows)]
pub use process::find_process;
#[cfg(windows)]
pub use stream::{CaptureFn, Handler, RenderFn, StreamFormat, StreamHealth, Target, WasapiStream};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum WasapiError {
    #[error("{call}: {message}")]
    Win32 { call: &'static str, message: String },
    #[error("no {0} endpoint named '{1}'")]
    NoSuchEndpoint(&'static str, String),
    #[error("no running process named '{0}'")]
    NoSuchProcess(String),
    #[error("activating the capture timed out")]
    Timeout,
    #[error("the stream thread stopped unexpectedly")]
    Gone,
    #[error("the stream has already been started")]
    AlreadyStarted,
}

#[cfg(windows)]
pub(crate) trait Context<T> {
    fn call(self, call: &'static str) -> Result<T, WasapiError>;
}

#[cfg(windows)]
impl<T> Context<T> for windows::core::Result<T> {
    fn call(self, call: &'static str) -> Result<T, WasapiError> {
        self.map_err(|e| WasapiError::Win32 { call, message: e.message() })
    }
}
