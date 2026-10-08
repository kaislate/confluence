//! A subscription: the engine's snapshot, then its event stream.

use std::fs::File;
use std::time::Duration;

use confluence_api::{read_envelope, write_frame, Command, Envelope, Event, Response, State};

use crate::{open, ClientError};

/// One subscription connection. Engine death ends it with `Closed` (or an i/o error).
pub struct Subscription {
    file: File,
}

impl Subscription {
    /// Connects and subscribes: returns the snapshot and the stream.
    pub fn connect(name: &str, timeout: Duration) -> Result<(State, Subscription), ClientError> {
        Self::start(name, timeout, Command::Subscribe)
    }

    /// As [`connect`](Self::connect), with meter frames (`Event::Meters`)
    /// about 20 times a second among the events.
    pub fn connect_with_meters(name: &str, timeout: Duration) -> Result<(State, Subscription), ClientError> {
        Self::start(name, timeout, Command::SubscribeMeters)
    }

    fn start(name: &str, timeout: Duration, subscribe: Command) -> Result<(State, Subscription), ClientError> {
        let mut file = open(name, timeout)?;
        write_frame(&mut file, &Envelope::new(1, subscribe))?;
        match read_envelope::<_, Response>(&mut file)?.map(|e| e.body) {
            Some(Response::Snapshot(state)) => Ok((state, Subscription { file })),
            Some(Response::Error(e)) => Err(ClientError::Refused(e)),
            Some(other) => Err(ClientError::Unexpected(format!("{other:?}"))),
            None => Err(ClientError::Closed),
        }
    }

    /// The next event (blocks).
    pub fn recv(&mut self) -> Result<Event, ClientError> {
        match read_envelope::<_, Response>(&mut self.file)?.map(|e| e.body) {
            Some(Response::Event(e)) => Ok(e),
            Some(other) => Err(ClientError::Unexpected(format!("{other:?}"))),
            None => Err(ClientError::Closed),
        }
    }
}
