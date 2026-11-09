// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! On-demand power snapshot.
//!
//! Everything here is read only when a `power.get` request arrives; the daemon
//! itself never polls these nodes. That keeps the zero-periodic-wakeup rule
//! intact while still exposing live frequency, charging and thermal state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::json;
use tempo_core::DeviceProfile;

use crate::writer::{policy_dir, read_u32};

const BATTERY: &str = "/sys/class/power_supply/battery";
const THERMAL: &str = "/sys/class/thermal";

pub struct PowerSnapshot {
    pub actual_freq_khz: Vec<u32>,
    pub battery_percent: Option<u32>,
    pub battery_temp_c: Option<f32>,
    pub current_now_ua: Option<i64>,
    pub status: Option<String>,
    pub thermal_c: Option<f32>,
}

impl PowerSnapshot {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "ok": true,
            "actual_freq_khz": self.actual_freq_khz,
            "charging": self
                .status
                .as_deref()
                .map(|status| status.eq_ignore_ascii_case("charging") || status.eq_ignore_ascii_case("full"))
                .unwrap_or(false),
            "battery": {
                "percent": self.battery_percent,
                "temp_c": self.battery_temp_c,
                "current_ua": self.current_now_ua,
                "status": self.status,
            },
            "thermal_c": self.thermal_c,
        })
    }
}

pub fn snapshot(profile: &DeviceProfile) -> PowerSnapshot {
    let actual_freq_khz = profile
        .clusters
        .iter()
        .filter_map(|cluster| cluster.cpus.first().copied())
        .map(|cpu| {
            policy_dir(cpu)
                .and_then(|dir| read_u32(&dir.join("scaling_cur_freq")))
                .or_else(|| policy_dir(cpu).and_then(|dir| read_u32(&dir.join("cpuinfo_cur_freq"))))
                .unwrap_or(0)
        })
        .collect();

    PowerSnapshot {
        actual_freq_khz,
        battery_percent: read_u32(Path::new(BATTERY).join("capacity").as_path()),
        battery_temp_c: read_scaled(Path::new(BATTERY).join("temp").as_path(), 10.0),
        current_now_ua: read_i64(Path::new(BATTERY).join("current_now").as_path()),
        status: read_text(Path::new(BATTERY).join("status").as_path()),
        thermal_c: hottest_thermal(),
    }
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn read_i64(path: &Path) -> Option<i64> {
    read_text(path)?.parse().ok()
}

fn read_scaled(path: &Path, divisor: f32) -> Option<f32> {
    read_text(path)?
        .parse::<f32>()
        .ok()
        .map(|value| value / divisor)
}

/// Hottest readable thermal zone. Called only from events (demand samples and
/// control queries), never from a periodic loop.
pub fn thermal_c() -> Option<f32> {
    hottest_thermal()
}

/// Thermal zones that matter for the CPU guard. The sensor list is cached on
/// first use: reading a zone backed by `qti_qmi_sensor_v2` performs a QMI
/// transaction and can block for milliseconds, so walking all ~100 zones on
/// every guard tick is both slow and pointless. Only a handful of CPU/SoC/GPU
/// zones (and the battery zone) are consulted.
fn thermal_candidates() -> &'static [PathBuf] {
    static CANDIDATES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CANDIDATES.get_or_init(|| {
        let Ok(entries) = fs::read_dir(THERMAL) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(kind) = fs::read_to_string(dir.join("type")) else {
                continue;
            };
            let kind = kind.trim().to_ascii_lowercase();
            if kind.contains("vbat") {
                continue;
            }
            if kind.contains("cpu")
                || kind.contains("cpuss")
                || kind.contains("soc")
                || kind.contains("gpu")
                || kind.contains("battery")
            {
                candidates.push(dir.join("temp"));
            }
        }
        // A hard bound keeps each guard tick to a few QMI transactions.
        candidates.truncate(4);
        candidates
    })
}

fn hottest_thermal() -> Option<f32> {
    let mut hottest: Option<f32> = None;
    for path in thermal_candidates() {
        let Some(value) = read_u32(path) else {
            continue;
        };
        // Several zones report in millidegrees; normalise to degrees.
        let degrees = if value > 1000 {
            value as f32 / 1000.0
        } else {
            value as f32
        };
        if degrees < 100.0 {
            hottest = Some(hottest.map_or(degrees, |current| current.max(degrees)));
        }
    }
    hottest
}
