//! `confluence-cli`: command-line client for the Confluence Control API.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use confluence_api::{Command, Response};

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
                format!(
                    "#{:<3} {:<24} {:?}  in {}+{}  out {}+{}",
                    s.id, s.name, s.role, s.first_input, s.inputs, s.first_output, s.outputs
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Response::Health { blocks, slots } => {
            let mut lines = vec![format!("engine blocks: {blocks}")];
            lines.extend(slots.iter().map(|h| {
                format!(
                    "#{:<3} xruns {}/{}  fill {:.0}/{:.0}  drift {:+.1} ppm  correction {:+.1} ppm",
                    h.id, h.underruns, h.overruns, h.fill_frames, h.target_frames, h.device_ppm, h.correction_ppm
                )
            }));
            lines.join("\n")
        }
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
    use confluence_api::PointState;

    #[test]
    fn negative_gain_parses() {
        let cli = Cli::try_parse_from(["confluence-cli", "set", "1", "2", "--gain", "-6.5", "--invert"]).unwrap();
        assert_eq!(
            cli.command.to_command(),
            Command::SetPoint { input: 1, output: 2, gain_db: -6.5, mute: false, invert: true }
        );
    }

    #[test]
    fn routes_render_one_per_line() {
        let r = Response::Points(vec![
            PointState { input: 0, output: 1, gain_db: -6.0, mute: false, invert: true },
            PointState { input: 3, output: 1, gain_db: 0.0, mute: true, invert: false },
        ]);
        assert_eq!(render(&r), "in    0 -> out    1    -6.0 dB inverted\nin    3 -> out    1    +0.0 dB muted");
    }
}
