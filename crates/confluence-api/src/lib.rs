//! The Confluence Control API: typed, versioned requests and responses, plus
//! length-prefixed `postcard` framing used on the named pipe and in the journal.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

mod state;
pub use state::diff;

/// Protocol version. Bump the major part for incompatible changes.
pub const API_VERSION: u16 = 3;

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
    // New commands go at the end: postcard encodes the variant index, and the
    // journal still replays records written by older versions.
    /// Turns this connection into an event stream: the reply is
    /// `Response::Snapshot`, then only `Response::Event` frames follow.
    Subscribe,
    /// Engine status (rate, block, load, xruns, master).
    Status,
    /// Creates an insert bus of `channels` channels: send columns that feed
    /// it and return rows that carry its output, in the same block. The
    /// placement fields restore a saved layout (clients pass `None`).
    /// Replies `Added`. Remove it with `RemoveSlot`.
    AddBus {
        name: String,
        channels: u32,
        first_input: Option<u32>,
        first_output: Option<u32>,
    },
    /// Lists the CLAP plugins found on this PC. Replies `Plugins`.
    ListPlugins,
    /// Loads plugin `plugin_id` from the CLAP file `path` into an insert bus,
    /// replacing any plugin there. The file is checked in a separate process first.
    LoadPlugin {
        bus: BusRef,
        path: String,
        plugin_id: String,
    },
    /// Takes the plugin off a bus (it becomes a summing bus again).
    UnloadPlugin {
        bus: BusRef,
    },
    /// Sets a plugin parameter (clamped to its range).
    SetParam {
        bus: BusRef,
        param: u32,
        value: f64,
    },
    /// Loads a saved state chunk into a bus's plugin.
    SetPluginState {
        bus: BusRef,
        state: Vec<u8>,
    },
}

/// Which insert bus a command means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusRef {
    /// By slot id (what clients use; ids change between engine runs).
    Id(u32),
    /// By its first send column (stable across runs; what the journal uses).
    At(u32),
}

impl Command {
    /// True for commands that change engine state (and are journaled).
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Command::SetPoint { .. }
                | Command::RemovePoint { .. }
                | Command::LoadPlugin { .. }
                | Command::UnloadPlugin { .. }
                | Command::SetParam { .. }
                | Command::SetPluginState { .. }
        )
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

/// The `device` of an insert bus slot.
pub const BUS_DEVICE: &str = "bus";

impl SlotState {
    /// True for an insert bus (its inputs are the bus returns, its outputs the sends).
    pub fn is_bus(&self) -> bool {
        self.device == BUS_DEVICE
    }
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
    /// For a slot served to another program (VASIO, VAIO): whether one is attached.
    /// `None` for slots where this does not apply.
    pub attached: Option<bool>,
    /// When `attached` is `Some(false)`: why, in the slot's own terms (e.g.
    /// "no app playing" for VAIO).
    #[serde(default)]
    pub idle_note: Option<String>,
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
    /// The Confluence VAIO virtual Windows playback endpoint. `name` is `1`
    /// (Milestone 0 has one endpoint). What apps play to it becomes two engine inputs.
    Vaio,
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
            DeviceKind::Vaio => "vaio",
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
            DeviceKind::Vaio,
        ]
        .into_iter()
        .find(|k| k.prefix() == p)
    }
}

/// A CLAP plugin found on this PC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// The `.clap` file.
    pub path: String,
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
}

/// One parameter of a loaded plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamState {
    pub id: u32,
    pub name: String,
    /// Grouping path the plugin gives (e.g. `Filter/Envelope`), may be empty.
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    /// The value as the plugin shows it (e.g. `-6.0 dB`).
    pub text: String,
    /// Takes whole-number values only.
    pub stepped: bool,
    /// The plugin reports it; it cannot be set.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginStatus {
    Running,
    /// It reported an error or crashed while processing; its bus is silent
    /// until the plugin is loaded again.
    Faulted,
    /// It could not be loaded (e.g. its file is missing); its bus is silent.
    Failed(String),
}

/// The plugin on an insert bus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadedPlugin {
    /// The bus's slot id.
    pub bus: u32,
    pub info: PluginInfo,
    pub status: PluginStatus,
    /// Samples of delay the plugin reports (not compensated yet).
    pub latency: u32,
    pub params: Vec<ParamState>,
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
    /// The full state, first reply on a subscription.
    Snapshot(State),
    /// One event on a subscription (envelope id 0).
    Event(Event),
    Status(EngineStatus),
    /// A mutation succeeded; `version` is the state version after it.
    Applied {
        version: u64,
    },
    /// `AddDevice` succeeded over the pipe: the new slots, and the version after it.
    Added {
        ids: Vec<u32>,
        version: u64,
    },
    Plugins(Vec<PluginInfo>),
}

/// Live engine numbers: in snapshots and in every telemetry event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineStatus {
    /// The master clock's binding, e.g. `asio:GoXLR ASIO Driver`, or `internal`.
    pub master: String,
    pub sample_rate: f64,
    pub block: u32,
    /// Engine blocks run.
    pub blocks: u64,
    /// Smoothed fraction (0..=1) of the block period spent processing.
    pub dsp_load: f32,
    /// Sum of all slots' under- and overruns.
    pub xruns: u64,
}

/// Everything a subscriber needs to rebuild the engine's state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u64,
    pub status: EngineStatus,
    /// Sorted by id.
    pub slots: Vec<SlotState>,
    /// Sorted by (input, output).
    pub points: Vec<PointState>,
    /// Devices that can be added.
    pub devices: Vec<DeviceInfo>,
    pub notices: Vec<String>,
    /// CLAP plugins found on this PC, sorted by name.
    pub plugins: Vec<PluginInfo>,
    /// CLAP files that failed the load check: (path, why).
    pub bad_plugins: Vec<(String, String)>,
    /// The plugin on each insert bus that has one, sorted by bus id.
    pub bus_plugins: Vec<LoadedPlugin>,
}

/// One difference between two published states.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Change {
    /// Added or modified.
    PointSet(PointState),
    PointRemoved {
        input: u32,
        output: u32,
    },
    SlotAdded(SlotState),
    /// Same id, some field changed (e.g. `online`).
    SlotChanged(SlotState),
    SlotRemoved {
        id: u32,
    },
    DevicesChanged(Vec<DeviceInfo>),
    NoticesChanged(Vec<String>),
    /// The discovered plugins and the files that failed the load check.
    PluginsChanged(Vec<PluginInfo>, Vec<(String, String)>),
    /// A bus's plugin was loaded, replaced, or changed other than by values.
    BusPluginSet(LoadedPlugin),
    BusPluginRemoved {
        bus: u32,
    },
    /// Only a parameter's value (and its text) changed.
    ParamChanged {
        bus: u32,
        id: u32,
        value: f64,
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Versioned: `version` is the previous version + 1.
    Changed { version: u64, changes: Vec<Change> },
    /// Unversioned, about 10 Hz.
    Telemetry { status: EngineStatus, health: Vec<SlotHealth> },
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

/// As [`read_envelope`], but accepts any protocol version from `oldest` up to
/// [`API_VERSION`]. The journal uses it, so files written by an older engine
/// still replay (commands are only ever appended, never reordered).
pub fn read_envelope_since<R: Read, T: for<'de> Deserialize<'de>>(
    r: &mut R,
    oldest: u16,
) -> Result<Option<Envelope<T>>, FrameError> {
    match read_frame::<R, Envelope<T>>(r)? {
        Some(env) if env.version < oldest || env.version > API_VERSION => Err(FrameError::Version(env.version)),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_commands_come_after_the_older_ones() {
        let first = |c: &Command| postcard::to_allocvec(c).unwrap()[0];
        assert_eq!(first(&Command::Status), 10);
        let bus = BusRef::Id(3);
        let cmds = [
            Command::AddBus { name: "b".into(), channels: 2, first_input: None, first_output: None },
            Command::ListPlugins,
            Command::LoadPlugin { bus, path: "p.clap".into(), plugin_id: "x".into() },
            Command::UnloadPlugin { bus },
            Command::SetParam { bus, param: 1, value: -6.0 },
            Command::SetPluginState { bus: BusRef::At(8), state: vec![1, 2, 3] },
        ];
        for (n, c) in cmds.iter().enumerate() {
            assert_eq!(first(c), 11 + n as u8, "{c:?}");
            let bytes = postcard::to_allocvec(c).unwrap();
            assert_eq!(&postcard::from_bytes::<Command>(&bytes).unwrap(), c);
        }
        assert!(cmds[2].is_mutation() && cmds[3].is_mutation() && cmds[4].is_mutation() && cmds[5].is_mutation());
        assert!(!cmds[1].is_mutation());
    }

    #[test]
    fn add_bus_round_trips_and_older_variants_keep_their_index() {
        let cmd = Command::AddBus { name: "Reverb".into(), channels: 2, first_input: Some(4), first_output: None };
        let bytes = postcard::to_allocvec(&cmd).unwrap();
        assert_eq!(postcard::from_bytes::<Command>(&bytes).unwrap(), cmd);
        // Older variants keep their index (Status is 10) and AddBus comes after
        // them, so old journals and clients decode unchanged.
        assert_eq!(postcard::to_allocvec(&Command::Status).unwrap(), vec![10]);
        assert_eq!(bytes[0], 11);
    }

    #[test]
    fn a_bus_slot_is_recognised_by_its_device() {
        let s = SlotState {
            id: 1,
            name: "Reverb".into(),
            device: BUS_DEVICE.into(),
            role: ClockRole::Strict,
            online: true,
            first_input: 0,
            inputs: 2,
            first_output: 0,
            outputs: 2,
        };
        assert!(s.is_bus());
        assert!(!SlotState { device: "vasio:1".into(), ..s }.is_bus());
    }

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
