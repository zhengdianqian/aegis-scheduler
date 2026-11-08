// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde_json::json;
use tempo_core::controller::Decision;
use tempo_core::{Config, DeviceProfile};

use crate::power;
use crate::writer::CpufreqWriter;

/// Runtime view shared between the event loop and control queries.
#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    pub probe: String,
    pub probe_error: Option<String>,
    pub cpu_count: usize,
    pub events: u64,
    pub active_events: u64,
    pub foreground_pids: usize,
    pub foreground_error: Option<String>,
    pub foreground_package: Option<String>,
    pub screen_on: bool,
    pub screen_source: String,
    pub busy: Vec<f32>,
    pub foreground: Vec<f32>,
    pub fg_switches: Vec<u64>,
    pub switches: Vec<u64>,
    pub quiet: bool,
    pub decisions: Vec<Decision>,
    pub touch_sources: usize,
    pub touch_events: u64,
    pub launch_lifts: u64,
    pub mask_tracked: bool,
    pub mask_open: bool,
    pub mask_windows: u64,
    pub mask_unmask_us: u64,
    pub mask_remask_us: u64,
    pub fas_available: bool,
    pub fas_active: bool,
    pub charge_node: Option<String>,
    pub charge_stopped: bool,
    pub charge_reasons: String,
    pub charge_plan_allowed: bool,
    pub charge_plan_enabled: bool,
    pub charge_phase: String,
    pub charge_trickle: bool,
    pub charge_latched: bool,
    pub charge_current_ma: Option<i32>,
    pub thermal_available: bool,
    pub write_enabled: bool,
    pub writes: u64,
    pub cap_writes: u64,
    pub floor_writes: u64,
    pub rejected_floor: u64,
    pub write_error: Option<String>,
    pub config_generation: u64,
    pub restart_required: bool,
}

impl RuntimeStatus {
    fn probe_label(&self) -> &str {
        if self.probe.is_empty() {
            "starting"
        } else {
            self.probe.as_str()
        }
    }
}

/// What the event loop must do after a control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// `config.apply` succeeded; the caller re-creates the controller.
    Reloaded,
    /// The cpufreq policies were restored to their original values.
    Restored,
}

pub struct ControlContext<'a> {
    pub config: &'a mut Config,
    pub profile: &'a DeviceProfile,
    pub runtime: &'a RuntimeStatus,
    pub config_path: &'a Path,
    pub log_path: &'a Path,
    pub started: Instant,
    pub writer: Option<&'a mut CpufreqWriter>,
}

pub fn status_json(
    config: &Config,
    profile: &DeviceProfile,
    uptime_ms: u128,
    runtime: &RuntimeStatus,
) -> serde_json::Value {
    let clusters: Vec<_> = profile
        .clusters
        .iter()
        .enumerate()
        .map(|(index, cluster)| {
            json!({
                "cpus": cluster.cpus,
                "role": cluster.role,
                "min_khz": cluster.min_khz,
                "max_khz": cluster.max_khz,
                "capacity": cluster.capacity,
                "busy": runtime.busy.get(index).copied().unwrap_or(0.0),
                "foreground": runtime.foreground.get(index).copied().unwrap_or(0.0),
            })
        })
        .collect();
    let snapshot = power::snapshot(profile);
    let writing = runtime.write_enabled;

    json!({
        "ok": true,
        "service": "Aegis",
        "version": env!("CARGO_PKG_VERSION"),
        "stage": if writing { "active" } else { "observe" },
        "mode": if writing { "adaptive" } else { "observe-only" },
        "scheduler_writes": writing,
        "mask": {
            "tracked": runtime.mask_tracked,
            "released": runtime.mask_open,
            "windows": runtime.mask_windows,
            "unmask_us": runtime.mask_unmask_us,
            "remask_us": runtime.mask_remask_us,
        },
        "schema": config.schema,
        "platform": profile.platform,
        "sdk": profile.sdk,
        "cluster_count": profile.clusters.len(),
        "clusters": clusters,
        "actual_freq_khz": snapshot.actual_freq_khz,
        "charging": snapshot
            .status
            .as_deref()
            .map(|status| status.eq_ignore_ascii_case("charging") || status.eq_ignore_ascii_case("full"))
            .unwrap_or(false),
        "battery": {
            "percent": snapshot.battery_percent,
            "temp_c": snapshot.battery_temp_c,
            "current_ua": snapshot.current_now_ua,
            "status": snapshot.status,
        },
        "thermal_c": snapshot.thermal_c,
        "charge": {
            "node": runtime.charge_node,
            "stopped": runtime.charge_stopped,
            "reasons": runtime.charge_reasons,
            "plan_allowed": runtime.charge_plan_allowed,
            "plan_enabled": runtime.charge_plan_enabled,
            "phase": runtime.charge_phase,
            "trickle": runtime.charge_trickle,
            "latched": runtime.charge_latched,
            "current_ma": runtime.charge_current_ma,
        },
        "capabilities": {
            "scheduler_write": writing,
            "probe": runtime.probe == "ready",
            "touch": runtime.touch_sources > 0,
            "fas": runtime.fas_available,
            "charge_control": runtime.charge_node.is_some(),
            "thermal_guard": runtime.thermal_available,
            "doze_safe": true,
        },
        "config_generation": runtime.config_generation,
        "restart_required": runtime.restart_required,
        "uptime_ms": uptime_ms,
        "foreground_package": runtime.foreground_package,
        "probe": {
            "state": runtime.probe_label(),
            "cpu_count": runtime.cpu_count,
            "events": runtime.events,
            "active_events": runtime.active_events,
            "foreground_pids": runtime.foreground_pids,
            "foreground_error": runtime.foreground_error,
            "foreground_switches": runtime.fg_switches.iter().sum::<u64>(),
            "switches": runtime.switches.iter().sum::<u64>(),
            "error": runtime.probe_error,
        },
        "screen": {
            "on": runtime.screen_on,
            "source": runtime.screen_source,
        },
        "fas": {
            "available": runtime.fas_available,
            "active": runtime.fas_active,
            "enabled": config.fas.enabled,
            "apps": config.fas.apps,
        },
        "touch": {
            "sources": runtime.touch_sources,
            "events": runtime.touch_events,
        },
        "launch": {
            "lifts": runtime.launch_lifts,
        },
        "writer": {
            "enabled": writing,
            "writes": runtime.writes,
            "cap_writes": runtime.cap_writes,
            "floor_writes": runtime.floor_writes,
            "rejected_floor": runtime.rejected_floor,
            "error": runtime.write_error,
        },
        "control": {
            "quiet": runtime.quiet,
            "decisions": runtime.decisions,
        },
    })
}

#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub fn handle(ctx: &mut ControlContext<'_>, line: &str) -> (String, Outcome) {
    let request: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return (response(error("bad_json")), Outcome::None),
    };
    let command = request
        .get("cmd")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    let mut outcome = Outcome::None;

    let response = match command {
        "ping" => json!({
            "ok": true,
            "service": "Aegis",
            "version": env!("CARGO_PKG_VERSION"),
        }),
        "status" => status_json(
            ctx.config,
            ctx.profile,
            ctx.started.elapsed().as_millis(),
            ctx.runtime,
        ),
        "profile.get" => json!({ "ok": true, "profile": ctx.profile }),
        "config.get" => match ctx.config.to_toml_string() {
            Ok(toml) => json!({ "ok": true, "toml": toml }),
            Err(error) => {
                json!({ "ok": false, "error": "encode_failed", "detail": error.to_string() })
            }
        },
        "config.validate" => match ctx.config.validate() {
            Ok(()) => json!({ "ok": true }),
            Err(error) => {
                json!({ "ok": false, "error": "config_rejected", "detail": error.to_string() })
            }
        },
        "config.apply" => match apply_config(ctx, &request) {
            Ok(restart_required) => {
                outcome = Outcome::Reloaded;
                json!({
                    "ok": true,
                    "applied": true,
                    "restart_required": restart_required,
                    "config_generation": ctx.runtime.config_generation + 1,
                })
            }
            Err(error) => {
                json!({ "ok": false, "error": "apply_failed", "detail": error })
            }
        },
        "log.tail" => {
            let lines = request
                .get("lines")
                .and_then(|value| value.as_u64())
                .unwrap_or(40)
                .clamp(1, 200) as usize;
            let text = fs::read_to_string(ctx.log_path).unwrap_or_default();
            let tail: Vec<&str> = {
                let all: Vec<&str> = text.lines().collect();
                let start = all.len().saturating_sub(lines);
                all[start..].to_vec()
            };
            json!({ "ok": true, "path": ctx.log_path.display().to_string(), "lines": tail })
        }
        "power.get" => {
            let mut value = power::snapshot(ctx.profile).to_json();
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "config_generation".to_owned(),
                    json!(ctx.runtime.config_generation),
                );
            }
            value
        }
        "scheduler.restore" => match ctx.writer.as_deref_mut() {
            Some(writer) => match writer.restore() {
                Ok(()) => {
                    outcome = Outcome::Restored;
                    json!({ "ok": true, "restored": true })
                }
                Err(error) => json!({ "ok": false, "error": "restore_failed", "detail": error }),
            },
            None => json!({ "ok": false, "error": "writer_unavailable" }),
        },
        _ => error("unknown_command"),
    };

    let mut text = response.to_string();
    text.push('\n');
    (text, outcome)
}

fn apply_config(ctx: &mut ControlContext<'_>, request: &serde_json::Value) -> Result<bool, String> {
    let parsed = if let Some(text) = request.get("toml").and_then(|value| value.as_str()) {
        toml::from_str::<Config>(text).map_err(|error| format!("parse: {error}"))?
    } else if let Some(value) = request.get("config") {
        serde_json::from_value::<Config>(value.clone())
            .map_err(|error| format!("parse: {error}"))?
    } else {
        return Err("missing toml or config".to_owned());
    };
    parsed.validate().map_err(|error| error.to_string())?;

    let encoded = parsed
        .to_toml_string()
        .map_err(|error| format!("encode: {error}"))?;
    let previous = fs::read_to_string(ctx.config_path).unwrap_or_default();
    fs::write(ctx.config_path, &encoded).map_err(|error| format!("write config: {error}"))?;
    if let Err(error) = Config::load(ctx.config_path) {
        let _ = fs::write(ctx.config_path, previous);
        return Err(format!("reload rejected: {error}"));
    }

    let restart_required = parsed.daemon.profile_path != ctx.config.daemon.profile_path
        || parsed.daemon.log_level != ctx.config.daemon.log_level;
    *ctx.config = parsed;
    Ok(restart_required)
}

#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn error(kind: &str) -> serde_json::Value {
    json!({ "ok": false, "error": kind })
}

fn response(value: serde_json::Value) -> String {
    let mut text = value.to_string();
    text.push('\n');
    text
}
