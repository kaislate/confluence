//! `confluence-cli`: command-line client for the Confluence Control API.

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use confluence_api::{BusRef, Change, Command, DeviceKind, EngineStatus, Event, PosId, PositionStatus, Response};

#[derive(Parser)]
#[command(version, about = "Control a running Confluence engine")]
struct Cli {
    /// Pipe name (default: confluence-<USERNAME>).
    #[arg(long, global = true)]
    pipe: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Route matrix input IN to output OUT.
    Set {
        input: u32,
        output: u32,
        /// Gain in dB (−100 … +24).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        gain: f32,
        #[arg(long)]
        mute: bool,
        #[arg(long)]
        invert: bool,
    },
    /// Fade out and remove a route.
    Remove { input: u32, output: u32 },
    /// List routes.
    Points,
    /// List slots and their channel ranges.
    Slots,
    /// Show engine and per-slot clock health.
    Health,
    /// Stop the engine.
    Shutdown,
    /// List audio devices the engine can open.
    Devices,
    /// Open a device as slot(s): `add-device asio "MOTU Gen 5"`, `add-device vasio 1:8x2`, `add-device vaio 1`.
    AddDevice { kind: Kind, name: String },
    /// Close a slot and remove the routes on its channels.
    RemoveSlot { id: u32 },
    /// Create an insert bus (send columns + return rows) of CHANNELS channels.
    AddBus { name: String, channels: u32 },
    /// List the CLAP plugins found on this PC.
    Plugins,
    /// Load plugin ID from the CLAP file PATH onto insert bus BUS (a slot id).
    LoadPlugin { bus: u32, path: String, id: String },
    /// Take the plugin off insert bus BUS.
    UnloadPlugin { bus: u32 },
    /// Open the editor of the plugin on insert bus BUS.
    ShowEditor { bus: u32 },
    /// Close the editor of the plugin on insert bus BUS.
    HideEditor { bus: u32 },
    /// List the scenes.
    Scenes,
    /// Save the current routes and plugin settings as scene NAME.
    SaveScene {
        name: String,
        /// Seconds recalling it takes to glide there (0 to 10).
        #[arg(long, default_value_t = 0.0)]
        morph: f64,
    },
    /// Glide to scene NAME.
    RecallScene { name: String },
    /// Delete scene NAME.
    DeleteScene { name: String },
    /// Show MIDI inputs and bindings.
    Midi,
    /// List the Luau scripts and their status.
    Scripts,
    /// List the other Confluence engines found on the network.
    Peers,
    /// Store the Luau script in FILE as NAME (and start it, unless --disabled).
    SetScript {
        name: String,
        file: std::path::PathBuf,
        #[arg(long)]
        disabled: bool,
    },
    /// Delete script NAME.
    DeleteScript { name: String },
    /// Colour slot SLOT's device: #rrggbb, or `default`.
    SetColor {
        slot: u32,
        #[arg(value_parser = parse_color)]
        color: ColorArg,
    },
    /// The next CC that arrives binds to the gain of route IN → OUT.
    LearnMidi { input: u32, output: u32 },
    /// Handle a MIDI message as if DEVICE sent it (bytes in decimal or 0x hex).
    InjectMidi {
        device: String,
        #[arg(value_parser = parse_byte, num_args = 1..=3)]
        bytes: Vec<u8>,
    },
    /// Set parameter PARAM of the plugin on insert bus BUS.
    SetParam {
        bus: u32,
        param: u32,
        #[arg(allow_negative_numbers = true)]
        value: f64,
    },
    /// Show the engine's master clock, rate, block, DSP load and xruns.
    Status,
    /// Follow every change to routes, slots and devices live, with a status line each second.
    Watch,
    /// List the fixed positions (VASIO A-H, ASIO 1-8, ...) and what each holds.
    Positions,
    /// Put device NAME of KIND in position POS (`fill asio:2 asio "MOTU Gen 5"`); a filled one is swapped.
    Fill {
        #[arg(value_parser = parse_pos)]
        pos: PosId,
        kind: Kind,
        name: String,
    },
    /// Empty position POS: its device is closed and its routes removed.
    Clear {
        #[arg(value_parser = parse_pos)]
        pos: PosId,
    },
    /// Turn VASIO LETTER on (with the DAW's inputs x outputs, e.g. 16x4) or off.
    Vasio {
        #[arg(value_parser = parse_vasio_letter)]
        letter: PosId,
        state: OnOff,
        #[arg(value_parser = parse_shape)]
        shape: Option<(u32, u32)>,
    },
    /// The master clock from the next start: an ASIO position (`asio:1`) or `internal`.
    Master {
        #[arg(value_parser = parse_master)]
        pos: MasterArg,
    },
    /// Show every slot channel's level once.
    Meters,
    /// Reset the latched clip indicators.
    ClearClip,
    /// Give slot SLOT's device a custom name (or with --in N / --out N one of
    /// its channels); --clear removes it.
    Name {
        slot: u32,
        #[arg(long = "in", value_parser = parse_channel, conflicts_with = "output")]
        input: Option<u32>,
        #[arg(long = "out", value_parser = parse_channel)]
        output: Option<u32>,
        #[arg(long, conflicts_with = "text")]
        clear: bool,
        #[arg(required_unless_present = "clear")]
        text: Option<String>,
    },
    /// List the custom names given to devices and channels.
    Names,
}

/// A channel number as typed (1-based), as an index.
fn parse_channel(s: &str) -> Result<u32, String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n - 1),
        _ => Err(format!("{s}: channels are numbered from 1")),
    }
}

/// Every custom name, device by device.
fn render_names(slots: &[confluence_api::SlotState]) -> String {
    let mut lines = Vec::new();
    for s in slots {
        let mut mine = Vec::new();
        if let Some(l) = &s.label {
            mine.push(format!("#{} {}: {l}", s.id, s.name));
        }
        for (dir, labels, names) in
            [("in", &s.input_labels, &s.input_names), ("out", &s.output_labels, &s.output_names)]
        {
            for (i, l) in labels.iter().enumerate() {
                if let Some(l) = l {
                    let device = names.get(i).map(String::as_str).unwrap_or("");
                    mine.push(format!("  #{} {dir} {} ({device}): {l}", s.id, i + 1));
                }
            }
        }
        lines.extend(mine);
    }
    if lines.is_empty() {
        "no custom names".into()
    } else {
        lines.join("\n")
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

/// `master` argument: an ASIO position, or `None` for the internal clock.
#[derive(Clone, Copy, Debug)]
struct MasterArg(Option<PosId>);

fn parse_pos(s: &str) -> Result<PosId, String> {
    s.parse::<PosId>().map_err(|e| format!("{s}: {e}"))
}

fn parse_vasio_letter(s: &str) -> Result<PosId, String> {
    parse_pos(&format!("vasio:{}", s.trim().to_ascii_uppercase()))
}

/// `IxO` as the DAW sees it (16x4: 16 inputs, 4 outputs), as the engine's
/// (inputs, outputs): the DAW's outputs are the engine's inputs.
fn parse_shape(s: &str) -> Result<(u32, u32), String> {
    let (i, o) = s.split_once(['x', 'X']).ok_or_else(|| format!("{s}: expected INxOUT, e.g. 8x8"))?;
    let n = |t: &str| t.trim().parse::<u32>().map_err(|e| format!("{s}: {e}"));
    Ok((n(o)?, n(i)?))
}

fn parse_master(s: &str) -> Result<MasterArg, String> {
    if s.eq_ignore_ascii_case("internal") {
        return Ok(MasterArg(None));
    }
    parse_pos(s).map(|p| MasterArg(Some(p)))
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Asio,
    WasapiOut,
    WasapiIn,
    App,
    Vasio,
    Vaio,
    NetOut,
    NetIn,
}

impl From<Kind> for DeviceKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Asio => DeviceKind::Asio,
            Kind::WasapiOut => DeviceKind::WasapiRender,
            Kind::WasapiIn => DeviceKind::WasapiCapture,
            Kind::App => DeviceKind::AppCapture,
            Kind::Vasio => DeviceKind::Vasio,
            Kind::Vaio => DeviceKind::Vaio,
            Kind::NetOut => DeviceKind::NetSend,
            Kind::NetIn => DeviceKind::NetReceive,
        }
    }
}

impl Cmd {
    fn to_command(&self) -> Command {
        match *self {
            Cmd::Set { input, output, gain, mute, invert } => {
                Command::SetPoint { input, output, gain_db: gain, mute, invert }
            }
            Cmd::Remove { input, output } => Command::RemovePoint { input, output },
            Cmd::Points => Command::ListPoints,
            Cmd::Slots => Command::ListSlots,
            Cmd::Health => Command::Health,
            Cmd::Shutdown => Command::Shutdown,
            Cmd::Devices => Command::ListDevices,
            Cmd::AddDevice { kind, ref name } => Command::AddDevice { kind: kind.into(), name: name.clone() },
            Cmd::RemoveSlot { id } => Command::RemoveSlot { id },
            Cmd::AddBus { ref name, channels } => {
                Command::AddBus { name: name.clone(), channels, first_input: None, first_output: None }
            }
            Cmd::Plugins => Command::ListPlugins,
            Cmd::LoadPlugin { bus, ref path, ref id } => {
                Command::LoadPlugin { bus: BusRef::Id(bus), path: path.clone(), plugin_id: id.clone() }
            }
            Cmd::UnloadPlugin { bus } => Command::UnloadPlugin { bus: BusRef::Id(bus) },
            Cmd::ShowEditor { bus } => Command::ShowEditor { bus: BusRef::Id(bus) },
            Cmd::Scenes => Command::ListScenes,
            Cmd::SaveScene { ref name, morph } => {
                Command::SaveScene { name: name.clone(), morph_ms: (morph.clamp(0.0, 10.0) * 1000.0).round() as u32 }
            }
            Cmd::RecallScene { ref name } => Command::RecallScene { name: name.clone() },
            Cmd::DeleteScene { ref name } => Command::DeleteScene { name: name.clone() },
            Cmd::Midi | Cmd::Scripts | Cmd::Peers => Command::Subscribe,
            // Reads a file: see `script_command`.
            Cmd::SetScript { ref name, .. } => {
                Command::SetScript { name: name.clone(), source: String::new(), enabled: false }
            }
            Cmd::DeleteScript { ref name } => Command::DeleteScript { name: name.clone() },
            Cmd::SetColor { slot, color } => Command::SetSlotColor { id: slot, color: color.0 },
            Cmd::LearnMidi { input, output } => Command::LearnMidi { input, output },
            Cmd::InjectMidi { ref device, ref bytes } => {
                Command::InjectMidi { device: device.clone(), bytes: bytes.clone() }
            }
            Cmd::HideEditor { bus } => Command::HideEditor { bus: BusRef::Id(bus) },
            Cmd::SetParam { bus, param, value } => Command::SetParam { bus: BusRef::Id(bus), param, value },
            Cmd::Status => Command::Status,
            Cmd::Watch | Cmd::Positions => Command::Subscribe,
            Cmd::Fill { pos, kind, ref name } => Command::FillPosition { pos, kind: kind.into(), name: name.clone() },
            Cmd::Clear { pos } => Command::ClearPosition { pos },
            Cmd::Vasio { letter, state, shape } => {
                let on = matches!(state, OnOff::On);
                Command::SetVirtual { pos: letter, on, shape: shape.filter(|_| on) }
            }
            Cmd::Master { pos } => Command::SetMaster { pos: pos.0 },
            Cmd::Meters => Command::SubscribeMeters,
            Cmd::ClearClip => Command::ClearClip,
            Cmd::Name { slot, input, output, clear, ref text } => Command::SetSlotLabel {
                id: slot,
                channel: input
                    .map(|index| confluence_api::ChannelRef { input: true, index })
                    .or(output.map(|index| confluence_api::ChannelRef { input: false, index })),
                name: if clear { None } else { text.clone() },
            },
            Cmd::Names => Command::Subscribe,
        }
    }
}

/// `set-script`: the command with the file's contents.
fn script_command(cmd: &Cmd) -> Result<Command, String> {
    match cmd {
        Cmd::SetScript { name, file, disabled } => {
            let source = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            Ok(Command::SetScript { name: name.clone(), source, enabled: !disabled })
        }
        other => Ok(other.to_command()),
    }
}

/// One line per position, from a state snapshot.
fn render_positions(positions: &[confluence_api::PositionState]) -> String {
    if positions.is_empty() {
        return "no positions (an engine older than API 10?)".into();
    }
    positions
        .iter()
        .map(|p| {
            let status = match p.status {
                PositionStatus::Empty => "Empty".to_string(),
                PositionStatus::Filled { online: true } => "Filled".into(),
                PositionStatus::Filled { online: false } => "Filled (offline)".into(),
                PositionStatus::Off => "Off".into(),
                PositionStatus::On { online } => match (&p.daw, online) {
                    (Some(daw), _) => format!("On (DAW: {daw})"),
                    (None, true) => "On (connected)".into(),
                    (None, false) => "On".into(),
                },
            };
            let mut line = format!("{:<10} {status}", p.pos.to_string());
            if let Some(d) = p.device.as_ref().filter(|_| !p.pos.group.is_virtual()) {
                line.push_str(&format!("  {}", d.name));
            }
            if let Some((i, o)) = p.shape {
                line.push_str(&format!("  {o}x{i}")); // as the DAW sees it
            }
            if !p.slots.is_empty() {
                line.push_str(&format!("  slots {:?}", p.slots));
            }
            if p.master {
                line.push_str("  MASTER");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every slot channel's level as a text bar (-60 to 0 dB), from one meter frame.
fn render_meters(s: &confluence_api::State, f: &confluence_api::MeterFrame) -> String {
    let bar = |b: u8| {
        let db = confluence_api::byte_db(b);
        let filled = if db.is_finite() { (((db + 60.0) / 6.0).round().clamp(0.0, 10.0)) as usize } else { 0 };
        let text = if db.is_finite() { format!("{db:6.1}") } else { "  -inf".into() };
        format!("{}{} {text}", "\u{2588}".repeat(filled), "\u{2591}".repeat(10 - filled))
    };
    let mut lines = Vec::new();
    for slot in &s.slots {
        for k in 0..slot.inputs {
            let i = (slot.first_input + k).checked_sub(f.first_input).map(|i| i as usize);
            let peak = i.and_then(|i| f.inputs.get(i)).map_or(0, |m| m[0]);
            lines.push(format!("#{} in {} {}", slot.id, k + 1, bar(peak)));
        }
        for k in 0..slot.outputs {
            let o = (slot.first_output + k).checked_sub(f.first_output).map(|o| o as usize);
            let peak = o.and_then(|o| f.outputs.get(o)).map_or(0, |m| m[0]);
            lines.push(format!("#{} out {} {}", slot.id, k + 1, bar(peak)));
        }
    }
    if lines.is_empty() {
        return "no slots".into();
    }
    lines.join("\n")
}

/// Subscribes with meters and prints the first frame.
#[cfg(windows)]
fn meters(pipe: &str) -> ExitCode {
    use std::time::Duration;

    use confluence_client::Subscription;

    let (state, mut sub) = match Subscription::connect_with_meters(pipe, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot subscribe to engine on pipe '{pipe}': {e}");
            return ExitCode::FAILURE;
        }
    };
    loop {
        match sub.recv() {
            Ok(Event::Meters(f)) => {
                println!("{}", render_meters(&state, &f));
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("engine stream ended: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// The engines found on the network.
fn render_peers(peers: &[confluence_api::Peer]) -> String {
    if peers.is_empty() {
        return "no other engines found".into();
    }
    peers.iter().map(|p| format!("{}  {}:{}", p.name, p.address, p.port)).collect::<Vec<_>>().join("\n")
}

/// Scripts and their status, from a state snapshot.
fn render_scripts(s: &confluence_api::State) -> String {
    if s.scripts.is_empty() {
        return "no scripts".into();
    }
    s.scripts
        .iter()
        .map(|sc| {
            let status = match &sc.status {
                confluence_api::ScriptStatus::Running => "running".to_string(),
                confluence_api::ScriptStatus::Disabled => "disabled".to_string(),
                confluence_api::ScriptStatus::Stopped(why) => format!("stopped: {why}"),
            };
            format!("{}  {status}", sc.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// MIDI inputs, bindings and learn, from a state snapshot.
fn render_midi(s: &confluence_api::State) -> String {
    let mut lines = vec![if s.midi_inputs.is_empty() {
        "MIDI inputs: none".to_string()
    } else {
        format!("MIDI inputs: {}", s.midi_inputs.join(", "))
    }];
    if s.midi_bindings.is_empty() {
        lines.push("no bindings".into());
    }
    for b in &s.midi_bindings {
        lines.push(format!("CC {} ch {} {} -> in {} out {}", b.cc, b.channel, b.device, b.input, b.output));
    }
    if let Some((i, o)) = s.midi_learning {
        lines.push(format!("learning: in {i} -> out {o}"));
    }
    lines.join("\n")
}

/// A colour argument: `None` for the default.
#[derive(Clone, Copy, Debug)]
struct ColorArg(Option<confluence_api::Rgb>);

/// `#rrggbb` (the `#` is optional), or `default`.
fn parse_color(s: &str) -> Result<ColorArg, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("default") {
        return Ok(ColorArg(None));
    }
    let hex = t.strip_prefix('#').unwrap_or(t);
    let bad = || format!("{s}: give a colour as #rrggbb, or default");
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
    Ok(ColorArg(Some([byte(0)?, byte(2)?, byte(4)?])))
}

/// A byte given as decimal or `0x` hex.
fn parse_byte(s: &str) -> Result<u8, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => s.parse(),
    };
    r.map_err(|e| format!("{s}: {e}"))
}

fn render(resp: &Response) -> String {
    match resp {
        Response::Ok => "ok".into(),
        Response::Scenes(s) if s.is_empty() => "no scenes".into(),
        Response::Scenes(s) => s
            .iter()
            .map(|s| {
                format!(
                    "{}  morph {:.1} s  {} routes  {} parameters",
                    s.name,
                    s.morph_ms as f64 / 1000.0,
                    s.routes,
                    s.params
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Response::Plugins(p) if p.is_empty() => "no CLAP plugins found".into(),
        Response::Plugins(p) => p
            .iter()
            .map(|p| format!("{} ({}) {}  {}  {}", p.name, p.vendor, p.version, p.id, p.path))
            .collect::<Vec<_>>()
            .join("\n"),
        Response::Error(e) => format!("error: {e}"),
        Response::Points(points) if points.is_empty() => "no routes".into(),
        Response::Points(points) => points
            .iter()
            .map(|p| {
                let flags =
                    format!("{}{}", if p.mute { " muted" } else { "" }, if p.invert { " inverted" } else { "" });
                format!("in {:>4} -> out {:>4}  {:+6.1} dB{flags}", p.input, p.output, p.gain_db)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Response::Slots(slots) if slots.is_empty() => "no slots".into(),
        Response::Slots(slots) => slots
            .iter()
            .map(|s| {
                let status = if s.online { "" } else { "  OFFLINE" };
                format!(
                    "#{:<3} {:<24} {:?}  in {}+{}  out {}+{}{status}",
                    s.id, s.name, s.role, s.first_input, s.inputs, s.first_output, s.outputs
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Response::Health { blocks, slots, notices } => {
            let mut lines = vec![format!("engine blocks: {blocks}")];
            lines.extend(notices.iter().map(|n| format!("note: {n}")));
            lines.extend(slots.iter().map(|h| {
                let mut line = format!(
                    "#{:<3} xruns {}/{}  fill {:.0}/{:.0}  drift {:+.1} ppm  correction {:+.1} ppm",
                    h.id, h.underruns, h.overruns, h.fill_frames, h.target_frames, h.device_ppm, h.correction_ppm
                );
                if h.device_lost {
                    line.push_str("  DEVICE LOST");
                }
                if h.device_faults > 0 {
                    line.push_str(&format!("  {} faults", h.device_faults));
                }
                if h.attached == Some(false) {
                    line.push_str(&format!("  {}", h.idle_note.as_deref().unwrap_or("nothing attached")));
                }
                if h.driver_requests > 0 {
                    line.push_str(&format!("  {} driver requests (re-add the device)", h.driver_requests));
                }
                if let Some(n) = h.net {
                    line.push_str(&format!(
                        "  net {} packets, {} lost, {} late, {} reordered",
                        n.packets, n.lost, n.late, n.reordered
                    ));
                }
                line
            }));
            lines.join("\n")
        }
        Response::Devices(devices) if devices.is_empty() => "no devices".into(),
        Response::Devices(devices) => devices
            .iter()
            .map(|d| format!("{:<10} {:<40} in {:>3}  out {:>3}", d.kind.prefix(), d.name, d.inputs, d.outputs))
            .collect::<Vec<_>>()
            .join("\n"),
        Response::SlotsAdded(ids) => {
            format!("added slot(s) {}", ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", "))
        }
        Response::Added { ids, version } => format!(
            "added slot(s) {} (version {version})",
            ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
        ),
        Response::Applied { version } => format!("ok (version {version})"),
        Response::Snapshot(s) => {
            format!("state version {} ({} slots, {} points)", s.version, s.slots.len(), s.points.len())
        }
        Response::Event(e) => format!("{e:?}"),
        Response::Status(s) => render_status(s),
    }
}

#[cfg(windows)]
fn main() -> ExitCode {
    use std::time::Duration;

    use confluence_client::{default_pipe_name, Client};

    let cli = Cli::parse();
    let pipe = cli.pipe.clone().unwrap_or_else(default_pipe_name);
    if matches!(cli.command, Cmd::Watch) {
        return watch(&pipe);
    }
    if matches!(cli.command, Cmd::Meters) {
        return meters(&pipe);
    }
    let mut client = match Client::connect(&pipe, Duration::from_secs(2)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot reach engine on pipe '{pipe}': {e}");
            return ExitCode::FAILURE;
        }
    };
    let command = match script_command(&cli.command) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match client.call(command) {
        Ok(Response::Snapshot(s)) if matches!(cli.command, Cmd::Midi) => {
            println!("{}", render_midi(&s));
            ExitCode::SUCCESS
        }
        Ok(Response::Snapshot(s)) if matches!(cli.command, Cmd::Scripts) => {
            println!("{}", render_scripts(&s));
            ExitCode::SUCCESS
        }
        Ok(Response::Snapshot(s)) if matches!(cli.command, Cmd::Names) => {
            println!("{}", render_names(&s.slots));
            ExitCode::SUCCESS
        }
        Ok(Response::Snapshot(s)) if matches!(cli.command, Cmd::Positions) => {
            println!("{}", render_positions(&s.positions));
            ExitCode::SUCCESS
        }
        Ok(Response::Snapshot(s)) if matches!(cli.command, Cmd::Peers) => {
            println!("{}", render_peers(&s.peers));
            ExitCode::SUCCESS
        }
        Ok(resp) => {
            println!("{}", render(&resp));
            if matches!(resp, Response::Error(_)) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn render_status(s: &EngineStatus) -> String {
    format!(
        "master {}  {:.0} Hz  block {}  dsp {:.1}%  xruns {}  blocks {}",
        s.master,
        s.sample_rate,
        s.block,
        s.dsp_load * 100.0,
        s.xruns,
        s.blocks
    )
}

fn render_change(c: &Change) -> String {
    match c {
        Change::PositionsChanged(p) => format!("positions changed ({} positions)", p.len()),
        Change::PointSet(p) => format!(
            "route {} -> {} {:+.1} dB{}{}",
            p.input,
            p.output,
            p.gain_db,
            if p.mute { " muted" } else { "" },
            if p.invert { " inverted" } else { "" }
        ),
        Change::PointRemoved { input, output } => format!("route {input} -> {output} removed"),
        Change::SlotAdded(s) => format!("slot #{} {} added", s.id, s.name),
        Change::SlotChanged(s) => format!("slot #{} {} {}", s.id, s.name, if s.online { "online" } else { "offline" }),
        Change::SlotRemoved { id } => format!("slot #{id} removed"),
        Change::DevicesChanged(d) => format!("{} devices available", d.len()),
        Change::NoticesChanged(n) if n.is_empty() => "notices: none".into(),
        Change::NoticesChanged(n) => format!("notices: {}", n.join("; ")),
        Change::PluginsChanged(found, bad) => {
            format!("{} plugins found, {} failed the load check", found.len(), bad.len())
        }
        Change::BusPluginSet(p) => format!("bus #{}: {} {:?}", p.bus, p.info.name, p.status),
        Change::BusPluginRemoved { bus } => format!("bus #{bus}: plugin removed"),
        Change::MidiChanged(inputs, bindings, learning) => format!(
            "MIDI: {} inputs, {} bindings{}",
            inputs.len(),
            bindings.len(),
            learning.map(|(i, o)| format!(", learning {i} -> {o}")).unwrap_or_default()
        ),
        Change::ScriptsChanged(s) => format!("{} scripts", s.len()),
        Change::PeersChanged(p) => format!("{} engines on the network", p.len()),
        Change::ScenesChanged(s, current, morphing) => format!(
            "{} scenes, current: {}{}",
            s.len(),
            current.as_deref().unwrap_or("none"),
            if *morphing { " (morphing)" } else { "" }
        ),
        Change::ParamChanged { bus, id, text, .. } => format!("bus #{bus}: parameter {id} = {text}"),
    }
}

/// One line per change; telemetry is shown separately (once a second).
fn render_event(e: &Event) -> Option<String> {
    match e {
        Event::Meters(_) => None,
        Event::Changed { version, changes } => {
            Some(changes.iter().map(|c| format!("v{version}  {}", render_change(c))).collect::<Vec<_>>().join("\n"))
        }
        Event::Telemetry { .. } => None,
    }
}

/// Subscribes and prints changes as they come, plus a status line each second.
#[cfg(windows)]
fn watch(pipe: &str) -> ExitCode {
    use std::time::{Duration, Instant};

    use confluence_client::Subscription;

    let (state, mut sub) = match Subscription::connect(pipe, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot subscribe to engine on pipe '{pipe}': {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("state version {}: {} slots, {} routes", state.version, state.slots.len(), state.points.len());
    println!("{}", render_status(&state.status));
    let mut last_status = Instant::now();
    loop {
        match sub.recv() {
            Ok(Event::Telemetry { status, .. }) => {
                if last_status.elapsed() >= Duration::from_secs(1) {
                    println!("{}", render_status(&status));
                    last_status = Instant::now();
                }
            }
            Ok(e) => {
                if let Some(text) = render_event(&e) {
                    println!("{text}");
                }
            }
            Err(e) => {
                eprintln!("engine stream ended: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("confluence-cli runs on Windows only");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{Change, EngineStatus, Event, PointState, SlotHealth, SlotState};

    #[test]
    fn positions_render_one_line_each() {
        use confluence_api::{PositionDevice, PositionState};
        let positions = vec![
            PositionState {
                pos: "vasio:A".parse().unwrap(),
                status: PositionStatus::On { online: true },
                device: Some(PositionDevice { kind: DeviceKind::Vasio, name: "1:8x8".into() }),
                shape: Some((2, 8)),
                daw: Some("Ableton Live 12 Suite".into()),
                master: false,
                color: None,
                slots: vec![3],
            },
            PositionState {
                pos: "asio:1".parse().unwrap(),
                status: PositionStatus::Filled { online: true },
                device: Some(PositionDevice { kind: DeviceKind::Asio, name: "GoXLR ASIO Driver".into() }),
                shape: None,
                daw: None,
                master: true,
                color: None,
                slots: vec![1, 2],
            },
        ];
        assert_eq!(
            render_positions(&positions),
            "vasio:A    On (DAW: Ableton Live 12 Suite)  8x2  slots [3]
asio:1     Filled  GoXLR ASIO Driver  slots [1, 2]  MASTER"
        );
    }

    #[test]
    fn name_commands_parse() {
        let parse = |a: &[&str]| {
            let mut v = vec!["confluence-cli"];
            v.extend_from_slice(a);
            Cli::try_parse_from(v).map(|c| c.command.to_command())
        };
        let ch = |input, index| Some(confluence_api::ChannelRef { input, index });
        assert_eq!(
            parse(&["name", "3", "Kick drum"]).unwrap(),
            Command::SetSlotLabel { id: 3, channel: None, name: Some("Kick drum".into()) }
        );
        assert_eq!(
            parse(&["name", "3", "--in", "2", "Snare"]).unwrap(),
            Command::SetSlotLabel { id: 3, channel: ch(true, 1), name: Some("Snare".into()) },
            "channels are typed 1-based"
        );
        assert_eq!(
            parse(&["name", "3", "--out", "1", "--clear"]).unwrap(),
            Command::SetSlotLabel { id: 3, channel: ch(false, 0), name: None }
        );
        assert!(parse(&["name", "3", "--in", "0", "x"]).is_err(), "channel 0 does not exist");
        assert!(parse(&["name", "3"]).is_err(), "a name or --clear is needed");
        assert_eq!(parse(&["names"]).unwrap(), Command::Subscribe);
    }

    #[test]
    fn names_render_devices_and_channels() {
        let mut s = SlotState {
            id: 5,
            name: "VASIO 1".into(),
            device: "vasio:1:2x2".into(),
            role: confluence_api::ClockRole::Strict,
            online: true,
            first_input: 0,
            inputs: 2,
            first_output: 0,
            outputs: 2,
            color: None,
            input_names: vec!["DAW out 1".into(), "DAW out 2".into()],
            output_names: vec!["DAW in 1".into(), "DAW in 2".into()],
            label: Some("Ableton".into()),
            input_labels: vec![Some("Kick".into()), None],
            output_labels: vec![None, None],
        };
        let text = render_names(&[s.clone()]);
        assert!(text.contains("#5 VASIO 1: Ableton"), "{text}");
        assert!(text.contains("in 1 (DAW out 1): Kick"), "{text}");
        s.label = None;
        s.input_labels = vec![None, None];
        assert_eq!(render_names(&[s]), "no custom names");
    }

    #[test]
    fn position_commands_parse() {
        let parse = |a: &[&str]| {
            let mut v = vec!["confluence-cli"];
            v.extend_from_slice(a);
            Cli::try_parse_from(v).map(|c| c.command.to_command())
        };
        assert_eq!(
            parse(&["fill", "asio:2", "asio", "MOTU Gen 5"]).unwrap(),
            Command::FillPosition { pos: "asio:2".parse().unwrap(), kind: DeviceKind::Asio, name: "MOTU Gen 5".into() }
        );
        assert_eq!(
            parse(&["clear", "win-out:1"]).unwrap(),
            Command::ClearPosition { pos: "win-out:1".parse().unwrap() }
        );
        assert_eq!(
            parse(&["vasio", "B", "on", "16x4"]).unwrap(),
            Command::SetVirtual { pos: "vasio:B".parse().unwrap(), on: true, shape: Some((4, 16)) }
        );
        assert_eq!(
            parse(&["vasio", "B", "off"]).unwrap(),
            Command::SetVirtual { pos: "vasio:B".parse().unwrap(), on: false, shape: None }
        );
        assert_eq!(parse(&["master", "asio:1"]).unwrap(), Command::SetMaster { pos: Some("asio:1".parse().unwrap()) });
        assert_eq!(parse(&["master", "internal"]).unwrap(), Command::SetMaster { pos: None });
        assert_eq!(parse(&["clear-clip"]).unwrap(), Command::ClearClip);
        assert!(parse(&["clear", "asio:9"]).is_err());
    }

    #[test]
    fn status_and_events_render_readably() {
        let s = EngineStatus {
            master: "asio:GoXLR ASIO Driver".into(),
            sample_rate: 48_000.0,
            block: 512,
            blocks: 1234,
            dsp_load: 0.125,
            xruns: 2,
        };
        let line = render_status(&s);
        assert!(line.contains("asio:GoXLR ASIO Driver") && line.contains("48000") && line.contains("512"), "{line}");
        assert!(line.contains("12.5%") && line.contains("xruns 2"), "{line}");
        assert_eq!(render(&Response::Status(s.clone())), line);
        let e = Event::Changed {
            version: 7,
            changes: vec![Change::PointRemoved { input: 1, output: 2 }, Change::SlotRemoved { id: 3 }],
        };
        let text = render_event(&e).unwrap();
        assert_eq!(
            text,
            "v7  route 1 -> 2 removed
v7  slot #3 removed"
        );
        assert!(render_event(&Event::Telemetry { status: s, health: Vec::new() }).is_none());
        let cli = Cli::try_parse_from(["confluence-cli", "watch"]).unwrap();
        assert!(matches!(cli.command, Cmd::Watch));
        let cli = Cli::try_parse_from(["confluence-cli", "status"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::Status);
    }

    #[test]
    fn mutations_print_their_version() {
        assert_eq!(render(&Response::Applied { version: 12 }), "ok (version 12)");
    }

    #[test]
    fn negative_gain_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "set", "1", "2", "--gain", "-6.5", "--invert"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::SetPoint { input: 1, output: 2, gain_db: -6.5, mute: false, invert: true }
        );
    }

    #[test]
    fn network_streams_parse_and_peers_are_shown() {
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "net-out", "Lilith/Main:2"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::AddDevice { kind: DeviceKind::NetSend, name: "Lilith/Main:2".into() }
        );
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "net-in", "Lilith/Main"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::AddDevice { kind: DeviceKind::NetReceive, name: "Lilith/Main".into() }
        );
        assert_eq!(render_peers(&[]), "no other engines found");
        let lilith = confluence_api::Peer { name: "Lilith".into(), address: "192.168.50.12".into(), port: 6990 };
        assert_eq!(render_peers(&[lilith]), "Lilith  192.168.50.12:6990");
    }

    #[test]
    fn add_device_vaio_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "vaio", "1"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::AddDevice { kind: DeviceKind::Vaio, name: "1".into() });
    }

    #[test]
    fn midi_state_is_shown() {
        let mut st = confluence_api::State {
            version: 3,
            status: EngineStatus {
                master: "internal".into(),
                sample_rate: 48_000.0,
                block: 256,
                blocks: 0,
                dsp_load: 0.0,
                xruns: 0,
            },
            slots: Vec::new(),
            points: Vec::new(),
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
            scenes: Vec::new(),
            current_scene: None,
            morphing: false,
            midi_inputs: vec!["nanoKONTROL2".into()],
            midi_bindings: vec![confluence_api::MidiBinding {
                device: "nanoKONTROL2".into(),
                channel: 1,
                cc: 7,
                input: 3,
                output: 4,
            }],
            midi_learning: None,
            scripts: Vec::new(),
            peers: Vec::new(),
            positions: Vec::new(),
        };
        let text = render_midi(&st);
        assert!(text.contains("nanoKONTROL2"), "{text}");
        assert!(text.contains("CC 7 ch 1 nanoKONTROL2 -> in 3 out 4"), "{text}");
        st.midi_learning = Some((3, 4));
        assert!(render_midi(&st).contains("learning: in 3 -> out 4"));
    }

    #[test]
    fn set_script_reads_the_file() {
        let dir = std::env::temp_dir().join(format!("confluence-cli-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mute.luau");
        std::fs::write(&file, "function on_midi(m) end").unwrap();
        let cli = Cli::try_parse_from(["confluence-cli", "set-script", "mute", file.to_str().unwrap(), "--disabled"])
            .unwrap();
        assert_eq!(
            script_command(&cli.command).unwrap(),
            Command::SetScript { name: "mute".into(), source: "function on_midi(m) end".into(), enabled: false }
        );
    }

    #[test]
    fn set_color_takes_a_hex_colour_or_default() {
        let parse = |args: &[&str]| {
            let mut all = vec!["confluence-cli"];
            all.extend_from_slice(args);
            Cli::try_parse_from(all).map(|c| c.command.to_command())
        };
        assert_eq!(
            parse(&["set-color", "3", "#40a0FF"]).unwrap(),
            Command::SetSlotColor { id: 3, color: Some([0x40, 0xa0, 0xff]) }
        );
        assert_eq!(
            parse(&["set-color", "3", "40a0ff"]).unwrap(),
            Command::SetSlotColor { id: 3, color: Some([0x40, 0xa0, 0xff]) }
        );
        assert_eq!(parse(&["set-color", "3", "default"]).unwrap(), Command::SetSlotColor { id: 3, color: None });
        assert!(parse(&["set-color", "3", "#12345"]).is_err());
        assert!(parse(&["set-color", "3", "blue"]).is_err());
    }

    #[test]
    fn plugin_commands_parse() {
        let parse = |args: &[&str]| {
            let mut all = vec!["confluence-cli"];
            all.extend_from_slice(args);
            Cli::try_parse_from(all).unwrap().command.to_command()
        };
        assert_eq!(parse(&["plugins"]), Command::ListPlugins);
        assert_eq!(
            parse(&["load-plugin", "4", "C:\\x.clap", "dev.x"]),
            Command::LoadPlugin { bus: BusRef::Id(4), path: "C:\\x.clap".into(), plugin_id: "dev.x".into() }
        );
        assert_eq!(parse(&["unload-plugin", "4"]), Command::UnloadPlugin { bus: BusRef::Id(4) });
        assert_eq!(parse(&["show-editor", "4"]), Command::ShowEditor { bus: BusRef::Id(4) });
        assert_eq!(parse(&["scenes"]), Command::ListScenes);
        assert_eq!(parse(&["save-scene", "Verse"]), Command::SaveScene { name: "Verse".into(), morph_ms: 0 });
        assert_eq!(
            parse(&["save-scene", "Chorus", "--morph", "1.5"]),
            Command::SaveScene { name: "Chorus".into(), morph_ms: 1500 }
        );
        assert_eq!(parse(&["recall-scene", "Verse"]), Command::RecallScene { name: "Verse".into() });
        assert_eq!(parse(&["delete-scene", "Verse"]), Command::DeleteScene { name: "Verse".into() });
        assert_eq!(parse(&["learn-midi", "1", "2"]), Command::LearnMidi { input: 1, output: 2 });
        assert_eq!(parse(&["delete-script", "mute"]), Command::DeleteScript { name: "mute".into() });
        assert_eq!(
            parse(&["inject-midi", "nanoKONTROL2", "0xB0", "7", "100"]),
            Command::InjectMidi { device: "nanoKONTROL2".into(), bytes: vec![0xB0, 7, 100] }
        );
        assert_eq!(parse(&["hide-editor", "4"]), Command::HideEditor { bus: BusRef::Id(4) });
        assert_eq!(
            parse(&["set-param", "4", "1", "-6.5"]),
            Command::SetParam { bus: BusRef::Id(4), param: 1, value: -6.5 }
        );
    }

    #[test]
    fn add_bus_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "add-bus", "Reverb", "2"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::AddBus { name: "Reverb".into(), channels: 2, first_input: None, first_output: None }
        );
    }

    #[test]
    fn device_commands_parse() {
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "asio", "MOTU Gen 5"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::AddDevice { kind: DeviceKind::Asio, name: "MOTU Gen 5".into() });
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "app", "Discord.exe"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::AddDevice { kind: DeviceKind::AppCapture, name: "Discord.exe".into() }
        );
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "vasio", "2:8x2"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::AddDevice { kind: DeviceKind::Vasio, name: "2:8x2".into() });
        let cli = Cli::try_parse_from(["confluence-cli", "remove-slot", "3"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::RemoveSlot { id: 3 });
    }

    #[test]
    fn routes_render_one_per_line() {
        let r = Response::Points(vec![
            PointState { input: 0, output: 1, gain_db: -6.0, mute: false, invert: true },
            PointState { input: 3, output: 1, gain_db: 0.0, mute: true, invert: false },
        ]);
        assert_eq!(render(&r), "in    0 -> out    1    -6.0 dB inverted\nin    3 -> out    1    +0.0 dB muted");
    }

    #[test]
    fn health_flags_device_problems() {
        let h = |id, device_lost, device_faults, driver_requests| SlotHealth {
            id,
            underruns: 0,
            overruns: 0,
            fill_frames: 600.0,
            target_frames: 600.0,
            device_ppm: 1.0,
            correction_ppm: 0.0,
            device_lost,
            device_faults,
            driver_requests,
            attached: None,
            idle_note: None,
            net: None,
        };
        let text = render(&Response::Health {
            blocks: 9,
            slots: vec![h(1, false, 0, 0), h(2, true, 0, 0), h(3, false, 4, 2)],
            notices: Vec::new(),
        });
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines[1].contains("DEVICE"), "{text}");
        assert!(lines[2].contains("DEVICE LOST"), "{text}");
        assert!(lines[3].contains("4 faults") && lines[3].contains("2 driver requests"), "{text}");
        let mut waiting = h(4, false, 0, 0);
        waiting.attached = Some(false);
        waiting.idle_note = Some("no app playing".into());
        let text = render(&Response::Health { blocks: 9, slots: vec![waiting], notices: vec!["x is odd".into()] });
        assert!(text.contains("no app playing") && !text.contains("DAW"), "the slot's own note: {text}");
        assert!(text.contains("note: x is odd"), "{text}");
        let mut stream = h(6, false, 0, 0);
        stream.net = Some(confluence_api::NetStats {
            packets: 900,
            lost: 2,
            late: 1,
            reordered: 5,
            malformed: 0,
            silent_ms: 3,
            mismatched: 0,
        });
        let text = render(&Response::Health { blocks: 9, slots: vec![stream], notices: Vec::new() });
        assert!(text.contains("net 900 packets, 2 lost, 1 late, 5 reordered"), "{text}");
        let mut unexplained = h(5, false, 0, 0);
        unexplained.attached = Some(false);
        let text = render(&Response::Health { blocks: 9, slots: vec![unexplained], notices: Vec::new() });
        assert!(text.contains("nothing attached"), "{text}");
    }
}
