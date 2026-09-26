//! Device slots: opens ASIO, WASAPI and per-app capture devices as engine
//! slots, persists their bindings with channel ranges (machine-specific, spec
//! §15), restores missing devices as offline slots so channel numbers never
//! shift (spec §5.2, §16), and runs an ASIO device as the hardware master.

use std::path::PathBuf;

use confluence_api::{ClockRole, Command, DeviceInfo, DeviceKind, Response};
use confluence_core::asrc::AsrcQuality;
use confluence_core::bridge::{InputDeviceSide, OutputDeviceSide};
use confluence_provider_asio::registry::installed_drivers;
use confluence_provider_asio::{AsioDevice, AsioHostError, AsioIo, StreamConfig, StreamInfo};
use confluence_provider_wasapi::{endpoints, find_endpoint, find_process, Direction, Handler, Target, WasapiStream};
use serde::{Deserialize, Serialize};

use crate::audio::AudioEngine;
use crate::engine::{Engine, MasterChannels, MasterSlotSpec, OfflineSlotSpec, SoftSlotSpec};

/// A device bound to slots, with the channel ranges it occupies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub kind: DeviceKind,
    pub name: String,
    pub first_input: u32,
    pub inputs: u32,
    pub first_output: u32,
    pub outputs: u32,
}

impl Binding {
    fn device(&self) -> String {
        format!("{}:{}", self.kind.prefix(), self.name)
    }
}

/// An open device. Held only for its `Drop`, which stops the stream.
#[allow(dead_code)]
enum Handle {
    Asio(AsioDevice),
    Wasapi(WasapiStream),
}

struct Bound {
    binding: Binding,
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

/// Opens an ASIO driver by name (injectable so tests can use fake drivers).
pub type AsioOpener = Box<dyn Fn(&str) -> Result<AsioDevice, AsioHostError> + Send>;

pub struct DeviceManager {
    bound: Vec<Bound>,
    master: Option<Binding>,
    /// Bindings read at startup, not yet restored.
    saved: Saved,
    path: Option<PathBuf>,
    asio_open: AsioOpener,
    quality: AsrcQuality,
}

impl DeviceManager {
    /// A manager that does not persist bindings.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            bound: Vec::new(),
            master: None,
            saved: Saved::default(),
            path,
            asio_open: Box::new(AsioDevice::open_installed),
            quality: AsrcQuality::Sinc64,
        }
    }

    /// Reads saved bindings from `path`. An unreadable file is renamed to
    /// `.bad` (never silently discarded) and reported as a warning.
    pub fn open_file(path: PathBuf) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let saved = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => {
                warnings.push(format!("could not read {}: {e}", path.display()));
                Saved::default()
            }
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                let bad = path.with_extension("bad");
                let _ = std::fs::rename(&path, &bad);
                warnings.push(format!(
                    "{} is not valid ({e}); kept as {} and starting with no devices",
                    path.display(),
                    bad.display()
                ));
                Saved::default()
            }),
        };
        let mut m = Self::new(Some(path));
        m.saved = saved;
        (m, warnings)
    }

    /// Saved channel placement (first input, first output) of the master `name`, if any.
    pub fn saved_master(&self, name: &str) -> Option<(u32, u32)> {
        self.saved.master.as_ref().filter(|b| b.name == name).map(|b| (b.first_input, b.first_output))
    }

    /// Records the running master's placement so it is reused next time.
    pub fn set_master(&mut self, name: &str, ch: MasterChannels) -> Result<(), String> {
        self.master = Some(Binding {
            kind: DeviceKind::Asio,
            name: name.to_string(),
            first_input: ch.first_input as u32,
            inputs: ch.inputs as u32,
            first_output: ch.first_output as u32,
            outputs: ch.outputs as u32,
        });
        self.save()
    }

    pub fn with_asio_opener(mut self, opener: AsioOpener) -> Self {
        self.asio_open = opener;
        self
    }

    /// Devices that can be opened. ASIO channel counts are unknown until a
    /// driver is loaded, so they are reported as 0.
    pub fn list(&self) -> Result<Vec<DeviceInfo>, String> {
        let mut out: Vec<DeviceInfo> = installed_drivers()
            .map_err(|e| e.to_string())?
            .into_iter()
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
        Ok(out)
    }

    /// Opens a device as slot(s) and saves the binding. A device that is
    /// already open (or is the master) is refused: many drivers misbehave when
    /// loaded twice. An offline device comes back on its saved channels.
    pub fn add(&mut self, engine: &mut Engine, kind: DeviceKind, name: &str) -> Result<Vec<u32>, String> {
        let device = format!("{}:{}", kind.prefix(), name);
        if kind == DeviceKind::Asio && self.master.as_ref().is_some_and(|m| m.name == name) {
            return Err(format!("{device} is the master clock device"));
        }
        let existing = self.bound.iter().position(|b| b.binding.kind == kind && b.binding.name == name);
        let offline = match existing {
            Some(i) if !self.bound[i].handles.is_empty() => {
                return Err(format!("{device} is already open as slot(s) {:?}", self.bound[i].slots));
            }
            Some(i) => {
                let b = self.bound.remove(i);
                for id in &b.slots {
                    engine.remove_slot(*id).map_err(|e| e.to_string())?;
                }
                Some(b.binding)
            }
            None => None,
        };
        let bound = match self.open(engine, kind, name, offline.as_ref()) {
            Ok(bound) => bound,
            Err(e) => {
                // Still missing: keep holding its channels.
                if let Some(b) = offline {
                    let parked = Self::park_offline(engine, b).map_err(|pe| format!("{e}; {pe}"))?;
                    self.bound.push(parked);
                }
                return Err(e);
            }
        };
        let ids = bound.slots.clone();
        self.bound.push(bound);
        self.save()?;
        Ok(ids)
    }

    /// Closes the device owning `slot` (all its slots) and saves the bindings.
    /// Returns `Ok(false)` if the slot is not a device slot.
    pub fn remove(&mut self, engine: &mut Engine, slot: u32) -> Result<bool, String> {
        let Some(i) = self.bound.iter().position(|b| b.slots.contains(&slot)) else { return Ok(false) };
        let b = self.bound.remove(i);
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
        let bindings = std::mem::take(&mut self.saved.devices);
        let mut warnings = Vec::new();
        for b in bindings {
            match self.open(engine, b.kind, &b.name, Some(&b)) {
                Ok(bound) => self.bound.push(bound),
                Err(e) => {
                    warnings.push(format!("{} is offline: {e}", b.device()));
                    match Self::park_offline(engine, b) {
                        Ok(parked) => self.bound.push(parked),
                        Err(e) => warnings.push(e),
                    }
                }
            }
        }
        warnings
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
            Ok(id) => Ok(Bound { binding: b, slots: vec![id], handles: Vec::new() }),
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
            Command::RemoveSlot { id } => match self.remove(engine, *id) {
                Ok(true) => Response::Ok,
                Ok(false) => return None,
                Err(e) => Response::Error(e),
            },
            _ => return None,
        })
    }

    /// Current bindings (for tests and diagnostics).
    pub fn bindings(&self) -> Vec<Binding> {
        self.bound.iter().map(|b| b.binding.clone()).collect()
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let saved = Saved { master: self.master.clone(), devices: self.bindings() };
        let json = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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
        }
    }

    fn open(
        &mut self,
        engine: &mut Engine,
        kind: DeviceKind,
        name: &str,
        at: Option<&Binding>,
    ) -> Result<Bound, String> {
        let device = format!("{}:{}", kind.prefix(), name);
        match kind {
            DeviceKind::Asio => self.open_asio(engine, name, device, at),
            DeviceKind::WasapiRender | DeviceKind::WasapiCapture | DeviceKind::AppCapture => {
                let target = match kind {
                    DeviceKind::WasapiRender => {
                        let ep = find_endpoint(Direction::Render, name).map_err(|e| e.to_string())?;
                        Target::Endpoint { id: ep.id, direction: Direction::Render }
                    }
                    DeviceKind::WasapiCapture => {
                        let ep = find_endpoint(Direction::Capture, name).map_err(|e| e.to_string())?;
                        Target::Endpoint { id: ep.id, direction: Direction::Capture }
                    }
                    _ => Target::App { pid: find_process(name).map_err(|e| e.to_string())? },
                };
                let mut stream = WasapiStream::open(target).map_err(|e| e.to_string())?;
                let f = stream.format();
                let mut binding =
                    Binding { kind, name: name.to_string(), first_input: 0, inputs: 0, first_output: 0, outputs: 0 };
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
                Ok(Bound { binding, slots: vec![id], handles: vec![Handle::Wasapi(stream)] })
            }
        }
    }

    fn open_asio(
        &mut self,
        engine: &mut Engine,
        name: &str,
        device: String,
        at: Option<&Binding>,
    ) -> Result<Bound, String> {
        let mut dev = (self.asio_open)(name).map_err(|e| e.to_string())?;
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
        if let Err(e) = dev.start(StreamConfig { sample_rate: None, block: Some(block) }, Box::new(callback)) {
            undo(engine, &slots);
            return Err(e.to_string());
        }
        let mut binding = Binding {
            kind: DeviceKind::Asio,
            name: name.to_string(),
            first_input: 0,
            inputs: ins as u32,
            first_output: 0,
            outputs: outs as u32,
        };
        for s in engine.slots().into_iter().filter(|s| slots.contains(&s.id)) {
            if s.inputs > 0 {
                binding.first_input = s.first_input;
            }
            if s.outputs > 0 {
                binding.first_output = s.first_output;
            }
        }
        Ok(Bound { binding, slots, handles: vec![Handle::Asio(dev)] })
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
