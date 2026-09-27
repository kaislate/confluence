//! The Confluence Control API: typed, versioned requests and responses, plus
//! length-prefixed `postcard` framing used on the named pipe and in the journal.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Protocol version. Bump the major part for incompatible changes.
pub const API_VERSION: u16 = 1;

/// Largest accepted frame, guarding against corrupt or hostile length prefixes.
pub const MAX_FRAME_BYTES: u32 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Adds or updates a matrix point. Gain is in dB (−100 … +24; lower = silent).
    SetPoint {
        input: u32,
        output: u32,
        gain_db: f32,
        mute: bool,
        invert: bool,
    },
    /// Fades out and removes a matrix point.
    RemovePoint {
        input: u32,
        output: u32,
    },
    ListPoints,
    ListSlots,
    Health,
    /// Asks the engine process to exit cleanly.
    Shutdown,
    /// Lists audio devices the engine can open.
    ListDevices,
    /// Opens a device as one or more slots (an ASIO device becomes an input
    /// slot and an output slot). Replies `SlotsAdded`.
    AddDevice {
        kind: DeviceKind,
        name: String,
    },
    /// Closes a slot, removes the routes on its channels and frees them.
    RemoveSlot {
        id: u32,
    },
}

impl Command {
    /// True for commands that change engine state (and are journaled).
    pub fn is_mutation(&self) -> bool {
        matches!(self, Command::SetPoint { .. } | Command::RemovePoint { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointState {
    pub input: u32,
    pub output: u32,
    pub gain_db: f32,
    pub mute: bool,
    pub invert: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClockRole {
    Master,
    Strict,
    Soft,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotState {
    pub id: u32,
    pub name: String,
    /// Device binding, e.g. `asio:MOTU Gen 5` (empty for none).
    pub device: String,
    pub role: ClockRole,
    /// False while the bound device is missing; its channels stay reserved.
    pub online: bool,
    /// First global input channel and count (0 if the slot has no inputs).
    pub first_input: u32,
    pub inputs: u32,
    /// First global output channel and count (0 if the slot has no outputs).
    pub first_output: u32,
    pub outputs: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotHealth {
    pub id: u32,
    pub underruns: u64,
    pub overruns: u64,
    pub fill_frames: f64,
    pub target_frames: f64,
    pub device_ppm: f64,
    pub correction_ppm: f64,
    /// The device disappeared (unplugged or disabled); the slot keeps its channels.
    pub device_lost: bool,
    /// Callbacks whose handler faulted (the outputs were silenced instead).
    pub device_faults: u64,
    /// Reset, resync and rate-change requests from the driver. The engine does
    /// not re-open devices yet: re-add the device to apply them.
    pub driver_requests: u64,
    /// For a slot served to another program (VASIO): whether one is attached.
    /// `None` for slots where this does not apply.
    pub attached: Option<bool>,
}

/// Kinds of device the engine can open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    Asio,
    WasapiRender,
    WasapiCapture,
    /// Per-application capture (process loopback); `name` is the process name or PID.
    AppCapture,
    /// A Confluence virtual ASIO driver instance for a DAW. `name` is `N`
    /// (instance N, 2 channels each way), `N:C` (C each way) or `N:IxO`
    /// (I DAW inputs, O DAW outputs).
    Vasio,
}

impl DeviceKind {
    /// Prefix used in device bindings (`asio:<name>`).
    pub fn prefix(self) -> &'static str {
        match self {
            DeviceKind::Asio => "asio",
            DeviceKind::WasapiRender => "wasapi-out",
            DeviceKind::WasapiCapture => "wasapi-in",
            DeviceKind::AppCapture => "app",
            DeviceKind::Vasio => "vasio",
        }
    }

    /// Parses a prefix produced by [`DeviceKind::prefix`].
    pub fn from_prefix(p: &str) -> Option<Self> {
        [
            DeviceKind::Asio,
            DeviceKind::WasapiRender,
            DeviceKind::WasapiCapture,
            DeviceKind::AppCapture,
            DeviceKind::Vasio,
        ]
        .into_iter()
        .find(|k| k.prefix() == p)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    pub name: String,
    pub inputs: u32,
    pub outputs: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Points(Vec<PointState>),
    Slots(Vec<SlotState>),
    /// `notices`: engine-wide conditions a user should know about (e.g.
    /// device changes not being saved).
    Health {
        blocks: u64,
        slots: Vec<SlotHealth>,
        notices: Vec<String>,
    },
    Error(String),
    Devices(Vec<DeviceInfo>),
    SlotsAdded(Vec<u32>),
}

/// Every message on the wire carries the protocol version and a request id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u16,
    pub id: u32,
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(id: u32, body: T) -> Self {
        Self { version: API_VERSION, id, body }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(u32),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("unsupported protocol version {0}")]
    Version(u16),
}

/// Writes `value` as a little-endian u32 length followed by postcard bytes.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<(), FrameError> {
    let bytes = postcard::to_stdvec(value)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. Returns `Ok(None)` on a clean end of stream before a frame starts.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(postcard::from_bytes(&buf)?))
}

/// Reads one enveloped frame and rejects other protocol versions.
pub fn read_envelope<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<Envelope<T>>, FrameError> {
    match read_frame::<R, Envelope<T>>(r)? {
        Some(env) if env.version != API_VERSION => Err(FrameError::Version(env.version)),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_through_frames() {
        let cmd = Envelope::new(7, Command::SetPoint { input: 1, output: 2, gain_db: -6.0, mute: false, invert: true });
        let mut wire = Vec::new();
        write_frame(&mut wire, &cmd).unwrap();
        write_frame(&mut wire, &Envelope::new(8, Command::ListPoints)).unwrap();
        let mut r = &wire[..];
        assert_eq!(read_envelope::<_, Command>(&mut r).unwrap(), Some(cmd));
        assert_eq!(read_envelope::<_, Command>(&mut r).unwrap().map(|e| e.id), Some(8));
        assert_eq!(read_envelope::<_, Command>(&mut r).unwrap(), None);
    }

    #[test]
    fn device_commands_round_trip() {
        let cmd = Envelope::new(3, Command::AddDevice { kind: DeviceKind::Asio, name: "MOTU Gen 5".into() });
        let mut wire = Vec::new();
        write_frame(&mut wire, &cmd).unwrap();
        assert_eq!(read_envelope::<_, Command>(&mut &wire[..]).unwrap(), Some(cmd));
        for k in [DeviceKind::Asio, DeviceKind::WasapiRender, DeviceKind::WasapiCapture, DeviceKind::AppCapture] {
            assert_eq!(DeviceKind::from_prefix(k.prefix()), Some(k));
        }
        assert_eq!(DeviceKind::from_prefix("nope"), None);
    }

    #[test]
    fn oversized_length_prefix_is_rejected_without_allocating_it() {
        let mut wire = (MAX_FRAME_BYTES + 1).to_le_bytes().to_vec();
        wire.extend_from_slice(&[0; 8]);
        let err = read_frame::<_, Command>(&mut &wire[..]).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[test]
    fn other_versions_are_rejected() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Envelope { version: 99, id: 1, body: Command::Health }).unwrap();
        assert!(matches!(read_envelope::<_, Command>(&mut &wire[..]), Err(FrameError::Version(99))));
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_clean_end() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Envelope::new(1, Command::ListSlots)).unwrap();
        wire.truncate(wire.len() - 1);
        assert!(read_frame::<_, Envelope<Command>>(&mut &wire[..]).is_err());
    }
}
