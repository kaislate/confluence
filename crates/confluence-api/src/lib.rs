//! The Confluence Control API: typed, versioned requests and responses, plus
//! length-prefixed `postcard` framing used on the named pipe and in the journal.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

mod state;
pub mod taper;
pub use state::diff;

/// Protocol version. Bump the major part for incompatible changes.
pub const API_VERSION: u16 = 9;

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
    /// Opens the editor of a bus's plugin (the engine owns the window), or
    /// brings it to the front.
    ShowEditor {
        bus: BusRef,
    },
    /// Closes the editor of a bus's plugin.
    HideEditor {
        bus: BusRef,
    },
    /// Captures the current routes and plugin parameters as scene `name`
    /// (replacing a scene of that name), with a morph time.
    SaveScene {
        name: String,
        morph_ms: u32,
    },
    /// Stores a scene as given (the journal's form of `SaveScene`).
    PutScene {
        scene: Scene,
    },
    DeleteScene {
        name: String,
    },
    SetSceneMorph {
        name: String,
        morph_ms: u32,
    },
    /// Glides to scene `name` over its morph time.
    RecallScene {
        name: String,
    },
    /// Replies `Scenes`.
    ListScenes,
    /// The next CC that arrives binds that control to this route's gain.
    LearnMidi {
        input: u32,
        output: u32,
    },
    CancelMidiLearn,
    /// Binds a control to a route's gain (replacing that control's binding).
    SetMidiBinding {
        binding: MidiBinding,
    },
    RemoveMidiBinding {
        device: String,
        channel: u8,
        cc: u8,
    },
    /// Handles `bytes` as a MIDI message received from `device` (diagnostics, tests).
    InjectMidi {
        device: String,
        bytes: Vec<u8>,
    },
    /// Stores a Luau script (replacing one with that name) and (re)starts it if enabled.
    SetScript {
        name: String,
        source: String,
        enabled: bool,
    },
    DeleteScript {
        name: String,
    },
    /// Colours slot `id`'s device (all its slots), or back to the default
    /// with `None`. Saved as [`Command::SetColor`] for the device.
    SetSlotColor {
        id: u32,
        color: Option<Rgb>,
    },
    /// Colours whatever has colour key `key` (see [`SlotState::color`]): what
    /// the journal keeps, since slot ids change between runs.
    SetColor {
        key: String,
        color: Option<Rgb>,
    },
}

/// A colour: red, green, blue.
pub type Rgb = [u8; 3];

/// Largest script source accepted.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// Largest total of all scripts' sources (state must fit a message).
pub const MAX_SCRIPTS_BYTES: usize = 512 * 1024;
/// Longest script name, in characters.
pub const MAX_SCRIPT_NAME: usize = 64;

/// Why a script can't be stored, if it can't: `others` is the size of every
/// other script (not one it replaces).
pub fn script_problem(name: &str, source: &str, others: usize) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some("a script needs a name".into());
    }
    if name.chars().count() > MAX_SCRIPT_NAME {
        return Some(format!("script names are at most {MAX_SCRIPT_NAME} characters"));
    }
    if name.chars().any(char::is_control) {
        return Some("script names are one line of text".into());
    }
    if source.len() > MAX_SCRIPT_BYTES {
        return Some("scripts are at most 256 KB".into());
    }
    if others + source.len() > MAX_SCRIPTS_BYTES {
        return Some("all scripts together are at most 512 KB".into());
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptStatus {
    Running,
    /// It failed to load, raised an error or ran out of time; saving or
    /// enabling it again starts it.
    Stopped(String),
    Disabled,
}

/// A Luau script as clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptInfo {
    pub name: String,
    pub source: String,
    pub enabled: bool,
    pub status: ScriptStatus,
    /// Its last log lines (`confluence.log`, `print`), oldest first.
    pub log: Vec<String>,
}

/// A hardware control (a CC on a channel of a MIDI input) bound to a route's gain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiBinding {
    /// The MIDI input's name.
    pub device: String,
    /// 1 to 16.
    pub channel: u8,
    pub cc: u8,
    /// The route, by its channels.
    pub input: u32,
    pub output: u32,
}

/// Most a morph can last.
pub const MAX_MORPH_MS: u32 = 10_000;

/// A saved mix: route values and plugin parameter values (parameter-only:
/// recalling it creates or removes nothing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    pub morph_ms: u32,
    pub points: Vec<PointState>,
    pub params: Vec<SceneParam>,
}

/// One plugin parameter value in a scene; the bus is named by its first send column.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneParam {
    pub bus_at: u32,
    pub param: u32,
    pub value: f64,
}

/// What clients see of a scene.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneInfo {
    pub name: String,
    pub morph_ms: u32,
    pub routes: u32,
    pub params: u32,
}

/// Which insert bus a command means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
                | Command::SaveScene { .. }
                | Command::PutScene { .. }
                | Command::DeleteScene { .. }
                | Command::SetSceneMorph { .. }
                | Command::RecallScene { .. }
                | Command::SetMidiBinding { .. }
                | Command::RemoveMidiBinding { .. }
                | Command::SetScript { .. }
                | Command::DeleteScript { .. }
                | Command::SetSlotColor { .. }
                | Command::SetColor { .. }
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
    /// The colour chosen for this slot's device (`None`: the default). A
    /// device's slots share it; it is kept per device (an insert bus: per its
    /// first send column).
    #[serde(default)]
    pub color: Option<Rgb>,
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
    /// For a network receive stream: what arrived.
    #[serde(default)]
    pub net: Option<NetStats>,
}

/// What a network receive stream has seen since it was added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStats {
    pub packets: u64,
    /// Never arrived (concealed).
    pub lost: u64,
    /// Arrived after their place was concealed (dropped).
    pub late: u64,
    /// Arrived out of order and were put back in order.
    pub reordered: u64,
    /// Could not be read (dropped).
    pub malformed: u64,
    /// Time since the last packet this stream could play, in ms.
    pub silent_ms: u64,
    /// Readable, but at a sample rate or block size this stream does not take
    /// (dropped): the sender changed; add the stream again.
    #[serde(default)]
    pub mismatched: u64,
}

/// Another Confluence engine found on the network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub address: String,
    pub port: u16,
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
    /// Audio sent to another Confluence engine. `name` is
    /// `<peer>/<stream>[:<channels>]`; `<peer>` is an engine name or an IPv4
    /// address (with an optional `:port`).
    NetSend,
    /// Audio received from another Confluence engine, `<peer>/<stream>[:<channels>]`.
    NetReceive,
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
            DeviceKind::NetSend => "net-out",
            DeviceKind::NetReceive => "net-in",
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
            DeviceKind::NetSend,
            DeviceKind::NetReceive,
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
    /// The plugin has an editor of its own.
    pub has_editor: bool,
    /// Its editor is open.
    pub editor_open: bool,
    pub params: Vec<ParamState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    pub name: String,
    pub inputs: u32,
    pub outputs: u32,
}

// A full `State` (a subscription's first reply) is much larger than the other
// replies; replies are built once and moved rarely, so it is not boxed.
#[allow(clippy::large_enum_variant)]
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
    Scenes(Vec<SceneInfo>),
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
    /// Scenes, in the order they were made.
    pub scenes: Vec<SceneInfo>,
    /// The scene last recalled, until something else changes the mix.
    pub current_scene: Option<String>,
    /// A recall is gliding to its scene.
    pub morphing: bool,
    /// MIDI inputs open now.
    pub midi_inputs: Vec<String>,
    pub midi_bindings: Vec<MidiBinding>,
    /// The route waiting for a control to be moved (MIDI Learn).
    pub midi_learning: Option<(u32, u32)>,
    /// Luau scripts, sorted by name.
    pub scripts: Vec<ScriptInfo>,
    /// Other Confluence engines found on the network, sorted by name.
    #[serde(default)]
    pub peers: Vec<Peer>,
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
    /// The scenes, the current one, and whether a morph runs.
    ScenesChanged(Vec<SceneInfo>, Option<String>, bool),
    /// MIDI inputs, bindings, and the route being learned.
    MidiChanged(Vec<String>, Vec<MidiBinding>, Option<(u32, u32)>),
    ScriptsChanged(Vec<ScriptInfo>),
    PeersChanged(Vec<Peer>),
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
            Command::ShowEditor { bus },
            Command::HideEditor { bus },
            Command::SaveScene { name: "Verse".into(), morph_ms: 500 },
            Command::PutScene {
                scene: Scene {
                    name: "Verse".into(),
                    morph_ms: 500,
                    points: vec![PointState { input: 1, output: 2, gain_db: -3.0, mute: false, invert: false }],
                    params: vec![SceneParam { bus_at: 8, param: 1, value: -6.0 }],
                },
            },
            Command::DeleteScene { name: "Verse".into() },
            Command::SetSceneMorph { name: "Verse".into(), morph_ms: 2000 },
            Command::RecallScene { name: "Verse".into() },
            Command::ListScenes,
            Command::LearnMidi { input: 1, output: 2 },
            Command::CancelMidiLearn,
            Command::SetMidiBinding {
                binding: MidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7, input: 1, output: 2 },
            },
            Command::RemoveMidiBinding { device: "nanoKONTROL2".into(), channel: 1, cc: 7 },
            Command::InjectMidi { device: "nanoKONTROL2".into(), bytes: vec![0xB0, 7, 100] },
            Command::SetScript { name: "mute".into(), source: "-- hi".into(), enabled: true },
            Command::DeleteScript { name: "mute".into() },
        ];
        for (n, c) in cmds.iter().enumerate() {
            assert_eq!(first(c), 11 + n as u8, "{c:?}");
            let bytes = postcard::to_allocvec(c).unwrap();
            assert_eq!(&postcard::from_bytes::<Command>(&bytes).unwrap(), c);
        }
        assert!(cmds[2].is_mutation() && cmds[3].is_mutation() && cmds[4].is_mutation() && cmds[5].is_mutation());
        assert!(!cmds[1].is_mutation());
        assert!(!cmds[6].is_mutation() && !cmds[7].is_mutation(), "editors are not saved");
        assert!(cmds[8..13].iter().all(Command::is_mutation), "scene commands are saved");
        assert!(!cmds[13].is_mutation(), "listing is not");
        let saved: Vec<bool> = cmds[14..].iter().map(Command::is_mutation).collect();
        assert_eq!(
            saved,
            [false, false, true, true, false, true, true],
            "learn/cancel/inject not saved; bindings, scripts are"
        );
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
    fn colour_commands_round_trip_are_saved_and_come_after_older_variants() {
        let by_slot = Command::SetSlotColor { id: 3, color: Some([0x40, 0xa0, 0xff]) };
        let by_key = Command::SetColor { key: "vasio:1".into(), color: None };
        for cmd in [&by_slot, &by_key] {
            let bytes = postcard::to_allocvec(cmd).unwrap();
            assert_eq!(&postcard::from_bytes::<Command>(&bytes).unwrap(), cmd);
            assert!(cmd.is_mutation(), "{cmd:?} is saved");
        }
        let script = postcard::to_allocvec(&Command::DeleteScript { name: String::new() }).unwrap()[0];
        assert!(postcard::to_allocvec(&by_slot).unwrap()[0] > script, "old journals decode unchanged");
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
            color: None,
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

    #[test]
    fn scripts_are_checked_against_the_limits() {
        assert_eq!(script_problem("a", "", 0), None);
        assert_eq!(script_problem(" ", "", 0), Some("a script needs a name".into()));
        assert_eq!(script_problem(&"n".repeat(65), "", 0), Some("script names are at most 64 characters".into()));
        assert_eq!(
            script_problem(
                "a
b", "", 0
            ),
            Some("script names are one line of text".into())
        );
        let big = "-".repeat(MAX_SCRIPT_BYTES + 1);
        assert_eq!(script_problem("a", &big, 0), Some("scripts are at most 256 KB".into()));
        let half = "-".repeat(MAX_SCRIPT_BYTES / 2);
        assert_eq!(script_problem("a", &half, MAX_SCRIPTS_BYTES - half.len()), None);
        assert_eq!(
            script_problem("a", &half, MAX_SCRIPTS_BYTES - half.len() + 1),
            Some("all scripts together are at most 512 KB".into())
        );
    }
}
