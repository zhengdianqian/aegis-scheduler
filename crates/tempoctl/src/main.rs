// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tempo_core::controller::{Controller, DemandSample};
use tempo_core::{Config, DeviceProfile};

#[derive(Debug, Parser)]
#[command(name = "tempoctl", about = "Aegis control and offline replay CLI")]
struct Cli {
    #[arg(long, default_value = "/data/adb/tempo/config.toml")]
    config: PathBuf,

    /// Overrides daemon.profile_path from the configuration.
    #[arg(long)]
    profile: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Query the running daemon over its Unix socket.
    Status {
        #[arg(long, default_value = "/data/adb/tempo/run/control.sock")]
        socket: PathBuf,
    },
    /// Print the parsed device profile as JSON.
    Profile,
    /// Print the parsed configuration as TOML.
    Config,
    /// Validate configuration and profile together.
    Validate,
    /// Print live frequency, charging and thermal state.
    Power {
        #[arg(long, default_value = "/data/adb/tempo/run/control.sock")]
        socket: PathBuf,
    },
    /// Tail the daemon log.
    Logs {
        #[arg(long, default_value = "/data/adb/tempo/run/control.sock")]
        socket: PathBuf,
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Restore the cpufreq policies to their pre-write values.
    Restore {
        #[arg(long, default_value = "/data/adb/tempo/run/control.sock")]
        socket: PathBuf,
    },
    /// Validate and apply a TOML file through the running daemon.
    Apply {
        #[arg(long, default_value = "/data/adb/tempo/run/control.sock")]
        socket: PathBuf,
        #[arg(long)]
        file: PathBuf,
    },
    /// Offline replay: feed one demand sample and print decisions.
    Decide {
        /// Comma-separated total busy ratios per cluster, 0..1.
        #[arg(long, default_value = "0.5")]
        busy: String,
        /// Comma-separated foreground busy ratios per cluster, 0..1.
        #[arg(long, default_value = "0.0")]
        fg_busy: String,
        /// Time between screen-on and the demand sample, milliseconds.
        #[arg(long, default_value_t = 200)]
        idle_ms: u64,
        /// on or off.
        #[arg(long, default_value = "on")]
        screen: String,
        /// Simulate a touch contact at t=0.
        #[arg(long)]
        touch: bool,
        /// Optional thermal cap fraction, 0.05..1.0.
        #[arg(long)]
        thermal: Option<f32>,
        /// Advance through one-shot deadlines up to this many milliseconds.
        #[arg(long, default_value_t = 300)]
        advance_ms: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("tempoctl: {message}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let config = Config::load(&cli.config).map_err(|error| error.to_string())?;
    let profile_path = cli
        .profile
        .clone()
        .unwrap_or_else(|| PathBuf::from(&config.daemon.profile_path));
    let profile = DeviceProfile::load(&profile_path).map_err(|error| error.to_string())?;

    match cli.command {
        Command::Status { socket } => {
            let response = query_daemon(&socket, "{\"cmd\":\"status\"}")?;
            println!("{response}");
            Ok(())
        }
        Command::Profile => {
            let text = serde_json::to_string_pretty(&profile).map_err(|error| error.to_string())?;
            println!("{text}");
            Ok(())
        }
        Command::Config => {
            let text = config.to_toml_string().map_err(|error| error.to_string())?;
            println!("{text}");
            Ok(())
        }
        Command::Validate => {
            let response = serde_json::json!({
                "ok": true,
                "config": config.schema,
                "profile": profile.schema,
                "clusters": profile.clusters.len(),
                "platform": profile.platform,
            });
            println!("{}", serde_json::to_string_pretty(&response).unwrap());
            Ok(())
        }
        Command::Power { socket } => {
            let response = query_daemon(&socket, "{\"cmd\":\"power.get\"}")?;
            println!("{response}");
            Ok(())
        }
        Command::Logs { socket, lines } => {
            let request = format!("{{\"cmd\":\"log.tail\",\"lines\":{lines}}}");
            let response = query_daemon(&socket, &request)?;
            println!("{response}");
            Ok(())
        }
        Command::Restore { socket } => {
            let response = query_daemon(&socket, "{\"cmd\":\"scheduler.restore\"}")?;
            println!("{response}");
            Ok(())
        }
        Command::Apply { socket, file } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|error| format!("read {}: {error}", file.display()))?;
            let request = serde_json::json!({ "cmd": "config.apply", "toml": text }).to_string();
            let response = query_daemon(&socket, &request)?;
            println!("{response}");
            Ok(())
        }
        Command::Decide {
            busy,
            fg_busy,
            idle_ms,
            screen,
            touch,
            thermal,
            advance_ms,
        } => run_decide(
            config, profile, busy, fg_busy, idle_ms, screen, touch, thermal, advance_ms,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_decide(
    config: Config,
    profile: DeviceProfile,
    busy: String,
    fg_busy: String,
    idle_ms: u64,
    screen: String,
    touch: bool,
    thermal: Option<f32>,
    advance_ms: u64,
) -> Result<(), String> {
    let cluster_count = profile.clusters.len();
    let busy = parse_ratios(&busy, cluster_count, "busy")?;
    let fg_busy = parse_ratios(&fg_busy, cluster_count, "fg-busy")?;
    let screen_on = match screen.as_str() {
        "on" => true,
        "off" => false,
        other => return Err(format!("screen must be on/off, got {other}")),
    };

    let mut controller = Controller::new(profile, config);
    let mut trace = Vec::new();

    let decisions = controller.on_screen(0, screen_on);
    trace.push(serde_json::json!({ "at_ms": 0, "event": "screen", "decisions": decisions }));

    if touch {
        let decisions = controller.on_touch(0);
        trace.push(serde_json::json!({ "at_ms": 0, "event": "touch", "decisions": decisions }));
    }
    if let Some(cap) = thermal {
        let decisions = controller.on_thermal_cap(0, Some(cap));
        trace.push(serde_json::json!({ "at_ms": 0, "event": "thermal", "decisions": decisions }));
    }

    let ts_us = idle_ms.saturating_mul(1_000);
    let decisions = controller.on_demand(DemandSample {
        ts_us,
        cluster_busy: busy,
        foreground_busy: fg_busy,
        foreground_active: false,
    });
    trace.push(serde_json::json!({ "at_ms": idle_ms, "event": "demand", "decisions": decisions }));

    let horizon = ts_us.saturating_add(advance_ms.saturating_mul(1_000));
    let mut guard = 0;
    while let Some(deadline) = controller.next_deadline(ts_us) {
        if deadline.at_us > horizon || guard >= 16 {
            break;
        }
        guard += 1;
        let decisions = controller.on_deadline(deadline.at_us);
        trace.push(serde_json::json!({
            "at_ms": deadline.at_us / 1_000,
            "event": format!("{:?}", deadline.kind),
            "decisions": decisions,
        }));
    }

    let output = serde_json::json!({
        "ok": true,
        "quiet": controller.is_quiet(),
        "next_deadline": controller.next_deadline(horizon).map(|deadline| serde_json::json!({
            "at_ms": deadline.at_us / 1_000,
            "kind": format!("{:?}", deadline.kind),
        })),
        "trace": trace,
    });
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
    Ok(())
}

fn parse_ratios(text: &str, expected: usize, name: &str) -> Result<Vec<f32>, String> {
    let values: Vec<f32> = text
        .split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{name} must be comma-separated numbers"))?;
    if values.len() != expected {
        return Err(format!(
            "{name} needs {expected} values (one per cluster), got {}",
            values.len()
        ));
    }
    Ok(values)
}

#[cfg(unix)]
fn query_daemon(socket: &PathBuf, request: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("connect {}: {error}", socket.display()))?;
    writeln!(stream, "{request}").map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    if line.trim().is_empty() {
        return Err("empty response from daemon".to_owned());
    }
    Ok(line.trim().to_owned())
}

#[cfg(not(unix))]
fn query_daemon(_socket: &PathBuf, _request: &str) -> Result<String, String> {
    Err("the control socket is available only on Linux/Android".to_owned())
}
