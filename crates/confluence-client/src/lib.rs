//! Client side of the Confluence Control API over its named pipe: one-shot
//! commands ([`Client`]) and state subscriptions ([`Subscription`]).

use std::fs::{File, OpenOptions};
use std::io;
use std::time::{Duration, Instant};

use confluence_api::{read_envelope, write_frame, Command, Envelope, FrameError, Response};

mod subscription;
pub use subscription::Subscription;

mod store;
pub use store::{ConnState, Gap, HealthSample, History, SessionEnd, StateStore, Store, StoreView, HISTORY_LEN};

pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// Default pipe name for the current user.
pub fn default_pipe_name() -> String {
    format!("confluence-{}", std::env::var("USERNAME").unwrap_or_else(|_| "user".into()))
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the engine closed the connection")]
    Closed,
    #[error("the engine refused: {0}")]
    Refused(String),
    #[error("unexpected reply: {0}")]
    Unexpected(String),
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Frame(FrameError::Io(e))
    }
}

/// Opens the pipe, retrying for up to `timeout` while it is missing or busy.
pub(crate) fn open(name: &str, timeout: Duration) -> io::Result<File> {
    let path = pipe_path(name);
    let deadline = Instant::now() + timeout;
    loop {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => return Ok(file),
            Err(e) if Instant::now() < deadline && matches!(e.raw_os_error(), Some(2) | Some(231)) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Blocking Control API client: one request, one reply.
pub struct Client {
    file: File,
    next_id: u32,
}

impl Client {
    /// Connects, retrying for up to `timeout` while the pipe is missing or busy.
    pub fn connect(name: &str, timeout: Duration) -> io::Result<Self> {
        Ok(Client { file: open(name, timeout)?, next_id: 1 })
    }

    pub fn call(&mut self, cmd: Command) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        write_frame(&mut self.file, &Envelope::new(id, cmd))?;
        match read_envelope::<_, Response>(&mut self.file)? {
            Some(env) => Ok(env.body),
            None => Err(ClientError::Closed),
        }
    }
}
