//! ASIO host for Confluence: hosts several ASIO drivers in one process, each on
//! its own STA control thread, with per-slot callback trampolines (spec §7.1).

pub mod convert;
#[cfg(windows)]
mod device;
#[cfg(windows)]
pub mod fake;
#[cfg(windows)]
mod io;
#[cfg(windows)]
pub mod registry;
pub mod sys;
#[cfg(windows)]
mod trampolines;

#[cfg(windows)]
pub use device::{AsioDevice, AsioHostError, DriverInfo, DriverSource, GetClassObject, StreamConfig, StreamInfo};
#[cfg(windows)]
pub use io::AsioIo;
#[cfg(windows)]
pub use trampolines::{AsioHealth, MAX_DRIVERS};

/// Per-block audio handler, run on the driver's callback thread. Must be
/// real-time safe: no allocation, locks or blocking.
#[cfg(windows)]
pub trait AsioCallback: Send + 'static {
    fn process(&mut self, io: &mut AsioIo<'_>);
}

#[cfg(windows)]
impl<F: FnMut(&mut AsioIo<'_>) + Send + 'static> AsioCallback for F {
    fn process(&mut self, io: &mut AsioIo<'_>) {
        self(io)
    }
}
