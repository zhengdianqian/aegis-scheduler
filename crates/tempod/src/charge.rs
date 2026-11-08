// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Charging control.
//!
//! The charge state machine lives in `tempo-core`; this file only feeds it from
//! the kernel and applies the resulting action to the first writable switch
//! found on the device. Power-supply changes arrive through the netlink
//! `KOBJECT_UEVENT` socket, so the daemon never polls battery nodes and never
//! holds a wakeup source.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use mio::event::Source;
use mio::{Interest, Registry, Token};
use serde::Deserialize;
use tempo_core::charge::{ChargeAction, ChargeDecision, ChargeGovernor, ChargePlan, ChargeSample};
use tempo_core::Config;

const BATTERY: &str = "/sys/class/power_supply/battery";

/// Battery node directory. `AEGIS_BATTERY_DIR` exists only so the exact same
/// binary can be driven against a synthetic node set during verification; the
/// normal path always reads the real power-supply nodes.
fn battery_dir() -> PathBuf {
    std::env::var_os("AEGIS_BATTERY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(BATTERY))
}

struct Candidate {
    path: &'static str,
    stop: &'static str,
    start: &'static str,
}

/// Ordered discovery list. Raw `input_suspend` nodes use 1 to cut power, the
/// `*_charging_enabled` family uses 0. All of them are restored verbatim on
/// drop, so leaving the module cannot strand the charger in a stopped state.
const CANDIDATES: &[Candidate] = &[
    Candidate {
        path: "/sys/class/qcom-battery/input_suspend",
        stop: "1",
        start: "0",
    },
    Candidate {
        path: "/sys/class/power_supply/battery/input_suspend",
        stop: "1",
        start: "0",
    },
    Candidate {
        path: "/sys/class/power_supply/battery/battery_input_suspend",
        stop: "1",
        start: "0",
    },
    Candidate {
        path: "/sys/class/power_supply/battery/charging_enabled",
        stop: "0",
        start: "1",
    },
    Candidate {
        path: "/sys/class/power_supply/battery/battery_charging_enabled",
        stop: "0",
        start: "1",
    },
    Candidate {
        path: "/sys/class/power_supply/battery/charge_enabled",
        stop: "0",
        start: "1",
    },
];

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct PlanFile {
    enabled: bool,
    mode: String,
    weekdays: Vec<u8>,
    windows: Vec<WindowFile>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct WindowFile {
    start: String,
    end: String,
}

pub struct ChargeWatcher {
    fd: libc::c_int,
    node: PathBuf,
    stop: &'static str,
    start: &'static str,
    original: Option<String>,
    governor: ChargeGovernor,
    stopped: bool,
    events: u64,
    plan_path: PathBuf,
    plan_stamp: Option<u64>,
    plan: ChargePlan,
    plan_allowed: bool,
    last: Option<ChargeDecision>,
    last_current_ma: Option<i32>,
}

impl ChargeWatcher {
    pub fn open(plan_path: PathBuf) -> io::Result<Self> {
        // Verification hook: point the same binary at a scratch switch so a
        // synthetic full-charge run never touches the real charger.
        let (node, stop, start) = match std::env::var_os("AEGIS_CHARGE_SWITCH") {
            Some(path) => (PathBuf::from(path), "1", "0"),
            None => {
                let candidate = CANDIDATES
                    .iter()
                    .find(|candidate| writable(Path::new(candidate.path)))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no writable charge switch")
                    })?;
                (
                    PathBuf::from(candidate.path),
                    candidate.stop,
                    candidate.start,
                )
            }
        };

        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        address.nl_groups = 1;
        let bound = unsafe {
            libc::bind(
                fd,
                &mut address as *mut libc::sockaddr_nl as *mut libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::c_uint,
            )
        };
        if bound < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }

        let original = read_node(&node);
        Ok(Self {
            fd,
            node,
            stop,
            start,
            original,
            governor: ChargeGovernor::new(),
            stopped: false,
            events: 0,
            plan_path,
            plan_stamp: None,
            plan: ChargePlan::default(),
            plan_allowed: true,
            last: None,
            last_current_ma: None,
        })
    }

    pub fn node(&self) -> String {
        self.node.display().to_string()
    }

    pub fn events(&self) -> u64 {
        self.events
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// 100% 涓流阶段：此时守护按 3 秒一拍读取 current/status 收集满充证据。
    pub fn is_trickling(&self) -> bool {
        self.governor.is_trickling()
    }

    pub fn is_latched(&self) -> bool {
        self.governor.is_latched()
    }

    pub fn phase(&self) -> &'static str {
        self.governor.phase()
    }

    pub fn last_current_ma(&self) -> Option<i32> {
        self.last_current_ma
    }

    pub fn plan_allowed(&self) -> bool {
        self.plan_allowed
    }

    pub fn plan_enabled(&self) -> bool {
        self.plan.enabled
    }

    pub fn reasons(&self) -> String {
        self.last
            .as_ref()
            .map(ChargeDecision::reasons_joined)
            .unwrap_or_default()
    }

    /// Consumes pending uevents and returns how many concerned the battery.
    pub fn drain(&mut self) -> usize {
        let mut battery = 0usize;
        let mut buffer = [0u8; 2048];
        loop {
            let count = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if count <= 0 {
                break;
            }
            let payload = String::from_utf8_lossy(&buffer[..count as usize]);
            if payload.contains("POWER_SUPPLY_NAME=battery") {
                battery += 1;
            }
        }
        self.events += battery as u64;
        battery
    }

    /// Reads one sample, advances the governor and applies the action.
    pub fn update(&mut self, config: &Config, ts_us: u64) -> ChargeDecision {
        self.refresh_plan();
        let (day, minute, _) = local_now();
        self.plan_allowed = self.plan.allows(day, minute);

        let battery = battery_dir();
        let percent = read_u32(&battery.join("capacity")).map(|value| value as u8);
        let mut sample = ChargeSample {
            ts_us,
            percent,
            temp_c: read_u32(&battery.join("temp")).map(|value| value as f32 / 10.0),
            plugged: external_power_present(),
            ..ChargeSample::default()
        };
        // 只有 100% 涓流阶段才需要电流/电压/status 证据，避免把节点读取
        // 变成常驻开销。
        if config.charge.stop_at_percent == 100
            && percent.is_some_and(|value| value >= config.charge.stop_at_percent)
        {
            sample.current_ma = read_i32(&battery.join("current_now")).map(|value| value / 1000);
            sample.voltage_mv = read_u32(&battery.join("voltage_now")).map(|value| value / 1000);
            sample.status_full = read_text(&battery.join("status"))
                .is_some_and(|status| status.eq_ignore_ascii_case("full"));
        }
        self.last_current_ma = sample.current_ma.map(|value| value.unsigned_abs() as i32);
        let was_stopped = self.stopped;
        let decision = self
            .governor
            .update(&sample, &config.charge, Some(self.plan_allowed));

        if !sample.plugged {
            // Leaving the charger clears every hold. Restore the switch so the
            // next plug-in is governed by the fresh session, not by a stale stop.
            if was_stopped {
                self.write(self.start);
            }
        } else {
            match decision.action {
                ChargeAction::Stop => self.write(self.stop),
                ChargeAction::Resume => self.write(self.start),
                ChargeAction::None | ChargeAction::Hold => {}
            }
        }
        self.stopped = self.governor.is_stopped();
        self.last = Some(decision.clone());
        decision
    }

    /// Seconds until the reservation window flips state. Used to arm the
    /// daemon's single one-shot boundary timer.
    pub fn next_boundary_secs(&self) -> Option<u32> {
        if !self.plan.enabled {
            return None;
        }
        let (day, minute, second) = local_now();
        self.plan
            .next_change_after(day, minute)
            .map(|minutes| minutes * 60 - second)
    }

    fn refresh_plan(&mut self) {
        let stamp = fs::metadata(&self.plan_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        if stamp == self.plan_stamp {
            return;
        }
        self.plan_stamp = stamp;
        let text = match fs::read_to_string(&self.plan_path) {
            Ok(text) => text,
            Err(_) => {
                self.plan = ChargePlan::default();
                return;
            }
        };
        let parsed: PlanFile = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(_) => return,
        };
        let windows: Vec<(String, String)> = parsed
            .windows
            .into_iter()
            .map(|window| (window.start, window.end))
            .collect();
        self.plan = ChargePlan::normalize(
            parsed.enabled,
            &parsed.mode,
            &parsed.weekdays,
            &windows,
        );
    }

    fn write(&self, value: &str) {
        let _ = fs::write(&self.node, value);
    }
}

impl Drop for ChargeWatcher {
    fn drop(&mut self) {
        if self.stopped {
            let value = self
                .original
                .clone()
                .unwrap_or_else(|| self.start.to_owned());
            let _ = fs::write(&self.node, value);
        }
        unsafe { libc::close(self.fd) };
    }
}

impl Source for ChargeWatcher {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&self.fd))
    }
}

/// USB/AC presence, not `battery/status`. Once charging is suspended the
/// battery reports `Discharging` even though a cable is attached, so using the
/// status field would mistake our own stop for an unplug and oscillate.
fn external_power_present() -> bool {
    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "battery") {
                continue;
            }
            let kind = read_text(&path.join("type")).unwrap_or_default();
            if kind.eq_ignore_ascii_case("battery") {
                continue;
            }
            if read_u32(&path.join("online")) == Some(1) {
                return true;
            }
            if read_u32(&path.join("present")) == Some(1) {
                return true;
            }
        }
    }
    // Fallback for devices without an online node.
    !read_text(&Path::new(BATTERY).join("status"))
        .map(|status| status.eq_ignore_ascii_case("discharging"))
        .unwrap_or(false)
}

fn writable(path: &Path) -> bool {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok()
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn read_u32(path: &Path) -> Option<u32> {
    read_text(path)?.parse().ok()
}

fn read_i32(path: &Path) -> Option<i32> {
    read_text(path)?.parse().ok()
}

fn read_node(path: &Path) -> Option<String> {
    read_text(path)
}

/// Local wall clock as (day 1=Monday..7=Sunday, minutes after midnight,
/// seconds after minute). The reservation plan is defined in local time.
fn local_now() -> (u8, u16, u32) {
    let mut seconds: libc::time_t = 0;
    unsafe { libc::time(&mut seconds) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    let day = if tm.tm_wday == 0 { 7 } else { tm.tm_wday as u8 };
    (
        day,
        (tm.tm_hour * 60 + tm.tm_min) as u16,
        tm.tm_sec.max(0) as u32,
    )
}
