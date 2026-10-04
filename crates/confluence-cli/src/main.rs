//! `confluence-cli`: command-line client for the Confluence Control API.

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use confluence_api::{Command, DeviceKind, Response};

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
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Asio,
    WasapiOut,
    WasapiIn,
    App,
    Vasio,
    Vaio,
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
        }
    }
}

fn render(resp: &Response) -> String {
    match resp {
        Response::Ok => "ok".into(),
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
        Response::Status(s) => format!("{s:?}"),
    }
}

#[cfg(windows)]
fn main() -> ExitCode {
    use std::time::Duration;

    use confluence_engine::ipc::{default_pipe_name, PipeClient};

    let cli = Cli::parse();
    let pipe = cli.pipe.clone().unwrap_or_else(default_pipe_name);
    let mut client = match PipeClient::connect(&pipe, Duration::from_secs(2)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot reach engine on pipe '{pipe}': {e}");
            return ExitCode::FAILURE;
        }
    };
    match client.call(cli.command.to_command()) {
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

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("confluence-cli runs on Windows only");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{PointState, SlotHealth};

    #[test]
    fn negative_gain_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "set", "1", "2", "--gain", "-6.5", "--invert"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::SetPoint { input: 1, output: 2, gain_db: -6.5, mute: false, invert: true }
        );
    }

    #[test]
    fn add_device_vaio_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "add-device", "vaio", "1"]).unwrap();
        assert_eq!(cli.command.to_command(), Command::AddDevice { kind: DeviceKind::Vaio, name: "1".into() });
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
        let mut unexplained = h(5, false, 0, 0);
        unexplained.attached = Some(false);
        let text = render(&Response::Health { blocks: 9, slots: vec![unexplained], notices: Vec::new() });
        assert!(text.contains("nothing attached"), "{text}");
    }
}
