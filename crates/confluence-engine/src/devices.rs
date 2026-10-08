//! Device slots: opens ASIO, WASAPI and per-app capture devices as engine
//! slots, persists their bindings with channel ranges (machine-specific, spec
//! §15), restores missing devices as offline slots so channel numbers never
//! shift (spec §5.2, §16), and runs an ASIO device as the hardware master.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use confluence_api::{
    all_positions, ClockRole, Command, DeviceInfo, DeviceKind, PosGroup, PosId, PositionDevice, PositionState,
    PositionStatus, Response,
};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{InputDeviceSide, OutputDeviceSide};
use confluence_provider_asio::registry::installed_drivers;
use confluence_provider_asio::{AsioCallback, AsioDevice, AsioHealth, AsioHostError, AsioIo, StreamConfig, StreamInfo};
use confluence_provider_wasapi::{
    endpoints, find_endpoint, find_process, Direction, Endpoint, Handler, Target, WasapiStream,
};
use serde::{Deserialize, Serialize};

use crate::audio::AudioEngine;
use crate::audio::StrictSide;
use crate::engine::{
    ChannelMove, Engine, MasterChannels, MasterSlotSpec, OfflineSlotSpec, SoftSlotSpec, StrictSlotSpec, StrictStats,
};
use crate::positions::{is_own_vasio_driver, next_free, vasio_device_name, PositionTable, VirtualState};
use confluence_core::buffer::PlanarBuffer;
use confluence_provider_vaio::{VaioSlot, VaioStats};
use confluence_provider_vasio::config::InstanceConfig;
use confluence_provider_vasio::{VasioSlot, VasioStats};
use std::sync::Arc;

use confluence_net::discovery::Discovery;
use confluence_net::host::{NetHost, ReceiveHandle, SendHandle, SendSide, SendSpec};

/// A device bound to slots, with the channel ranges it occupies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub kind: DeviceKind,
    pub name: String,
    pub first_input: u32,
    pub inputs: u32,
    pub first_output: u32,
    pub outputs: u32,
    /// A WASAPI endpoint's id: unlike its name, unique even for two identical
    /// USB devices. Tried first when the binding is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    /// A network receive stream's sample rate: it comes back at it before the
    /// stream is heard again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
}

impl Binding {
    fn device(&self) -> String {
        format!("{}:{}", self.kind.prefix(), self.name)
    }
}

/// An open device: its `Drop` stops the stream; its health feeds `annotate`.
/// (A VASIO slot itself lives on the audio thread; its handle is its counters.)
enum Handle {
    Vasio(#[allow(dead_code)] Arc<VasioStats>),
    Vaio(#[allow(dead_code)] Arc<VaioStats>),
    Asio(AsioDevice),
    Wasapi(WasapiStream),
    NetSend(#[allow(dead_code)] Arc<SendHandle>),
    NetReceive(ReceiveHandle),
}

struct Bound {
    binding: Binding,
    /// The fixed position this device fills (spec: slot model).
    pos: Option<PosId>,
    slots: Vec<u32>,
    /// Empty while the device is missing (offline).
    handles: Vec<Handle>,
}

/// What `devices.json` holds: the master's placement and every device slot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub master: Option<Binding>,
    #[serde(default)]
    pub devices: Vec<Binding>,
}

/// `devices.json` version 2: devices by position (spec: slot model §3).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SavedV2 {
    version: u32,
    #[serde(default)]
    master: Option<PosId>,
    /// The ASIO master's channel placement.
    #[serde(default)]
    master_binding: Option<Binding>,
    #[serde(default)]
    positions: Vec<SavedPosition>,
    #[serde(default)]
    unplaced: Vec<Binding>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedPosition {
    pos: PosId,
    /// Virtual positions only: switched on, and their (inputs, outputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shape: Option<(u32, u32)>,
    /// The device and its channel placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<Binding>,
}

/// Opens an ASIO driver by name (injectable so tests can use fake drivers).
pub type AsioOpener = Box<dyn Fn(&str) -> Result<AsioDevice, AsioHostError> + Send + Sync>;

/// A device loaded but not yet attached to the engine.
enum Loaded {
    Asio(AsioDevice),
    /// The stream, and the endpoint id (None for per-app capture).
    Wasapi(WasapiStream, Option<String>),
    /// Nothing slow to do: the engine side is created when attached.
    Vasio,
    /// As VASIO: the driver is attached when the slot is created.
    Vaio,
    /// A network stream (the slot opens it), and its peer's address if the
    /// name had to be looked up (discovery is asked again when it is attached).
    Net(NetName, Option<std::net::SocketAddr>),
}

/// One direction of a placement: its first channel, and the move its routes make.
type DirectionPlace = (Option<u32>, Option<(u32, u32, u32)>);

/// A loaded device's (inputs, outputs), where known before it is attached.
fn loaded_shape(kind: DeviceKind, name: &str, loaded: &Loaded) -> Option<(u32, u32)> {
    match loaded {
        Loaded::Asio(dev) => Some((dev.info().inputs() as u32, dev.info().outputs() as u32)),
        Loaded::Wasapi(stream, _) => {
            let f = stream.format();
            let ch = f.channels as u32;
            Some(if f.direction == Direction::Render { (0, ch) } else { (ch, 0) })
        }
        Loaded::Vasio => parse_vasio(name).ok().map(|(_, daw_in, daw_out)| (daw_out as u32, daw_in as u32)),
        // A stream named with its channel count (a receive learns it only when heard).
        Loaded::Net(n, _) => {
            n.channels.map(|c| if kind == DeviceKind::NetSend { (0, c as u32) } else { (c as u32, 0) })
        }
        Loaded::Vaio => None,
    }
}

/// VASIO instance `n` (1..=8) as its position (A..=H).
fn vasio_pos(n: u32) -> Result<PosId, String> {
    if !(1..=confluence_provider_vasio::INSTANCES).contains(&n) {
        return Err(format!("there is no VASIO instance {n} (1 to {})", confluence_provider_vasio::INSTANCES));
    }
    Ok(PosId { group: PosGroup::Vasio, index: (n - 1) as u8 })
}

/// The program on a VASIO instance, if its driver named it.
fn handle_client(h: &Handle) -> Option<String> {
    match h {
        Handle::Vasio(s) => s.client_name(),
        _ => None,
    }
}

/// A virtual device's program is connected (a DAW on VASIO, an app on VAIO).
fn handle_connected(h: &Handle) -> bool {
    match h {
        Handle::Vasio(s) => s.connected.load(Ordering::Relaxed),
        Handle::Vaio(s) => s.streaming.load(Ordering::Relaxed),
        _ => false,
    }
}

/// Finds a WASAPI endpoint by its saved id, else by name.
fn find_saved_endpoint(direction: Direction, name: &str, id: Option<&str>) -> Result<Endpoint, String> {
    if let Some(ep) = id.and_then(|id| find_endpoint(direction, id).ok()) {
        return Ok(ep);
    }
    find_endpoint(direction, name).map_err(|e| e.to_string())
}

/// Loads a device: the slow part of adding one (driver `init`, endpoint or
/// process lookup, stream set-up). Needs neither the engine nor the manager.
fn load(
    opener: &AsioOpener,
    kind: DeviceKind,
    name: &str,
    endpoint_id: Option<&str>,
    net: Option<&NetLookupSnapshot>,
) -> Result<Loaded, String> {
    Ok(match kind {
        DeviceKind::Asio => Loaded::Asio(opener(name).map_err(|e| e.to_string())?),
        DeviceKind::Vasio => {
            parse_vasio(name)?;
            Loaded::Vasio
        }
        DeviceKind::Vaio => {
            parse_vaio(name)?;
            Loaded::Vaio
        }
        DeviceKind::NetSend | DeviceKind::NetReceive => {
            let n = parse_net(name)?;
            let looked_up = net.and_then(|net| net.look_up(&n));
            Loaded::Net(n, looked_up)
        }
        DeviceKind::WasapiRender | DeviceKind::WasapiCapture | DeviceKind::AppCapture => {
            let target = match kind {
                DeviceKind::WasapiRender => {
                    let ep = find_saved_endpoint(Direction::Render, name, endpoint_id)?;
                    Target::Endpoint { id: ep.id, direction: Direction::Render }
                }
                DeviceKind::WasapiCapture => {
                    let ep = find_saved_endpoint(Direction::Capture, name, endpoint_id)?;
                    Target::Endpoint { id: ep.id, direction: Direction::Capture }
                }
                _ => Target::App { pid: find_process(name).map_err(|e| e.to_string())? },
            };
            let id = match &target {
                Target::Endpoint { id, .. } => Some(id.clone()),
                Target::App { .. } => None,
            };
            Loaded::Wasapi(WasapiStream::open(target).map_err(|e| e.to_string())?, id)
        }
    })
}

/// A device being added, between [`DeviceManager::begin_add`] and
/// [`DeviceManager::finish_add`]. Its [`load`](Self::load) is the slow part:
/// run it without holding the engine's lock.
pub struct PendingAdd {
    kind: DeviceKind,
    name: String,
    pos: Option<PosId>,
    /// An offline device's saved endpoint id, tried before its name.
    endpoint_id: Option<String>,
    opener: Arc<AsioOpener>,
    net: Option<NetLookupSnapshot>,
}

impl PendingAdd {
    /// Never panics: a driver or lookup that panics becomes an error, so the
    /// device is not left reserved as "being opened".
    pub fn load(self) -> LoadedAdd {
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            load(&self.opener, self.kind, &self.name, self.endpoint_id.as_deref(), self.net.as_ref())
        }));
        let device = format!("{}:{}", self.kind.prefix(), self.name);
        let loaded = opened.unwrap_or_else(|_| Err(format!("opening {device} panicked")));
        LoadedAdd { kind: self.kind, name: self.name, pos: self.pos, loaded }
    }
}

/// An ASIO driver wired to its slots but not yet started.
struct PendingStart {
    dev: AsioDevice,
    callback: Box<dyn AsioCallback>,
    block: usize,
}

/// A device attached to the engine but not started yet (from
/// [`DeviceManager::attach_add`]). Its [`start`](Self::start) can be slow (an
/// ASIO driver's createBuffers and start): run it without the engine lock.
pub struct AttachedAdd {
    kind: DeviceKind,
    name: String,
    pos: Option<PosId>,
    offline: Option<Binding>,
    bound: Bound,
    start: Option<PendingStart>,
}

impl AttachedAdd {
    /// The engine slots the device will stream through.
    pub fn slots(&self) -> &[u32] {
        &self.bound.slots
    }

    /// Starts the device. Never panics: a failure is reported by
    /// [`DeviceManager::commit_add`], which also undoes the slots.
    pub fn start(mut self) -> StartedAdd {
        let result = match self.start.take() {
            None => Ok(()),
            Some(PendingStart { mut dev, callback, block }) => {
                let cfg = StreamConfig { sample_rate: None, block: Some(block) };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dev.start(cfg, callback))) {
                    Ok(Ok(_)) => {
                        self.bound.handles.push(Handle::Asio(dev));
                        Ok(())
                    }
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("starting the device panicked".into()),
                }
            }
        };
        StartedAdd { kind: self.kind, name: self.name, pos: self.pos, offline: self.offline, bound: self.bound, result }
    }
}

/// The result of [`AttachedAdd::start`], for [`DeviceManager::commit_add`].
pub struct StartedAdd {
    kind: DeviceKind,
    name: String,
    pos: Option<PosId>,
    offline: Option<Binding>,
    bound: Bound,
    result: Result<(), String>,
}

/// The result of [`PendingAdd::load`], for [`DeviceManager::finish_add`].
pub struct LoadedAdd {
    kind: DeviceKind,
    name: String,
    pos: Option<PosId>,
    loaded: Result<Loaded, String>,
}

/// Looks up a computer's name (and port) as the system resolves names (DNS,
/// LLMNR, mDNS): for engines discovery has not found.
pub type NetLookup = Arc<dyn Fn(&str, u16) -> Option<std::net::SocketAddr> + Send + Sync>;

/// The engine's network audio: its host and how it finds other engines.
pub struct NetCtx {
    pub host: NetHost,
    pub discovery: Box<dyn Discovery>,
    lookup: NetLookup,
}

impl NetCtx {
    pub fn new(host: NetHost, discovery: Box<dyn Discovery>) -> NetCtx {
        NetCtx { host, discovery, lookup: Arc::new(system_lookup) }
    }

    /// Looks names up with `lookup` instead of the system's resolver (tests).
    pub fn with_lookup(mut self, lookup: NetLookup) -> NetCtx {
        self.lookup = lookup;
        self
    }

    fn snapshot(&self) -> NetLookupSnapshot {
        NetLookupSnapshot { peers: self.discovery.peers(), lookup: self.lookup.clone() }
    }
}

/// The first IPv4 address the system resolves `name` to.
fn system_lookup(name: &str, port: u16) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    (name, port).to_socket_addrs().ok()?.find(|a| a.is_ipv4())
}

/// What a stream's peer may be looked up with while it is loaded (without the
/// engine lock: a lookup can take seconds).
pub struct NetLookupSnapshot {
    peers: Vec<confluence_api::Peer>,
    lookup: NetLookup,
}

impl NetLookupSnapshot {
    /// The address of `n`'s peer when it is a name discovery has not found.
    fn look_up(&self, n: &NetName) -> Option<std::net::SocketAddr> {
        let known = n.peer.parse::<std::net::Ipv4Addr>().is_ok()
            || self.peers.iter().any(|p| p.name.eq_ignore_ascii_case(&n.peer));
        if known {
            return None;
        }
        (self.lookup)(&n.peer, n.port.unwrap_or(confluence_net::DEFAULT_PORT))
    }
}

pub struct DeviceManager {
    /// Network audio (`None`: off).
    net: Option<NetCtx>,
    bound: Vec<Bound>,
    master: Option<Binding>,
    /// Name of the ASIO driver running as the master, known before restore.
    master_name: Option<String>,
    /// Bindings read at startup, not yet restored.
    saved: Saved,
    path: Option<PathBuf>,
    asio_open: Arc<AsioOpener>,
    /// Devices between `begin_add` and `finish_add`.
    loading: Vec<(DeviceKind, String)>,
    /// Slots of devices between `attach_add` and `commit_add`.
    attaching: Vec<u32>,
    quality: AsrcQuality,
    /// Where VASIO shapes are remembered for the DLL (`None`: not at all).
    vasio_config_root: Option<String>,
    /// Saved bindings whose channels could not be reserved this run: kept so
    /// they are written back and tried again next time.
    unplaced: Vec<Binding>,
    /// Why the bindings file, which exists, is never overwritten this session
    /// (so device changes are not saved); `None` when saving works.
    save_blocked: Option<&'static str>,
    /// The ASIO master's slot and driver health, for `annotate`.
    master_health: Option<(u32, Arc<AsioHealth>)>,
    /// Offline network streams that failed to come back, and when to try again.
    net_retry_failed: Vec<(String, Instant)>,
    /// Virtual positions' on/off and shape, and the master flag.
    table: PositionTable,
    /// Devices read at startup with their positions, not yet restored.
    restore_list: Vec<(Option<PosId>, Binding)>,
    /// The last channel placement of each virtual position (kept while off).
    virtual_at: BTreeMap<PosId, Binding>,
    /// Positions held for devices being opened.
    reserved: Vec<(DeviceKind, String, PosId)>,
    /// What a migration from a version-1 file moved (shown for this run).
    migration_notes: Vec<String>,
    /// Colour keys a migration changed (old, new), for the engine's colours.
    color_rekeys: Vec<(String, String)>,
    /// The position of the master running now (the flag may name another
    /// for the next start).
    master_now: Option<PosId>,
}

impl DeviceManager {
    /// A manager that does not persist bindings.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            net: None,
            bound: Vec::new(),
            master: None,
            master_name: None,
            saved: Saved::default(),
            path,
            asio_open: Arc::new(Box::new(AsioDevice::open_installed)),
            loading: Vec::new(),
            attaching: Vec::new(),
            quality: AsrcQuality::Sinc64,
            vasio_config_root: Some(confluence_provider_vasio::config::root()),
            unplaced: Vec::new(),
            save_blocked: None,
            master_health: None,
            net_retry_failed: Vec::new(),
            table: Self::all_off(),
            restore_list: Vec::new(),
            virtual_at: BTreeMap::new(),
            reserved: Vec::new(),
            migration_notes: Vec::new(),
            color_rekeys: Vec::new(),
            master_now: None,
        }
    }

    /// A table with every virtual position off: a manager that does not
    /// persist (tests) never opens VASIO by surprise.
    fn all_off() -> PositionTable {
        let mut t = PositionTable::new_default();
        let _ = t.set_virtual(PosId { group: PosGroup::Vasio, index: 0 }, false, None);
        t
    }

    /// Reads saved bindings from `path`. A corrupt file is renamed to `.bad`
    /// (never silently discarded) and reported as a warning. A file that
    /// cannot be read, or cannot be moved aside, is never overwritten: this
    /// session's device changes are then not saved.
    pub fn open_file(path: PathBuf) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut save_blocked = None;
        let mut v2: Option<SavedV2> = None;
        let mut v1_file = false;
        let saved = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => {
                warnings.push(format!(
                    "could not read {}: {e}; it is left untouched and device changes will not be saved",
                    path.display()
                ));
                save_blocked = Some("could not be read at start-up");
                Saved::default()
            }
            Ok(text)
                if serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("version").and_then(|n| n.as_u64()))
                    == Some(2) =>
            {
                match serde_json::from_str::<SavedV2>(&text) {
                    Ok(v) => {
                        v2 = Some(v);
                        Saved::default()
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "{} is not valid ({e}): it is left untouched and device changes will not be saved",
                            path.display()
                        ));
                        save_blocked = Some("is not valid");
                        Saved::default()
                    }
                }
            }
            Ok(text) => serde_json::from_str(&text).inspect(|_| v1_file = true).unwrap_or_else(|e| {
                let bad = path.with_extension("bad");
                match std::fs::rename(&path, &bad) {
                    Ok(()) => warnings.push(format!(
                        "{} is not valid ({e}); kept as {} and starting with no devices",
                        path.display(),
                        bad.display()
                    )),
                    Err(re) => {
                        warnings.push(format!(
                            "{} is not valid ({e}) and could not be moved aside ({re}); it is left in place, \
                             starting with no devices, and device changes will not be saved",
                            path.display()
                        ));
                        save_blocked = Some("is not valid and could not be moved aside");
                    }
                }
                Saved::default()
            }),
        };
        let mut m = Self::new(Some(path));
        // A persisted setup starts with VASIO A on (unless the file says otherwise).
        m.table = PositionTable::new_default();
        m.restore_list = saved.devices.iter().cloned().map(|b| (None, b)).collect();
        m.saved = saved;
        if let Some(v) = v2 {
            let _ = m.table.set_master(v.master);
            m.saved.master = v.master_binding;
            m.unplaced = v.unplaced;
            for sp in v.positions {
                if sp.pos.group.is_virtual() {
                    let on = sp.on.unwrap_or(false);
                    let _ = m.table.set_virtual(sp.pos, on, sp.shape);
                    if let Some(b) = sp.binding {
                        m.virtual_at.insert(sp.pos, b.clone());
                        if on {
                            m.restore_list.push((Some(sp.pos), b));
                        }
                    }
                } else if let Some(b) = sp.binding {
                    m.restore_list.push((Some(sp.pos), b));
                }
            }
        }
        m.save_blocked = save_blocked;
        if v1_file && m.save_blocked.is_none() {
            m.migrate(&mut warnings);
        }
        (m, warnings)
    }

    /// Moves a version-1 setup into positions (spec: slot model §3), after
    /// backing the file up; a setup that cannot be backed up is not touched.
    fn migrate(&mut self, warnings: &mut Vec<String>) {
        let Some(path) = self.path.clone() else { return };
        if let Err(e) = crate::migrate::backup(&path) {
            warnings.push(format!("{} could not be backed up ({e}): it was not migrated", path.display()));
            self.save_blocked = Some("could not be backed up, so it was not migrated");
            return;
        }
        let master = self.saved.master.clone();
        let devices = std::mem::take(&mut self.restore_list).into_iter().map(|(_, b)| b).collect();
        let m = crate::migrate::migrate_v1(master, devices);
        let _ = self.table.set_master(m.saved.master);
        self.saved.master = m.saved.master_binding;
        for (pos, virt, binding) in m.saved.positions {
            match virt {
                Some((on, shape)) => {
                    let _ = self.table.set_virtual(pos, on, Some(shape));
                    if let Some(b) = binding {
                        self.virtual_at.insert(pos, b.clone());
                        if on {
                            self.restore_list.push((Some(pos), b));
                        }
                    }
                }
                None => {
                    if let Some(b) = binding {
                        self.restore_list.push((Some(pos), b));
                    }
                }
            }
        }
        self.unplaced.extend(m.saved.unplaced);
        self.migration_notes = m.notes;
        self.color_rekeys = m.color_keys;
        if let Err(e) = self.save() {
            warnings.push(format!("the migrated setup could not be saved: {e}"));
        }
    }

    /// Colour keys the migration changed (taken once, by the engine's start-up).
    pub fn take_color_rekeys(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.color_rekeys)
    }

    /// Saved channel placement (first input, first output) of the master `name`, if any.
    pub fn saved_master(&self, name: &str) -> Option<(u32, u32)> {
        let saved = self.saved.master.as_ref().filter(|b| b.name == name);
        // A device made master since: its binding at the master position.
        let made = || {
            let p = self.table.master()?;
            self.restore_list
                .iter()
                .find(|(q, b)| *q == Some(p) && b.kind == DeviceKind::Asio && b.name == name)
                .map(|(_, b)| b)
        };
        saved.or_else(made).map(|b| (b.first_input, b.first_output))
    }

    /// Declares which ASIO driver is the master, before [`restore`](Self::restore):
    /// a saved soft-slot binding of the same driver is then dropped instead of
    /// loading the driver a second time.
    pub fn claim_master(&mut self, name: &str) {
        self.master_name = Some(name.to_string());
    }

    /// Records the running master's placement so it is reused next time.
    pub fn set_master(&mut self, name: &str, ch: MasterChannels) -> Result<(), String> {
        self.master_name = Some(name.to_string());
        if self.table.master().is_none() {
            // A master given on the command line takes the first free ASIO position.
            let _ = self.table.set_master(next_free(PosGroup::Asio, &self.taken()));
        }
        self.master_now = self.table.master();
        // Its binding as an ordinary device (from when it was not master) is done with.
        let at = self.master_now;
        self.restore_list.retain(|(q, b)| !(*q == at && b.kind == DeviceKind::Asio && b.name == name));
        self.master = Some(Binding {
            kind: DeviceKind::Asio,
            name: name.to_string(),
            first_input: ch.first_input as u32,
            inputs: ch.inputs as u32,
            first_output: ch.first_output as u32,
            outputs: ch.outputs as u32,
            endpoint_id: None,
            rate: None,
        });
        self.save()
    }

    pub fn with_asio_opener(mut self, opener: AsioOpener) -> Self {
        self.asio_open = Arc::new(opener);
        self
    }

    /// Where VASIO shapes are saved for the DLL; tests pass a scratch key (or `None`).
    /// Turns network audio on.
    pub fn with_net(mut self, net: NetCtx) -> Self {
        self.net = Some(net);
        self
    }

    /// Other engines found on the network.
    pub fn peers(&self) -> Vec<confluence_api::Peer> {
        self.net.as_ref().map(|n| n.discovery.peers()).unwrap_or_default()
    }

    /// Network streams that can be added: one send per engine found, one
    /// receive per stream heard (`<engine or address>/<stream>`).
    pub fn net_devices(&self) -> Vec<DeviceInfo> {
        let Some(net) = &self.net else { return Vec::new() };
        let peers = net.discovery.peers();
        let mut out: Vec<DeviceInfo> = peers
            .iter()
            .map(|p| DeviceInfo { kind: DeviceKind::NetSend, name: p.name.clone(), inputs: 0, outputs: 0 })
            .collect();
        for h in net.host.heard() {
            let from = h.from.to_string();
            let label = peers.iter().find(|p| p.address == from).map_or(from, |p| p.name.clone());
            out.push(DeviceInfo {
                kind: DeviceKind::NetReceive,
                name: format!("{label}/{}", h.stream),
                inputs: h.channels as u32,
                outputs: 0,
            });
        }
        out
    }

    pub fn with_vasio_config_root(mut self, root: Option<String>) -> Self {
        self.vasio_config_root = root;
        self
    }

    /// Devices that can be opened. ASIO channel counts are unknown until a
    /// driver is loaded, so they are reported as 0; VASIO instances are
    /// listed by number.
    pub fn list(&self) -> Result<Vec<DeviceInfo>, String> {
        Self::list_devices()
    }

    /// As [`list`](Self::list), without the manager: enumeration can be slow,
    /// so the engine runs it without holding its lock.
    pub fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        let mut out: Vec<DeviceInfo> = installed_drivers()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|d| is_own_vasio_driver(&d.name).is_none())
            .map(|d| DeviceInfo { kind: DeviceKind::Asio, name: d.name, inputs: 0, outputs: 0 })
            .collect();
        for (dir, kind) in
            [(Direction::Render, DeviceKind::WasapiRender), (Direction::Capture, DeviceKind::WasapiCapture)]
        {
            for ep in endpoints(dir).map_err(|e| e.to_string())? {
                let (inputs, outputs) =
                    if dir == Direction::Capture { (ep.channels as u32, 0) } else { (0, ep.channels as u32) };
                out.push(DeviceInfo { kind, name: ep.name, inputs, outputs });
            }
        }
        // VASIO and VAIO are positions switched on and off, not devices to pick.
        Ok(out)
    }

    /// Opens a device as slot(s) and saves the binding. A device that is
    /// already open (or is the master) is refused: many drivers misbehave when
    /// loaded twice. An offline device comes back on its saved channels.
    pub fn add(&mut self, engine: &mut Engine, kind: DeviceKind, name: &str) -> Result<Vec<u32>, String> {
        match kind {
            // Virtual devices are positions that are switched on.
            DeviceKind::Vasio => {
                let (n, daw_in, daw_out) = parse_vasio(name)?;
                let pos = vasio_pos(n)?;
                self.set_virtual(engine, pos, true, Some((daw_out as u32, daw_in as u32)))
            }
            DeviceKind::Vaio => {
                parse_vaio(name)?;
                self.set_virtual(engine, PosId { group: PosGroup::Vaio, index: 0 }, true, None)
            }
            _ => {
                let loaded = self.begin_fill(None, kind, name)?.load();
                self.finish_add(engine, loaded)
            }
        }
    }

    /// First step of adding a device: refuses one that is the master, already
    /// open or already being opened, and reserves it. Always follow with
    /// [`PendingAdd::load`] (without the engine lock) and [`finish_add`](Self::finish_add).
    pub fn begin_add(&mut self, kind: DeviceKind, name: &str) -> Result<PendingAdd, String> {
        self.begin_fill(None, kind, name)
    }

    /// As [`begin_add`](Self::begin_add), into position `pos` (`None`: the
    /// next free position of the kind). Virtual devices are switched on with
    /// [`set_virtual`](Self::set_virtual) instead.
    pub fn begin_fill(&mut self, pos: Option<PosId>, kind: DeviceKind, name: &str) -> Result<PendingAdd, String> {
        self.check_addable(kind, name)?;
        if self.loading.iter().any(|(k, n)| same_device(&binding_of(*k, n), kind, name)) {
            return Err(format!("{}:{name} is being opened", kind.prefix()));
        }
        let group = PosGroup::for_kind(kind);
        if group.is_virtual() {
            return Err(format!("{}:{name} is a virtual device: turn it on instead", kind.prefix()));
        }
        // An offline device comes back in its own position.
        let own = self.bound.iter().find(|b| same_device(&b.binding, kind, name)).and_then(|b| b.pos);
        let taken = self.taken();
        let pos = match (pos, own) {
            (Some(p), _) => {
                if p.group != group {
                    return Err(format!("{} cannot hold a {} device", p.label(), kind.prefix()));
                }
                if Some(p) != own {
                    // A filled position is swapped (see `swap`); anything else in use is refused.
                    if let Some(q) = own {
                        return Err(format!("{}:{name} is in {}: clear it first", kind.prefix(), q.label()));
                    }
                    if self.reserved.iter().any(|(_, _, r)| *r == p) {
                        return Err(format!("{} is being filled", p.label()));
                    }
                    if taken.contains(&p) && !self.bound.iter().any(|b| b.pos == Some(p)) {
                        return Err(format!("{} holds the master clock device", p.label()));
                    }
                }
                p
            }
            (None, Some(p)) => p,
            (None, None) => {
                next_free(group, &taken).ok_or_else(|| format!("all {} positions are in use", group.label()))?
            }
        };
        self.loading.push((kind, name.to_string()));
        self.reserved.push((kind, name.to_string(), pos));
        let pos = Some(pos);
        let endpoint_id =
            self.bound.iter().find(|b| same_device(&b.binding, kind, name)).and_then(|b| b.binding.endpoint_id.clone());
        let net = matches!(kind, DeviceKind::NetSend | DeviceKind::NetReceive)
            .then(|| self.net.as_ref().map(NetCtx::snapshot))
            .flatten();
        Ok(PendingAdd { kind, name: name.to_string(), pos, endpoint_id, opener: self.asio_open.clone(), net })
    }

    /// Last step of adding a device: attaches what was loaded to the engine
    /// and saves the binding. A device that failed to load is reported here.
    pub fn finish_add(&mut self, engine: &mut Engine, loaded: LoadedAdd) -> Result<Vec<u32>, String> {
        let attached = self.attach_add(engine, loaded)?;
        let started = attached.start();
        self.commit_add(engine, started)
    }

    /// Attaches what was loaded to the engine (creates its slots) without
    /// starting it; follow with [`AttachedAdd::start`] (without the engine
    /// lock) and [`commit_add`](Self::commit_add).
    pub fn attach_add(&mut self, engine: &mut Engine, loaded: LoadedAdd) -> Result<AttachedAdd, String> {
        let (kind, name) = (loaded.kind, loaded.name.clone());
        let attached = self.attach_loaded(engine, loaded);
        if attached.is_err() {
            self.end_loading(kind, &name);
        }
        // Otherwise it stays reserved until commit_add: while it starts, it
        // must not be loaded a second time.
        attached
    }

    fn end_loading(&mut self, kind: DeviceKind, name: &str) {
        self.loading.retain(|(k, n)| !(*k == kind && n == name));
        self.reserved.retain(|(k, n, _)| !(*k == kind && n == name));
    }

    /// Positions in use: filled, held for a device being opened, or the master's.
    fn taken(&self) -> Vec<PosId> {
        let mut t: Vec<PosId> = self.bound.iter().filter_map(|b| b.pos).collect();
        t.extend(self.restore_list.iter().filter_map(|(p, _)| *p));
        t.extend(self.master_now.filter(|_| self.master.is_some()));
        t.extend(self.reserved.iter().map(|(_, _, p)| *p));
        t.extend(self.table.master().filter(|_| self.master.is_some() || self.saved.master.is_some()));
        t
    }

    /// Keys a device's slot colours by its position.
    fn key_colours(engine: &mut Engine, bound: &Bound) {
        for id in &bound.slots {
            engine.set_color_key(*id, bound.pos.map(|p| format!("pos:{p}")));
        }
    }

    fn attach_loaded(&mut self, engine: &mut Engine, loaded: LoadedAdd) -> Result<AttachedAdd, String> {
        let LoadedAdd { kind, name, pos, loaded } = loaded;
        let name = name.as_str();
        self.check_addable(kind, name)?;
        let existing = self.bound.iter().position(|b| same_device(&b.binding, kind, name));
        // A device that failed to load: an offline slot keeps holding its channels.
        let loaded = loaded?;
        // Into a filled position: the device there is swapped out.
        let held =
            pos.and_then(|p| self.bound.iter().position(|b| b.pos == Some(p) && !same_device(&b.binding, kind, name)));
        if let Some(i) = held {
            let bound = self.swap(engine, i, kind, name, loaded)?;
            self.attaching.extend(bound.slots.iter().copied());
            return Ok(AttachedAdd { kind, name: name.to_string(), pos, offline: None, bound, start: None });
        }
        let offline = match existing {
            Some(i) => {
                // Offline: hand its channels back to the device, routes and all.
                let b = self.bound.remove(i);
                for id in &b.slots {
                    engine.release_offline_slot(*id).map_err(|e| e.to_string())?;
                }
                Some(b.binding)
            }
            None => None,
        };
        let (bound, start) = match self.attach(engine, kind, name, offline.as_ref(), loaded) {
            Ok(attached) => attached,
            Err(e) => {
                // Still missing: keep holding its channels.
                if let Some(b) = offline {
                    let parked = Self::park_offline(engine, b).map_err(|pe| format!("{e}; {pe}"))?;
                    self.bound.push(parked);
                }
                return Err(e);
            }
        };
        self.attaching.extend(bound.slots.iter().copied());
        Ok(AttachedAdd { kind, name: name.to_string(), pos, offline, bound, start })
    }

    /// Last step of adding a device: records a started device and saves the
    /// binding, or undoes the slots of one that failed to start (an offline
    /// device gets its channels and routes back).
    pub fn commit_add(&mut self, engine: &mut Engine, started: StartedAdd) -> Result<Vec<u32>, String> {
        let StartedAdd { kind, name, pos, offline, mut bound, result } = started;
        self.attaching.retain(|id| !bound.slots.contains(id));
        self.end_loading(kind, &name);
        if let Err(e) = result {
            for id in &bound.slots {
                // An offline device gets its routes back below; a new one
                // leaves none on channels that are free again.
                let _ = if offline.is_some() { engine.detach_slot(*id) } else { engine.remove_slot(*id) };
            }
            drop(bound);
            if let Some(b) = offline {
                let parked = Self::park_offline(engine, b).map_err(|pe| format!("{e}; {pe}"))?;
                self.bound.push(parked);
            }
            return Err(e);
        }
        bound.pos = pos;
        Self::key_colours(engine, &bound);
        let ids = bound.slots.clone();
        self.bound.push(bound);
        // It is open now: an old unplaceable binding of it must not come back.
        self.unplaced.retain(|u| !same_device(u, kind, &name));
        self.save()?;
        Ok(ids)
    }

    /// Replaces the device in `self.bound[i]` with `loaded` (already loaded),
    /// started at once. Routes stay on the channels that remain, and follow
    /// the device if it needs a new place (spec: slot model §4). On failure
    /// the old device is reopened where it was, or parked offline there.
    fn swap(
        &mut self,
        engine: &mut Engine,
        i: usize,
        kind: DeviceKind,
        name: &str,
        loaded: Loaded,
    ) -> Result<Bound, String> {
        let shape = loaded_shape(kind, name, &loaded);
        let Bound { binding: old, slots, handles, pos } = self.bound.remove(i);
        drop(handles); // stop callbacks before the slots are detached
        for id in &slots {
            engine.detach_slot(*id).map_err(|e| e.to_string())?;
        }
        let placed = match shape {
            Some((ins, outs)) => self.place_like(engine, &old, ins, outs),
            None => Ok((Some(old.first_input), Some(old.first_output), ChannelMove::default())),
        };
        let attempt = placed.and_then(|(fi, fo, mv)| {
            let at = Binding { first_input: fi.unwrap_or(0), first_output: fo.unwrap_or(0), ..old.clone() };
            let (bound, start) = self.attach(engine, kind, name, Some(&at), loaded)?;
            Ok((start_now(engine, bound, start)?, mv))
        });
        match attempt {
            Ok((mut bound, mv)) => {
                // Routes on channels the new device does not have go first, then
                // the rest follow it if it moved.
                let lost = |first: u32, was: u32, now: u32| (was > now).then_some((first, was, now));
                engine.drop_points_outside(
                    lost(old.first_input, old.inputs, bound.binding.inputs),
                    lost(old.first_output, old.outputs, bound.binding.outputs),
                );
                if mv != ChannelMove::default() {
                    engine.remap_channels(&mv);
                }
                bound.pos = pos;
                Self::key_colours(engine, &bound);
                Ok(bound)
            }
            Err(e) => {
                let net = self.net.as_ref().map(NetCtx::snapshot);
                let back = load(&self.asio_open, old.kind, &old.name, old.endpoint_id.as_deref(), net.as_ref())
                    .and_then(|l| self.attach(engine, old.kind, &old.name, Some(&old), l))
                    .and_then(|(b, start)| start_now(engine, b, start));
                let mut b = match back {
                    Ok(b) => b,
                    Err(_) => Self::park_offline(engine, old).map_err(|pe| format!("{e}; {pe}"))?,
                };
                b.pos = pos;
                Self::key_colours(engine, &b);
                self.bound.push(b);
                Err(e)
            }
        }
    }

    /// Where a device of `inputs` x `outputs` replacing `old` goes, per
    /// direction: on `old`'s first channel if it fits there (the old device's
    /// channels are free by now), else the first free block, with the move
    /// its routes make. `None` for a direction it has no channels in.
    pub fn place_like(
        &self,
        engine: &Engine,
        old: &Binding,
        inputs: u32,
        outputs: u32,
    ) -> Result<(Option<u32>, Option<u32>, ChannelMove), String> {
        let one = |is_in: bool, first: u32, was: u32, now: u32| -> Result<DirectionPlace, String> {
            let what = if is_in { "inputs" } else { "outputs" };
            if now == 0 {
                return Ok((None, None));
            }
            if was > 0 && engine.channels_free(is_in, first, now) {
                return Ok((Some(first), None));
            }
            let at = engine
                .free_block(is_in, now)
                .ok_or_else(|| format!("no room for {now} {what}: remove something first"))?;
            Ok((Some(at), (was > 0 && at != first).then_some((first, at, was.min(now)))))
        };
        let (fi, mi) = one(true, old.first_input, old.inputs, inputs)?;
        let (fo, mo) = one(false, old.first_output, old.outputs, outputs)?;
        Ok((fi, fo, ChannelMove { inputs: mi, outputs: mo }))
    }

    /// Refuses a device that is the master or already open (an offline one may be re-added).
    fn check_addable(&self, kind: DeviceKind, name: &str) -> Result<(), String> {
        let device = format!("{}:{}", kind.prefix(), name);
        if kind == DeviceKind::Asio {
            if let Some(i) = is_own_vasio_driver(name) {
                return Err(format!("{name} is a virtual device: turn on VASIO {} instead", (b'A' + i) as char));
            }
        }
        if kind == DeviceKind::Asio && self.master_name.as_deref() == Some(name) {
            return Err(format!("{device} is the master clock device"));
        }
        match self.bound.iter().find(|b| same_device(&b.binding, kind, name)) {
            Some(b) if !b.handles.is_empty() => Err(format!("{device} is already open as slot(s) {:?}", b.slots)),
            _ => Ok(()),
        }
    }

    /// Closes the device owning `slot` (all its slots) and saves the bindings.
    /// Returns `Ok(false)` if the slot is not a device slot.
    pub fn remove(&mut self, engine: &mut Engine, slot: u32) -> Result<bool, String> {
        let Some(i) = self.bound.iter().position(|b| b.slots.contains(&slot)) else { return Ok(false) };
        let b = self.bound.remove(i);
        self.unplaced.retain(|u| !same_device(u, b.binding.kind, &b.binding.name));
        if let Some(p) = b.pos.filter(|p| p.group.is_virtual()) {
            let _ = self.table.set_virtual(p, false, None);
        }
        drop(b.handles); // stop callbacks before the bridge sides are detached
        for id in b.slots {
            engine.remove_slot(id).map_err(|e| e.to_string())?;
        }
        self.save()?;
        Ok(true)
    }

    /// Re-opens the bindings read by [`open_file`](Self::open_file) at their
    /// saved channels. A device that cannot be opened keeps its channels as an
    /// offline slot. Returns one warning per offline device.
    pub fn restore(&mut self, engine: &mut Engine) -> Vec<String> {
        self.saved.devices.clear();
        let bindings = std::mem::take(&mut self.restore_list);
        let mut warnings = Vec::new();
        for (pos, b) in bindings {
            if self.bound.iter().any(|x| same_device(&x.binding, b.kind, &b.name)) {
                warnings.push(format!("{} duplicates an open device; its binding was dropped", b.device()));
                continue;
            }
            if b.kind == DeviceKind::Asio && self.master_name.as_deref() == Some(b.name.as_str()) {
                // Made master at its own position: expected, nothing to say.
                if pos.is_none() || pos != self.table.master() {
                    warnings
                        .push(format!("{} is now the master clock device; its device binding was dropped", b.device()));
                }
                continue;
            }
            let pos = pos.or_else(|| self.default_pos(&b));
            let net = self.net.as_ref().map(NetCtx::snapshot);
            let opened = load(&self.asio_open, b.kind, &b.name, b.endpoint_id.as_deref(), net.as_ref())
                .and_then(|loaded| self.attach(engine, b.kind, &b.name, Some(&b), loaded))
                .and_then(|(bound, start)| start_now(engine, bound, start));
            match opened {
                Ok(mut bound) => {
                    bound.pos = pos;
                    Self::key_colours(engine, &bound);
                    if let Some(p) = pos.filter(|p| p.group.is_virtual()) {
                        self.virtual_at.insert(p, bound.binding.clone());
                    }
                    self.bound.push(bound);
                }
                Err(e) => {
                    warnings.push(format!("{} is offline: {e}", b.device()));
                    match Self::park_offline(engine, b.clone()) {
                        Ok(mut parked) => {
                            parked.pos = pos;
                            Self::key_colours(engine, &parked);
                            self.bound.push(parked);
                        }
                        Err(e) => {
                            warnings.push(e);
                            self.unplaced.push(b);
                        }
                    }
                }
            }
        }
        let on: Vec<(PosId, VirtualState)> = self.table.virtuals().filter(|(_, v)| v.on).collect();
        for (pos, v) in on {
            if self.bound.iter().any(|b| b.pos == Some(pos)) {
                continue;
            }
            if let Err(e) = self.open_virtual(engine, pos, v.shape) {
                warnings.push(format!("{} is offline: {e}", pos.label()));
            }
        }
        warnings
    }

    /// The position a device read without one belongs in.
    fn default_pos(&self, b: &Binding) -> Option<PosId> {
        match b.kind {
            DeviceKind::Vasio => parse_vasio(&b.name).ok().and_then(|(n, _, _)| vasio_pos(n).ok()),
            DeviceKind::Vaio => Some(PosId { group: PosGroup::Vaio, index: 0 }),
            k => next_free(PosGroup::for_kind(k), &self.taken()),
        }
    }

    /// Opens virtual position `pos` (switched on) with `shape`: at its last
    /// placement if that still fits, else wherever there is room.
    fn open_virtual(&mut self, engine: &mut Engine, pos: PosId, shape: (u32, u32)) -> Result<Vec<u32>, String> {
        let (kind, name) = match pos.group {
            PosGroup::Vasio => (DeviceKind::Vasio, vasio_device_name(pos, shape)),
            _ => (DeviceKind::Vaio, "1".to_string()),
        };
        let at = self.virtual_at.get(&pos).cloned();
        let loaded = load(&self.asio_open, kind, &name, None, None)?;
        let attached = match self.attach(engine, kind, &name, at.as_ref(), loaded) {
            Ok(a) => a,
            Err(_) if at.is_some() => {
                let loaded = load(&self.asio_open, kind, &name, None, None)?;
                self.attach(engine, kind, &name, None, loaded)?
            }
            Err(e) => return Err(e),
        };
        let (bound, start) = attached;
        let mut bound = start_now(engine, bound, start)?;
        bound.pos = Some(pos);
        Self::key_colours(engine, &bound);
        let ids = bound.slots.clone();
        self.virtual_at.insert(pos, bound.binding.clone());
        self.bound.push(bound);
        Ok(ids)
    }

    /// Turns virtual position `pos` on (opening it, or reshaping it) or off
    /// (closing it; its routes are removed). Returns its slots.
    pub fn set_virtual(
        &mut self,
        engine: &mut Engine,
        pos: PosId,
        on: bool,
        shape: Option<(u32, u32)>,
    ) -> Result<Vec<u32>, String> {
        let before = self.table.virtual_state(pos);
        let v = self.table.set_virtual(pos, on, shape)?;
        if let Some(i) = self.bound.iter().position(|b| b.pos == Some(pos)) {
            let b = &self.bound[i];
            let same = (b.binding.inputs, b.binding.outputs) == v.shape || pos.group == PosGroup::Vaio;
            if on && same && !b.handles.is_empty() {
                self.save()?;
                return Ok(b.slots.clone());
            }
            if on && !b.handles.is_empty() {
                // A reshape: the routes on the channels that remain are kept.
                let name = vasio_device_name(pos, v.shape);
                let swapped = load(&self.asio_open, DeviceKind::Vasio, &name, None, None)
                    .and_then(|l| self.swap(engine, i, DeviceKind::Vasio, &name, l));
                return match swapped {
                    Ok(bound) => {
                        let ids = bound.slots.clone();
                        self.virtual_at.insert(pos, bound.binding.clone());
                        self.bound.push(bound);
                        self.save()?;
                        Ok(ids)
                    }
                    Err(e) => {
                        if let Some(b) = before {
                            let _ = self.table.set_virtual(pos, b.on, Some(b.shape));
                        }
                        Err(e)
                    }
                };
            }
            let b = self.bound.remove(i);
            drop(b.handles);
            for id in b.slots {
                engine.remove_slot(id).map_err(|e| e.to_string())?;
            }
        }
        let ids = if on {
            match self.open_virtual(engine, pos, v.shape) {
                Ok(ids) => ids,
                Err(e) => {
                    if let Some(b) = before {
                        let _ = self.table.set_virtual(pos, b.on, Some(b.shape));
                    }
                    return Err(e);
                }
            }
        } else {
            Vec::new()
        };
        self.save()?;
        Ok(ids)
    }

    /// Empties position `pos`: its device is closed and its routes removed.
    pub fn clear_position(&mut self, engine: &mut Engine, pos: PosId) -> Result<(), String> {
        if pos.group.is_virtual() {
            return self.set_virtual(engine, pos, false, None).map(|_| ());
        }
        let Some(slot) = self.bound.iter().find(|b| b.pos == Some(pos)).and_then(|b| b.slots.first().copied()) else {
            return Err(format!("{} is empty", pos.label()));
        };
        self.remove(engine, slot).map(|_| ())
    }

    /// The ASIO position to use as master from the next start.
    pub fn set_master_pos(&mut self, pos: Option<PosId>) -> Result<(), String> {
        self.table.set_master(pos)?;
        // The master running now stays in the setup: from the next start it is
        // an ordinary device in its own position (unless it is chosen again).
        if let (Some(now), Some(m)) = (self.master_now, self.master.clone()) {
            self.restore_list.retain(|(q, b)| !(*q == Some(now) && b.name == m.name));
            if pos != Some(now) {
                self.restore_list.push((Some(now), m));
            }
        }
        self.save()
    }

    /// The ASIO driver to start as master: the device at the master position.
    pub fn saved_master_name(&self) -> Option<String> {
        let p = self.table.master()?;
        if let Some(b) = self.bound.iter().find(|b| b.pos == Some(p)) {
            return Some(b.binding.name.clone());
        }
        if let Some((_, b)) = self.restore_list.iter().find(|(q, _)| *q == Some(p)) {
            return Some(b.name.clone());
        }
        self.master.as_ref().or(self.saved.master.as_ref()).map(|b| b.name.clone())
    }

    /// Every fixed position and what it holds, for the published state.
    pub fn positions(&self, engine: &Engine) -> Vec<PositionState> {
        let slots = engine.slots();
        let colour = |ids: &[u32]| ids.first().and_then(|id| slots.iter().find(|s| s.id == *id)).and_then(|s| s.color);
        all_positions()
            .into_iter()
            .map(|pos| {
                let b = self.bound.iter().find(|b| b.pos == Some(pos));
                let ids = b.map(|b| b.slots.clone()).unwrap_or_default();
                let device = b.map(|b| PositionDevice { kind: b.binding.kind, name: b.binding.name.clone() });
                let master = self.table.master() == Some(pos);
                let daw = b.and_then(|b| b.handles.iter().find_map(handle_client));
                let (status, shape, device, ids) = if pos.group.is_virtual() {
                    let v = self.table.virtual_state(pos);
                    let on = v.is_some_and(|v| v.on);
                    let online = b.is_some_and(|b| b.handles.iter().any(handle_connected));
                    let status = if on { PositionStatus::On { online } } else { PositionStatus::Off };
                    (status, v.map(|v| v.shape), device, ids)
                } else if let Some(b) = b {
                    (PositionStatus::Filled { online: !b.handles.is_empty() }, None, device, ids)
                } else if Some(pos) == self.master_now && self.master.is_some() {
                    let m =
                        self.master.as_ref().map(|m| PositionDevice { kind: DeviceKind::Asio, name: m.name.clone() });
                    let ids = self.master_health.as_ref().map(|(id, _)| vec![*id]).unwrap_or_default();
                    (PositionStatus::Filled { online: true }, None, m, ids)
                } else {
                    (PositionStatus::Empty, None, None, ids)
                };
                PositionState { pos, status, device, shape, daw, master, color: colour(&ids), slots: ids }
            })
            .collect()
    }

    /// Re-adds offline network streams whose engine discovery has now found
    /// (call regularly). Returns whether any came back.
    pub fn retry_offline_net(&mut self, engine: &mut Engine) -> bool {
        let Some(net) = &self.net else { return false };
        let peers = net.discovery.peers();
        let now = Instant::now();
        let due: Vec<(DeviceKind, String)> = self
            .bound
            .iter()
            .filter(|b| b.handles.is_empty() && matches!(b.binding.kind, DeviceKind::NetSend | DeviceKind::NetReceive))
            .filter(|b| {
                parse_net(&b.binding.name).is_ok_and(|n| peers.iter().any(|p| p.name.eq_ignore_ascii_case(&n.peer)))
            })
            .filter(|b| !self.net_retry_failed.iter().any(|(name, at)| *name == b.binding.name && now < *at))
            .map(|b| (b.binding.kind, b.binding.name.clone()))
            .collect();
        let mut back = false;
        for (kind, name) in due {
            self.net_retry_failed.retain(|(n, _)| *n != name);
            match self.add(engine, kind, &name) {
                Ok(_) => back = true,
                Err(e) => {
                    eprintln!("confluence-engine: {}:{name} is still offline: {e}", kind.prefix());
                    self.net_retry_failed.push((name, now + NET_RETRY_AFTER));
                }
            }
        }
        back
    }

    /// Holds a missing device's channels with an offline slot.
    fn park_offline(engine: &mut Engine, b: Binding) -> Result<Bound, String> {
        let spec = OfflineSlotSpec {
            name: b.name.clone(),
            device: b.device(),
            role: ClockRole::Soft,
            first_input: b.first_input,
            inputs: b.inputs,
            first_output: b.first_output,
            outputs: b.outputs,
        };
        match engine.add_offline_slot(&spec) {
            Ok(id) => Ok(Bound { binding: b, slots: vec![id], handles: Vec::new(), pos: None }),
            Err(e) => Err(format!("{}: channels could not be reserved: {e}", b.device())),
        }
    }

    /// Handles device commands; `None` for commands that are not about devices.
    pub fn handle(&mut self, engine: &mut Engine, cmd: &Command) -> Option<Response> {
        Some(match cmd {
            Command::ListDevices => match self.list() {
                Ok(d) => Response::Devices(d),
                Err(e) => Response::Error(e),
            },
            Command::AddDevice { kind, name } => match self.add(engine, *kind, name) {
                Ok(ids) => Response::SlotsAdded(ids),
                Err(e) => Response::Error(e),
            },
            Command::RemoveSlot { id } if self.attaching.contains(id) => {
                Response::Error(format!("slot {id} belongs to a device that is still starting"))
            }
            Command::RemoveSlot { id } => match self.remove(engine, *id) {
                Ok(true) => Response::Ok,
                Ok(false) => return None,
                Err(e) => Response::Error(e),
            },
            Command::FillPosition { pos, kind, name } => {
                let done = if pos.group.is_virtual() {
                    Err(format!("{} is a virtual position: turn it on instead", pos.label()))
                } else {
                    self.begin_fill(Some(*pos), *kind, name).and_then(|p| {
                        let loaded = p.load();
                        self.finish_add(engine, loaded)
                    })
                };
                match done {
                    Ok(_) => Response::Ok,
                    Err(e) => Response::Error(e),
                }
            }
            Command::ClearPosition { pos } => match self.clear_position(engine, *pos) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            Command::SetVirtual { pos, on, shape } => match self.set_virtual(engine, *pos, *on, *shape) {
                Ok(_) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            Command::SetMaster { pos } => match self.set_master_pos(*pos) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            _ => return None,
        })
    }

    /// Reports the ASIO master's own faults and driver requests in `Health`
    /// (its slot `id`; the master is not one of this manager's devices).
    pub fn watch_master(&mut self, id: u32, health: Arc<AsioHealth>) {
        self.master_health = Some((id, health));
    }

    /// Engine-wide conditions a user should know about (also in Health).
    pub fn notices(&self) -> Vec<String> {
        let mut out = self.migration_notes.clone();
        if let Some(why) = self.save_blocked {
            out.push(match &self.path {
                Some(p) => format!("device changes are not being saved: {} {why}", p.display()),
                None => "device changes are not being saved".into(),
            });
        }
        out
    }

    /// Adds each open device's own health (loss, faults, driver requests) to
    /// the engine's `Health` response for that device's slots.
    pub fn annotate(&self, resp: &mut Response) {
        let Response::Health { slots, notices, .. } = resp else { return };
        notices.extend(self.notices());
        if let Some((id, hl)) = &self.master_health {
            for h in slots.iter_mut().filter(|h| h.id == *id) {
                h.device_faults = hl.faults.load(Ordering::Relaxed);
                h.driver_requests = hl.reset_requests.load(Ordering::Relaxed)
                    + hl.resync_requests.load(Ordering::Relaxed)
                    + hl.rate_changes.load(Ordering::Relaxed);
            }
        }
        for b in &self.bound {
            let (mut lost, mut faults, mut requests, mut net) = (false, 0, 0, None);
            for h in &b.handles {
                match h {
                    Handle::Asio(dev) => {
                        let hl = dev.health();
                        faults += hl.faults.load(Ordering::Relaxed);
                        requests += hl.reset_requests.load(Ordering::Relaxed)
                            + hl.resync_requests.load(Ordering::Relaxed)
                            + hl.rate_changes.load(Ordering::Relaxed);
                    }
                    Handle::Wasapi(stream) => {
                        let hl = stream.health();
                        lost |= hl.lost.load(Ordering::Relaxed);
                        faults += hl.faults.load(Ordering::Relaxed);
                    }
                    Handle::NetReceive(r) => {
                        let st = r.stats();
                        lost |= st.silent_ms > NET_SILENT_MS;
                        net = Some(st);
                    }
                    Handle::Vasio(_) | Handle::Vaio(_) | Handle::NetSend(_) => {}
                }
            }
            for h in slots.iter_mut().filter(|h| b.slots.contains(&h.id)) {
                (h.device_lost, h.device_faults, h.driver_requests) = (lost, faults, requests);
                h.net = net;
            }
        }
    }

    /// Current bindings (for tests and diagnostics).
    pub fn bindings(&self) -> Vec<Binding> {
        self.bound.iter().map(|b| b.binding.clone()).collect()
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        if self.save_blocked.is_some() {
            return Ok(());
        }
        let mut positions: Vec<SavedPosition> = Vec::new();
        let mut unplaced = self.unplaced.clone();
        for b in &self.bound {
            match b.pos {
                Some(pos) => {
                    let v = self.table.virtual_state(pos);
                    positions.push(SavedPosition {
                        pos,
                        on: v.map(|v| v.on),
                        shape: v.map(|v| v.shape),
                        binding: Some(b.binding.clone()),
                    });
                }
                None => unplaced.push(b.binding.clone()),
            }
        }
        // Devices read but not restored yet (e.g. saved before restore ran).
        for (pos, b) in &self.restore_list {
            if let Some(pos) = pos.filter(|p| !positions.iter().any(|q| q.pos == *p)) {
                positions.push(SavedPosition { pos, on: None, shape: None, binding: Some(b.clone()) });
            }
        }
        for (pos, v) in self.table.virtuals() {
            if !positions.iter().any(|q| q.pos == pos) {
                positions.push(SavedPosition {
                    pos,
                    on: Some(v.on),
                    shape: Some(v.shape),
                    binding: self.virtual_at.get(&pos).cloned(),
                });
            }
        }
        positions.sort_by_key(|p| p.pos);
        let saved = SavedV2 {
            version: 2,
            master: self.table.master(),
            master_binding: self.master.clone().or_else(|| self.saved.master.clone()),
            positions,
            unplaced,
        };
        let json = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Write, flush to disk, then atomically replace: a power loss leaves
        // either the old file or the new one, never an empty one.
        let tmp = path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()
        };
        write().map_err(|e| e.to_string())?;
        replace_durably(&tmp, path).map_err(|e| e.to_string())
    }

    fn soft_spec(
        &self,
        name: String,
        device: String,
        channels: usize,
        rate: f64,
        block: usize,
        at: Option<u32>,
    ) -> SoftSlotSpec {
        SoftSlotSpec {
            name,
            device,
            channels,
            device_rate: rate,
            device_block: block,
            quality: self.quality,
            first_channel: at,
            margin_frames: None,
            max_growth_frames: None,
        }
    }

    /// Attaches a loaded device to the engine: creates its slots and starts it.
    fn attach(
        &mut self,
        engine: &mut Engine,
        kind: DeviceKind,
        name: &str,
        at: Option<&Binding>,
        loaded: Loaded,
    ) -> Result<(Bound, Option<PendingStart>), String> {
        let device = format!("{}:{}", kind.prefix(), name);
        match loaded {
            Loaded::Asio(dev) => self.open_asio(engine, name, device, at, dev).map(|(b, s)| (b, Some(s))),
            Loaded::Vasio => self.open_vasio(engine, name, device, at).map(|b| (b, None)),
            Loaded::Vaio => self.open_vaio(engine, name, device, at).map(|b| (b, None)),
            Loaded::Net(n, looked_up) => self.open_net(engine, kind, name, at, n, looked_up).map(|b| (b, None)),
            Loaded::Wasapi(mut stream, endpoint_id) => {
                let f = stream.format();
                let mut binding = Binding { endpoint_id, ..binding_of(kind, name) };
                let (id, handler) = if f.direction == Direction::Render {
                    let spec = self.soft_spec(
                        name.to_string(),
                        device,
                        f.channels,
                        f.sample_rate,
                        f.period_frames,
                        at.map(|b| b.first_output),
                    );
                    let (id, mut side) = engine.add_soft_output(&spec).map_err(|e| e.to_string())?;
                    binding.outputs = f.channels as u32;
                    (id, Handler::Render(Box::new(move |buf: &mut [f32], now| side.read_interleaved(buf, now))))
                } else {
                    let spec = self.soft_spec(
                        name.to_string(),
                        device,
                        f.channels,
                        f.sample_rate,
                        f.period_frames,
                        at.map(|b| b.first_input),
                    );
                    let (id, mut side) = engine.add_soft_input(&spec).map_err(|e| e.to_string())?;
                    binding.inputs = f.channels as u32;
                    (id, Handler::Capture(Box::new(move |buf: &[f32], now| side.write_interleaved(buf, now))))
                };
                if let Err(e) = stream.start(handler) {
                    let _ = engine.remove_slot(id);
                    return Err(e.to_string());
                }
                let slot = engine.slots().into_iter().find(|s| s.id == id);
                if let Some(s) = slot {
                    (binding.first_input, binding.first_output) = (s.first_input, s.first_output);
                }
                Ok((Bound { binding, slots: vec![id], handles: vec![Handle::Wasapi(stream)], pos: None }, None))
            }
        }
    }

    /// A network stream: a send slot (strict, on the master clock) or a
    /// receive slot (soft: its bridge recovers the sender's clock).
    fn open_net(
        &mut self,
        engine: &mut Engine,
        kind: DeviceKind,
        name: &str,
        at: Option<&Binding>,
        n: NetName,
        looked_up: Option<std::net::SocketAddr>,
    ) -> Result<Bound, String> {
        let device = format!("{}:{}", kind.prefix(), name);
        let net = self.net.as_ref().ok_or("network audio is off in this engine")?;
        let rate = engine.config().sample_rate;
        let addr = match resolve(net, &n) {
            Ok(a) => a,
            Err(e) => looked_up.ok_or(e)?,
        };
        // Coming back: on its saved channels (others may be routed after them).
        let saved_inputs = at.map(|b| b.inputs as usize).filter(|&c| c > 0);
        let saved_outputs = at.map(|b| b.outputs as usize).filter(|&c| c > 0);
        if kind == DeviceKind::NetSend {
            let channels = n.channels.or(saved_outputs).unwrap_or(2);
            let spec = StrictSlotSpec {
                name: format!("{} to {}", n.stream, n.peer),
                device,
                inputs: 0,
                outputs: channels,
                first_input: None,
                first_output: at.map(|b| b.first_output),
            };
            let mut handle = None;
            let (id, ch) = engine
                .add_strict_slot(&spec, |ch| {
                    let (side, h) = net.host.add_sender(SendSpec {
                        dest: addr,
                        stream: n.stream.clone(),
                        channels,
                        rate: rate as u32,
                    });
                    let h = Arc::new(h);
                    handle = Some(h.clone());
                    let side = NetSendSide { side, first_output: ch.first_output };
                    Ok((Box::new(side) as Box<dyn StrictSide>, Arc::new(NetSendStats(h)) as Arc<dyn StrictStats>))
                })
                .map_err(|e| e.to_string())?;
            let binding =
                Binding { first_output: ch.first_output as u32, outputs: ch.outputs as u32, ..binding_of(kind, name) };
            let handles = handle.map(Handle::NetSend).into_iter().collect();
            return Ok(Bound { binding, slots: vec![id], handles, pos: None });
        }
        let from = addr.ip();
        let heard = net.host.heard().into_iter().find(|h| h.from == from && h.stream == n.stream);
        let channels = n.channels.or(saved_inputs).or(heard.as_ref().map(|h| h.channels as usize)).unwrap_or(2);
        let stream_rate =
            heard.as_ref().map(|h| h.rate as f64).or(at.and_then(|b| b.rate).map(f64::from)).unwrap_or(rate);
        let packet = (stream_rate / 1000.0).round().max(1.0) as usize;
        let mut spec = self.soft_spec(
            format!("{} from {}", n.stream, n.peer),
            device,
            channels,
            stream_rate,
            packet,
            at.map(|b| b.first_input),
        );
        spec.margin_frames = Some(2 * packet);
        spec.max_growth_frames = Some((NET_MAX_LATENCY_S * stream_rate) as usize);
        let (id, side) = engine.add_soft_input(&spec).map_err(|e| e.to_string())?;
        let h = net.host.add_receiver(from, &n.stream, channels, stream_rate as u32, Box::new(side));
        let slot = engine.slots().into_iter().find(|s| s.id == id);
        let binding = Binding {
            first_input: slot.map_or(0, |s| s.first_input),
            inputs: channels as u32,
            rate: Some(stream_rate as u32),
            ..binding_of(kind, name)
        };
        Ok(Bound { binding, slots: vec![id], handles: vec![Handle::NetReceive(h)], pos: None })
    }

    fn open_vasio(
        &mut self,
        engine: &mut Engine,
        name: &str,
        device: String,
        at: Option<&Binding>,
    ) -> Result<Bound, String> {
        let (instance, daw_inputs, daw_outputs) = parse_vasio(name)?;
        let (rate, block) = (engine.config().sample_rate, engine.config().block);
        // The DAW's outputs are engine inputs and its inputs are engine outputs.
        let spec = StrictSlotSpec {
            name: format!("VASIO {instance}"),
            device,
            inputs: daw_outputs,
            outputs: daw_inputs,
            first_input: at.map(|b| b.first_input),
            first_output: at.map(|b| b.first_output),
        };
        let mut stats = None;
        let (id, ch) = engine
            .add_strict_slot(&spec, |ch| {
                let slot =
                    VasioSlot::open(instance, daw_inputs, daw_outputs, rate, block).map_err(|e| e.to_string())?;
                stats = Some(slot.stats());
                let side = VasioSide { slot, first_input: ch.first_input, first_output: ch.first_output };
                Ok((Box::new(side) as Box<dyn StrictSide>, slot_stats(stats.clone())))
            })
            .map_err(|e| e.to_string())?;
        if let Some(root) = &self.vasio_config_root {
            let shape = InstanceConfig {
                daw_inputs: daw_inputs as u32,
                daw_outputs: daw_outputs as u32,
                sample_rate: rate.round() as u32,
                block: block as u32,
            };
            // Only a fallback for DAWs opened while the engine is down.
            let _ = confluence_provider_vasio::config::save_at(root, instance, &shape);
        }
        let binding = Binding {
            kind: DeviceKind::Vasio,
            name: name.to_string(),
            first_input: ch.first_input as u32,
            inputs: daw_outputs as u32,
            first_output: ch.first_output as u32,
            outputs: daw_inputs as u32,
            endpoint_id: None,
            rate: None,
        };
        let handles = stats.map(Handle::Vasio).into_iter().collect();
        Ok(Bound { binding, slots: vec![id], handles, pos: None })
    }

    fn open_vaio(
        &mut self,
        engine: &mut Engine,
        name: &str,
        device: String,
        at: Option<&Binding>,
    ) -> Result<Bound, String> {
        let instance = parse_vaio(name)?;
        let (rate, block) = (engine.config().sample_rate, engine.config().block);
        // Before any channel is taken or the driver touched.
        confluence_provider_vaio::check_rate(rate).map_err(|e| e.to_string())?;
        confluence_provider_vaio::ring_shape(block).map_err(|e| e.to_string())?;
        let spec = StrictSlotSpec {
            name: format!("VAIO {instance}"),
            device,
            inputs: confluence_provider_vaio::CHANNELS,
            outputs: 0,
            first_input: at.map(|b| b.first_input),
            first_output: None,
        };
        let mut stats = None;
        let (id, ch) = engine
            .add_strict_slot(&spec, |ch| {
                let slot = VaioSlot::open(rate, block).map_err(|e| e.to_string())?;
                let s = slot.stats();
                stats = Some(s.clone());
                let side = VaioSide { slot, first_input: ch.first_input };
                Ok((Box::new(side) as Box<dyn StrictSide>, s as Arc<dyn StrictStats>))
            })
            .map_err(|e| e.to_string())?;
        let binding = Binding {
            kind: DeviceKind::Vaio,
            name: name.trim().to_string(),
            first_input: ch.first_input as u32,
            inputs: ch.inputs as u32,
            first_output: 0,
            outputs: 0,
            endpoint_id: None,
            rate: None,
        };
        let handles = stats.map(Handle::Vaio).into_iter().collect();
        Ok(Bound { binding, slots: vec![id], handles, pos: None })
    }

    fn open_asio(
        &mut self,
        engine: &mut Engine,
        name: &str,
        device: String,
        at: Option<&Binding>,
        dev: AsioDevice,
    ) -> Result<(Bound, PendingStart), String> {
        let info = dev.info().clone();
        let (ins, outs) = (info.inputs(), info.outputs());
        let (rate, block) = (info.sample_rate, info.preferred_block.max(1) as usize);
        let mut slots = Vec::new();
        let mut dev_in: Option<InputDeviceSide> = None;
        let mut dev_out: Option<OutputDeviceSide> = None;
        let undo = |engine: &mut Engine, slots: &[u32]| {
            for id in slots {
                let _ = engine.remove_slot(*id);
            }
        };
        if ins > 0 {
            let spec =
                self.soft_spec(format!("{name} in"), device.clone(), ins, rate, block, at.map(|b| b.first_input));
            let (id, side) = engine.add_soft_input(&spec).map_err(|e| e.to_string())?;
            slots.push(id);
            dev_in = Some(side);
        }
        if outs > 0 {
            let spec = self.soft_spec(format!("{name} out"), device, outs, rate, block, at.map(|b| b.first_output));
            match engine.add_soft_output(&spec) {
                Ok((id, side)) => {
                    slots.push(id);
                    dev_out = Some(side);
                }
                Err(e) => {
                    undo(engine, &slots);
                    return Err(e.to_string());
                }
            }
        }
        let max = info.max_block.max(block as i32) as usize;
        let mut planar = vec![0f32; max];
        let mut inter = vec![0f32; max * ins.max(outs).max(1)];
        let callback = move |io: &mut AsioIo<'_>| {
            let n = io.frames().min(max);
            if let Some(d) = dev_in.as_mut() {
                for c in 0..ins {
                    io.read_input(c, &mut planar[..n]);
                    for (f, s) in planar[..n].iter().enumerate() {
                        inter[f * ins + c] = *s;
                    }
                }
                d.write_interleaved(&inter[..n * ins], io.now());
            }
            if let Some(d) = dev_out.as_mut() {
                d.read_interleaved(&mut inter[..n * outs], io.now());
                for c in 0..outs {
                    for (f, s) in planar[..n].iter_mut().enumerate() {
                        *s = inter[f * outs + c];
                    }
                    io.write_output(c, &planar[..n]);
                }
            }
        };
        let mut binding = Binding {
            kind: DeviceKind::Asio,
            name: name.to_string(),
            first_input: 0,
            inputs: ins as u32,
            first_output: 0,
            outputs: outs as u32,
            endpoint_id: None,
            rate: None,
        };
        for s in engine.slots().into_iter().filter(|s| slots.contains(&s.id)) {
            if s.inputs > 0 {
                binding.first_input = s.first_input;
            }
            if s.outputs > 0 {
                binding.first_output = s.first_output;
            }
        }
        // Started later (possibly without the engine lock): see AttachedAdd::start.
        Ok((
            Bound { binding, slots, handles: Vec::new(), pos: None },
            PendingStart { dev, callback: Box::new(callback), block },
        ))
    }
}

/// Registers `dev` as the engine's master slot and starts it: every driver
/// callback copies the master's inputs into the engine, runs one engine block
/// on the device's clock, and copies the engine's outputs back. The engine's
/// block size must equal the stream's (create the engine from `dev.info()`).
/// `placement` = saved (first input, first output) channels, if any.
pub fn start_asio_master(
    dev: &mut AsioDevice,
    engine: &mut Engine,
    mut audio: AudioEngine,
    name: &str,
    placement: Option<(u32, u32)>,
) -> Result<(u32, StreamInfo, MasterChannels), String> {
    let info = dev.info().clone();
    let block = engine.config().block;
    let spec = MasterSlotSpec {
        name: format!("{name} (master)"),
        device: format!("asio:{name}"),
        inputs: info.inputs(),
        outputs: info.outputs(),
        first_input: placement.map(|p| p.0),
        first_output: placement.map(|p| p.1),
    };
    let (id, ch) = engine.add_master_slot(&spec).map_err(|e| e.to_string())?;
    let callback = move |io: &mut AsioIo<'_>| {
        for c in 0..ch.inputs {
            io.read_input(c, audio.inputs_mut().channel_mut(ch.first_input + c));
        }
        audio.process_master_block(io.now(), io.frames_since_last());
        for c in 0..ch.outputs {
            io.write_output(c, audio.outputs().channel(ch.first_output + c));
        }
    };
    let stream = dev
        .start(StreamConfig { sample_rate: Some(engine.config().sample_rate), block: Some(block) }, Box::new(callback))
        .map_err(|e| e.to_string())?;
    Ok((id, stream, ch))
}

/// A VASIO instance as a strict slot on the audio thread.
struct VasioSide {
    slot: VasioSlot,
    first_input: usize,
    first_output: usize,
}

impl StrictSide for VasioSide {
    fn receive(&mut self, inputs: &mut PlanarBuffer) {
        self.slot.receive(inputs, self.first_input);
    }

    fn send(&mut self, outputs: &PlanarBuffer) {
        self.slot.send(outputs, self.first_output);
    }
}

impl StrictStats for VasioStats {
    fn xruns(&self) -> (u64, u64) {
        (self.underruns.load(Ordering::Relaxed), self.overruns.load(Ordering::Relaxed))
    }

    fn attached(&self) -> Option<bool> {
        Some(self.connected.load(Ordering::Relaxed))
    }

    fn idle_note(&self) -> Option<&'static str> {
        Some("no DAW attached (a DAW open on another rate or block needs a reset)")
    }
}

/// The VAIO endpoint as a strict slot on the audio thread (inputs only).
struct VaioSide {
    slot: VaioSlot,
    first_input: usize,
}

impl StrictSide for VaioSide {
    fn receive(&mut self, inputs: &mut PlanarBuffer) {
        self.slot.receive(inputs, self.first_input);
    }

    fn send(&mut self, _outputs: &PlanarBuffer) {}
}

impl StrictStats for VaioStats {
    fn xruns(&self) -> (u64, u64) {
        (self.underruns.load(Ordering::Relaxed), 0)
    }

    /// Whether an app is playing to the endpoint.
    fn attached(&self) -> Option<bool> {
        Some(self.streaming.load(Ordering::Relaxed))
    }

    fn idle_note(&self) -> Option<&'static str> {
        Some("no app playing")
    }
}

fn slot_stats(stats: Option<Arc<VasioStats>>) -> Arc<dyn StrictStats> {
    stats.unwrap_or_default()
}

/// Moves `from` over `to`, and returns only once the move itself is on disk.
fn replace_durably(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    // SAFETY: two valid, NUL-terminated wide paths.
    unsafe {
        MoveFileExW(
            &HSTRING::from(from.as_os_str()),
            &HSTRING::from(to.as_os_str()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

/// Starts a just-attached device on the spot (start-up restore, which holds
/// the engine lock anyway). On failure its slots are detached, keeping routes.
fn start_now(engine: &mut Engine, mut bound: Bound, start: Option<PendingStart>) -> Result<Bound, String> {
    let Some(PendingStart { mut dev, callback, block }) = start else { return Ok(bound) };
    match dev.start(StreamConfig { sample_rate: None, block: Some(block) }, callback) {
        Ok(_) => {
            bound.handles.push(Handle::Asio(dev));
            Ok(bound)
        }
        Err(e) => {
            for id in &bound.slots {
                let _ = engine.detach_slot(*id);
            }
            Err(e.to_string())
        }
    }
}

/// A binding with only its identity filled in (for comparisons).
fn binding_of(kind: DeviceKind, name: &str) -> Binding {
    Binding {
        kind,
        name: name.to_string(),
        first_input: 0,
        inputs: 0,
        first_output: 0,
        outputs: 0,
        endpoint_id: None,
        rate: None,
    }
}

/// Whether `binding` is the device `kind`/`name` (VASIO: the same instance,
/// however its shape is spelled).
fn same_device(binding: &Binding, kind: DeviceKind, name: &str) -> bool {
    if binding.kind != kind {
        return false;
    }
    match kind {
        DeviceKind::Vasio => match (parse_vasio(&binding.name), parse_vasio(name)) {
            (Ok(a), Ok(b)) => a.0 == b.0,
            _ => binding.name == name,
        },
        DeviceKind::Vaio => binding.name.trim() == name.trim(),
        DeviceKind::NetSend | DeviceKind::NetReceive => match (parse_net(&binding.name), parse_net(name)) {
            (Ok(a), Ok(b)) => a.peer.eq_ignore_ascii_case(&b.peer) && a.port == b.port && a.stream == b.stream,
            _ => binding.name == name,
        },
        _ => binding.name == name,
    }
}

/// An offline network stream whose engine is found but that fails to open is
/// tried again after this long.
const NET_RETRY_AFTER: Duration = Duration::from_secs(5);
/// A receive stream silent this long is reported lost.
const NET_SILENT_MS: u64 = 1000;
/// The most latency a receive stream may grow to absorb a bad network.
const NET_MAX_LATENCY_S: f64 = 0.040;

/// A network stream's name: `<peer>[:port]/<stream>[:<channels>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetName {
    pub peer: String,
    pub port: Option<u16>,
    pub stream: String,
    pub channels: Option<usize>,
}

pub fn parse_net(name: &str) -> Result<NetName, String> {
    let bad = || format!("{name:?}: a network stream is <computer>/<stream>[:<channels>]");
    let (peer, rest) = name.trim().split_once('/').ok_or_else(bad)?;
    let (stream, channels) = match rest.rsplit_once(':') {
        Some((s, c)) if !c.is_empty() && c.bytes().all(|d| d.is_ascii_digit()) => {
            (s, Some(c.parse::<usize>().map_err(|_| bad())?))
        }
        _ => (rest, None),
    };
    if channels.is_some_and(|c| !(1..=confluence_net::packet::MAX_CHANNELS as usize).contains(&c)) {
        return Err("a network stream has 1 to 64 channels".into());
    }
    let (peer, port) = match peer.rsplit_once(':') {
        Some((h, p)) => (h, Some(p.parse::<u16>().map_err(|_| bad())?)),
        None => (peer, None),
    };
    if peer.trim().is_empty() {
        return Err(bad());
    }
    if stream.is_empty() || stream.len() > confluence_net::packet::MAX_NAME || stream.chars().any(char::is_control) {
        return Err("stream names are 1 to 64 characters on one line".into());
    }
    Ok(NetName { peer: peer.trim().to_string(), port, stream: stream.to_string(), channels })
}

/// Where a stream's peer is: an IPv4 address, or an engine found on the network.
fn resolve(net: &NetCtx, n: &NetName) -> Result<std::net::SocketAddr, String> {
    if let Ok(ip) = n.peer.parse::<std::net::Ipv4Addr>() {
        return Ok((ip, n.port.unwrap_or(confluence_net::DEFAULT_PORT)).into());
    }
    let peers = net.discovery.peers();
    let p = peers
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(&n.peer))
        .ok_or_else(|| format!("{} was not found on the network", n.peer))?;
    let ip: std::net::IpAddr = p.address.parse().map_err(|_| format!("{} has no usable address", n.peer))?;
    Ok((ip, n.port.unwrap_or(p.port)).into())
}

/// A send stream on the audio thread: its output channels go to the network thread.
struct NetSendSide {
    side: SendSide,
    first_output: usize,
}

impl StrictSide for NetSendSide {
    fn receive(&mut self, _inputs: &mut PlanarBuffer) {}

    fn send(&mut self, outputs: &PlanarBuffer) {
        self.side.write(outputs, self.first_output);
    }
}

struct NetSendStats(Arc<SendHandle>);

impl StrictStats for NetSendStats {
    fn xruns(&self) -> (u64, u64) {
        (0, self.0.dropped())
    }
}

/// Milestone 0 has one VAIO endpoint, "1".
pub fn parse_vaio(name: &str) -> Result<u32, String> {
    match name.trim() {
        "1" => Ok(1),
        other => Err(format!("'{other}' is not a VAIO device: only VAIO 1 exists")),
    }
}

/// Parses a VASIO device name: `N`, `N:C` or `N:IxO` (DAW inputs x outputs).
pub fn parse_vasio(name: &str) -> Result<(u32, usize, usize), String> {
    let bad = || format!("'{name}' is not a VASIO device: use N, N:channels or N:INxOUT (e.g. 1, 1:8, 1:8x2)");
    let (n, shape) = name.trim().split_once(':').unwrap_or((name.trim(), "2"));
    let instance: u32 = n.parse().map_err(|_| bad())?;
    let (i, o) = match shape.split_once(['x', 'X']) {
        Some((i, o)) => (i.parse().map_err(|_| bad())?, o.parse().map_err(|_| bad())?),
        None => {
            let c = shape.parse().map_err(|_| bad())?;
            (c, c)
        }
    };
    Ok((instance, i, o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;

    #[test]
    fn wasapi_bindings_remember_the_endpoint_id_and_old_files_still_load() {
        let old = r#"{"kind":"WasapiRender","name":"Speakers (USB Audio Device)","first_input":0,"inputs":0,
            "first_output":4,"outputs":2}"#;
        let b: Binding = serde_json::from_str(old).unwrap();
        assert_eq!(b.endpoint_id, None, "files written before endpoint ids still load");
        let with_id = Binding { endpoint_id: Some("{0.0.0.00000000}.{abc}".into()), ..b };
        let text = serde_json::to_string(&with_id).unwrap();
        assert!(text.contains("endpoint_id"));
        assert_eq!(serde_json::from_str::<Binding>(&text).unwrap(), with_id);
        let asio = binding_of(DeviceKind::Asio, "x");
        assert!(!serde_json::to_string(&asio).unwrap().contains("endpoint_id"), "only written when known");
    }

    #[test]
    fn idle_strict_slots_say_why_in_their_own_terms() {
        let vaio = VaioStats::default();
        assert_eq!(StrictStats::idle_note(&vaio), Some("no app playing"));
        let vasio = VasioStats::default();
        assert!(StrictStats::idle_note(&vasio).is_some_and(|n| n.contains("no DAW attached")));
    }

    #[test]
    fn vaio_has_one_instance_named_1() {
        assert_eq!(parse_vaio("1"), Ok(1));
        assert_eq!(parse_vaio(" 1 "), Ok(1));
        assert!(parse_vaio("2").unwrap_err().contains("only VAIO 1"));
        assert!(parse_vaio("speakers").is_err());
    }

    #[test]
    fn vaio_on_a_44k_engine_is_a_clear_error_before_the_driver_is_touched() {
        let (mut engine, _audio) = Engine::new(EngineConfig::new(44_100.0, 256));
        let mut devices = DeviceManager::new(None);
        let err = devices.add(&mut engine, DeviceKind::Vaio, "1").unwrap_err();
        assert!(err.contains("48 kHz") && err.contains("44100"), "{err}");
        assert!(engine.slots().is_empty());
    }

    #[test]
    fn network_stream_names_parse() {
        let n = parse_net("Lilith/Main:4").unwrap();
        assert_eq!((n.peer.as_str(), n.port, n.stream.as_str(), n.channels), ("Lilith", None, "Main", Some(4)));
        let n = parse_net("192.168.50.12:7000/Stream 2").unwrap();
        assert_eq!(
            (n.peer.as_str(), n.port, n.stream.as_str(), n.channels),
            ("192.168.50.12", Some(7000), "Stream 2", None)
        );
        assert!(parse_net("Lilith").is_err(), "no stream");
        assert!(parse_net("/Main").is_err(), "no peer");
        assert!(parse_net("Lilith/").is_err());
        assert!(parse_net("Lilith/Main:0").is_err());
        assert!(parse_net("Lilith/Main:65").is_err());
        assert!(parse_net(&format!("Lilith/{}", "x".repeat(65))).is_err(), "name too long");
        assert!(parse_net("Lilith/a\nb").is_err());
    }

    #[test]
    fn vasio_names_parse() {
        assert_eq!(parse_vasio("1"), Ok((1, 2, 2)));
        assert_eq!(parse_vasio("3:8"), Ok((3, 8, 8)));
        assert_eq!(parse_vasio(" 2:16x2 "), Ok((2, 16, 2)));
        assert!(parse_vasio("x").is_err());
        assert!(parse_vasio("1:8y2").is_err());
    }
}
