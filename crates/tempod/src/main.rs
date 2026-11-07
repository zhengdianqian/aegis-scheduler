// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::time::{Duration, Instant};

use clap::Parser;
use tempo_core::{Config, DeviceProfile};
use tracing_subscriber::EnvFilter;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod charge;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod event_loop;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod foreground;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod fas;
mod ipc;
mod power;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod probe;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod screen;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod touch;
mod writer;

#[derive(Debug, Parser)]
#[command(
    name = "Aegis",
    about = "Aegis scheduling daemon (event driven, zero periodic polling)"
)]
struct Args {
    #[arg(long, default_value = "/data/adb/tempo/config.toml")]
    config: PathBuf,

    /// Overrides daemon.profile_path from the configuration.
    #[arg(long)]
    profile: Option<PathBuf>,

    #[arg(long, default_value = "/data/adb/tempo/run")]
    run_dir: PathBuf,

    #[arg(long, default_value = "/data/adb/tempo/Aegis.log")]
    log_file: PathBuf,

    /// Keep the cpufreq policies read-only. Used for measurement baselines.
    #[arg(long)]
    observe: bool,

    /// Validate configuration and profile, print a summary, then exit.
    #[arg(long)]
    check: bool,
}

/// Reads an Android system property through bionic, without spawning a
/// process. Used only during startup.
#[cfg(target_os = "android")]
fn android_property(name: &str) -> Option<String> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
    }

    let name = CString::new(name).ok()?;
    let mut value = [0 as c_char; 128];
    let length = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    if length <= 0 {
        return None;
    }
    Some(
        value[..length as usize]
            .iter()
            .map(|byte| *byte as u8 as char)
            .collect(),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn boot_completed() -> bool {
    #[cfg(target_os = "android")]
    {
        matches!(android_property("sys.boot_completed").as_deref(), Some("1"))
    }
    #[cfg(not(target_os = "android"))]
    {
        true
    }
}

/// Boot gate. Nothing may be written to cpufreq or any other sysfs node before
/// the framework has finished booting: forcing a low ceiling while system
/// services are still starting was the suspected cause of a boot hang.
///
/// Returns `false` when boot never completes; the caller exits without
/// touching a single node, which keeps the vendor defaults in place.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait_for_boot() -> bool {
    const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
    const SETTLE: Duration = Duration::from_secs(15);

    let deadline = Instant::now() + BOOT_TIMEOUT;
    while !boot_completed() {
        if Instant::now() >= deadline {
            tracing::warn!(
                timeout_secs = BOOT_TIMEOUT.as_secs(),
                "boot not completed; leaving cpufreq untouched"
            );
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    tracing::info!(
        settle_secs = SETTLE.as_secs(),
        "boot completed; settling before the first sysfs write"
    );
    std::thread::sleep(SETTLE);
    true
}

fn main() -> ExitCode {
    let args = Args::parse();

    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Aegis: configuration rejected: {error}");
            return ExitCode::from(2);
        }
    };

    let profile_path = args
        .profile
        .clone()
        .unwrap_or_else(|| PathBuf::from(&config.daemon.profile_path));
    let profile = match DeviceProfile::load(&profile_path) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("Aegis: device profile rejected: {error}");
            return ExitCode::from(2);
        }
    };

    let builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(config.daemon.log_level.as_str()))
        .with_ansi(false);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_file)
    {
        Ok(file) => {
            let _ = builder
                .with_writer(move || file.try_clone().expect("log file"))
                .try_init();
        }
        Err(_) => {
            let _ = builder.try_init();
        }
    }

    if args.check {
        let summary = ipc::status_json(&config, &profile, 0, &ipc::RuntimeStatus::default());
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
        return ExitCode::SUCCESS;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if !wait_for_boot() {
            return ExitCode::SUCCESS;
        }
        let started = Instant::now();
        match event_loop::run(
            &args.config,
            config,
            profile,
            &args.run_dir,
            started,
            &args.log_file,
            !args.observe,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Aegis: event loop failed: {error}");
                ExitCode::from(1)
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = profile;
        eprintln!(
            "Aegis: this daemon runs only on Linux/Android; use --check for offline validation"
        );
        ExitCode::from(3)
    }
}
