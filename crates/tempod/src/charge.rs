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

use mio::event::Source;
use mio::{Interest, Registry, Token};
use serde::Deserialize;
use tempo_core::charge::{ChargeDecision, ChargeGovernor, ChargePlan, ChargeSample};
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
    confirmed_stopped: Option<bool>,
    target_stopped: bool,
    last_write_error: Option<String>,
    battery: PathBuf,
    supplies: PathBuf,
    events: u64,
    plan_path: PathBuf,
    plan_text: Option<String>,
    plan: ChargePlan,
    plan_allowed: bool,
    last: Option<ChargeDecision>,
    last_current_ma: Option<i32>,
}

impl ChargeWatcher {
    pub fn open(plan_path: PathBuf) -> io::Result<Self> {
        let battery = battery_dir();
        let sandbox_switch = std::env::var_os("AEGIS_CHARGE_SWITCH").map(PathBuf::from);
        let sandbox_battery = std::env::var_os("AEGIS_BATTERY_DIR").map(PathBuf::from);
        let sandbox = sandbox_paths(sandbox_battery.as_deref(), sandbox_switch.as_deref())?;
        let supplies = sandbox
            .as_ref()
            .map(|(_, root)| root.clone())
            .unwrap_or_else(|| PathBuf::from("/sys/class/power_supply"));
        let (node, stop, start) = match sandbox {
            Some((path, _)) => {
                let enabled = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.ends_with("charging_enabled") || name == "charge_enabled"
                    });
                (
                    path,
                    if enabled { "0" } else { "1" },
                    if enabled { "1" } else { "0" },
                )
            }
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
        let confirmed_stopped = original.as_deref().and_then(|value| {
            if value == stop {
                Some(true)
            } else if value == start {
                Some(false)
            } else {
                None
            }
        });
        Ok(Self {
            fd,
            node,
            stop,
            start,
            original,
            governor: ChargeGovernor::new(),
            stopped: confirmed_stopped.unwrap_or(false),
            confirmed_stopped,
            target_stopped: false,
            last_write_error: None,
            battery,
            supplies,
            events: 0,
            plan_path,
            plan_text: None,
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

    pub fn needs_retry(&self) -> bool {
        self.confirmed_stopped != Some(self.target_stopped)
    }

    pub fn last_write_error(&self) -> Option<&str> {
        self.last_write_error.as_deref()
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
        self.confirmed_stopped = read_node(&self.node).and_then(|value| {
            if value == self.stop {
                Some(true)
            } else if value == self.start {
                Some(false)
            } else {
                None
            }
        });
        self.stopped = self.confirmed_stopped == Some(true);
        self.refresh_plan();
        let (day, minute, _) = local_now();
        self.plan_allowed = self.plan.allows(day, minute);

        let battery = &self.battery;
        let percent = read_u32(&battery.join("capacity"))
            .filter(|value| *value <= 100)
            .map(|value| value as u8);
        let mut sample = ChargeSample {
            ts_us,
            percent,
            temp_c: read_i32(&battery.join("temp")).map(|value| value as f32 / 10.0),
            plugged: external_power_present(&self.supplies),
            charging_suspended: self.confirmed_stopped != Some(false),
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
        let decision = self
            .governor
            .update(&sample, &config.charge, Some(self.plan_allowed));

        self.target_stopped = !decision.allow_charge;
        self.apply_target();
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
        let text = fs::read_to_string(&self.plan_path).ok();
        if text == self.plan_text {
            return;
        }
        self.plan_text = text.clone();
        let Some(text) = text else {
            self.plan = ChargePlan::default();
            return;
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
        self.plan = ChargePlan::normalize(parsed.enabled, &parsed.mode, &parsed.weekdays, &windows);
    }

    fn apply_target(&mut self) {
        if !self.needs_retry() {
            return;
        }
        let value = if self.target_stopped {
            self.stop
        } else {
            self.start
        };
        match write_confirmed(&self.node, value) {
            Ok(()) => {
                self.confirmed_stopped = Some(self.target_stopped);
                self.stopped = self.target_stopped;
                self.last_write_error = None;
            }
            Err(error) => {
                self.confirmed_stopped = None;
                tracing::warn!(node = %self.node.display(), target = value, %error, "charge switch write unconfirmed; retry required");
                self.last_write_error = Some(error.to_string());
            }
        }
    }
}

impl Drop for ChargeWatcher {
    fn drop(&mut self) {
        if self.stopped || self.needs_retry() {
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
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
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
fn external_power_present(supplies: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(supplies) {
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
        }
    }
    false
}

fn sandbox_paths(
    battery: Option<&Path>,
    switch: Option<&Path>,
) -> io::Result<Option<(PathBuf, PathBuf)>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox requires battery and switch under the same non-sysfs root",
        )
    };
    match (battery, switch) {
        (None, None) => Ok(None),
        (Some(battery), Some(switch)) => {
            let battery = battery.canonicalize()?;
            let root = battery.parent().ok_or_else(invalid)?.to_path_buf();
            let switch = switch.canonicalize()?;
            if battery.file_name().is_none_or(|name| name != "battery")
                || root == Path::new("/")
                || root.starts_with("/sys")
                || !switch.starts_with(&root)
            {
                return Err(invalid());
            }
            validate_sandbox_tree(&root, &root)?;
            Ok(Some((switch, root)))
        }
        _ => Err(invalid()),
    }
}

fn validate_sandbox_tree(path: &Path, root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox symlinks are not allowed",
            ));
        }
        if entry.file_type()?.is_dir() && entry.path().starts_with(root) {
            validate_sandbox_tree(&entry.path(), root)?;
        }
    }
    Ok(())
}

fn write_confirmed(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)?;
    confirm_readback(path, value)
}

fn confirm_readback(path: &Path, value: &str) -> io::Result<()> {
    if fs::read_to_string(path)?.trim() != value {
        return Err(io::Error::other("charge switch rejected requested value"));
    }
    Ok(())
}

fn writable(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "aegis-charge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("battery")).unwrap();
            fs::create_dir(root.join("usb")).unwrap();
            fs::write(root.join("usb/type"), "USB").unwrap();
            fs::write(root.join("usb/online"), "1").unwrap();
            fs::write(root.join("battery/capacity"), "100").unwrap();
            fs::write(root.join("battery/temp"), "330").unwrap();
            Self(root)
        }
        fn watcher(&self, enabled: bool) -> ChargeWatcher {
            let (stop, start) = if enabled { ("0", "1") } else { ("1", "0") };
            let node = self.0.join("battery").join(if enabled {
                "charging_enabled"
            } else {
                "input_suspend"
            });
            fs::write(&node, start).unwrap();
            ChargeWatcher {
                fd: -1,
                node,
                stop,
                start,
                original: Some(start.into()),
                governor: ChargeGovernor::new(),
                stopped: false,
                confirmed_stopped: Some(false),
                target_stopped: false,
                last_write_error: None,
                battery: self.0.join("battery"),
                supplies: self.0.clone(),
                events: 0,
                plan_path: self.0.join("plan.json"),
                plan_text: None,
                plan: ChargePlan::default(),
                plan_allowed: true,
                last: None,
                last_current_ma: None,
            }
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sandbox_requires_both_paths_and_rejects_escape() {
        let sandbox = Sandbox::new();
        let watcher = sandbox.watcher(false);
        assert!(sandbox_paths(Some(&watcher.battery), None).is_err());
        assert!(sandbox_paths(None, Some(&watcher.node)).is_err());
        assert!(sandbox_paths(Some(&watcher.battery), Some(&watcher.node))
            .unwrap()
            .is_some());
        assert!(sandbox_paths(Some(&watcher.battery), Some(Path::new("/dev/null"))).is_err());
        std::os::unix::fs::symlink("/sys", sandbox.0.join("escape")).unwrap();
        assert!(sandbox_paths(Some(&watcher.battery), Some(&watcher.node)).is_err());
    }

    #[test]
    fn online_not_present_and_missing_sensors_are_not_full() {
        let sandbox = Sandbox::new();
        let mut watcher = sandbox.watcher(false);
        let mut config = Config::default();
        config.charge.stop_at_percent = 100;
        assert!(watcher.update(&config, 0).allow_charge);
        assert!(watcher.update(&config, 20_000_000).allow_charge);
        assert!(!watcher.is_latched());
        fs::write(sandbox.0.join("usb/present"), "1").unwrap();
        fs::write(sandbox.0.join("usb/online"), "0").unwrap();
        assert!(!external_power_present(&sandbox.0));
        fs::remove_file(sandbox.0.join("usb/online")).unwrap();
        assert!(!external_power_present(&sandbox.0));
    }

    #[test]
    fn failed_stop_and_resume_retry_for_both_polarities() {
        for enabled in [false, true] {
            let sandbox = Sandbox::new();
            let mut watcher = sandbox.watcher(enabled);
            let mut config = Config::default();
            config.charge.stop_at_percent = 80;
            fs::remove_file(&watcher.node).unwrap();
            fs::create_dir(&watcher.node).unwrap();
            watcher.update(&config, 0);
            assert!(watcher.needs_retry());
            assert!(!watcher.is_stopped());
            assert!(watcher.last_write_error().is_some());
            fs::remove_dir(&watcher.node).unwrap();
            fs::write(&watcher.node, watcher.start).unwrap();
            watcher.update(&config, 1);
            assert!(!watcher.needs_retry());
            assert!(watcher.is_stopped());
            assert_eq!(read_node(&watcher.node).as_deref(), Some(watcher.stop));
            fs::remove_file(&watcher.node).unwrap();
            fs::create_dir(&watcher.node).unwrap();
            fs::write(sandbox.0.join("usb/online"), "0").unwrap();
            watcher.update(&config, 2);
            assert!(watcher.needs_retry());
            fs::remove_dir(&watcher.node).unwrap();
            fs::write(&watcher.node, watcher.stop).unwrap();
            watcher.update(&config, 3);
            assert!(!watcher.needs_retry());
            assert!(!watcher.is_stopped());
            assert_eq!(read_node(&watcher.node).as_deref(), Some(watcher.start));
        }
    }

    #[test]
    fn external_switch_overrides_are_reconciled_for_both_polarities() {
        for enabled in [false, true] {
            let sandbox = Sandbox::new();
            let mut watcher = sandbox.watcher(enabled);
            let mut config = Config::default();
            config.charge.stop_at_percent = 80;
            watcher.update(&config, 0);
            assert!(watcher.is_stopped());
            fs::write(&watcher.node, watcher.start).unwrap();
            watcher.update(&config, 1);
            assert!(watcher.is_stopped());
            assert!(!watcher.needs_retry());
            assert_eq!(read_node(&watcher.node).as_deref(), Some(watcher.stop));
            fs::write(watcher.battery.join("capacity"), "50").unwrap();
            watcher.update(&config, 2);
            assert!(!watcher.is_stopped());
            fs::write(&watcher.node, watcher.stop).unwrap();
            watcher.update(&config, 3);
            assert!(!watcher.is_stopped());
            assert!(!watcher.needs_retry());
            assert_eq!(read_node(&watcher.node).as_deref(), Some(watcher.start));
        }
    }

    #[test]
    fn unreadable_or_invalid_switch_resets_full_evidence() {
        for invalid_value in [false, true] {
            let sandbox = Sandbox::new();
            let mut watcher = sandbox.watcher(false);
            let mut config = Config::default();
            config.charge.stop_at_percent = 100;
            fs::write(watcher.battery.join("current_now"), "0").unwrap();
            watcher.update(&config, 0);
            if invalid_value {
                fs::write(&watcher.node, "invalid").unwrap();
            } else {
                fs::remove_file(&watcher.node).unwrap();
                fs::create_dir(&watcher.node).unwrap();
            }
            watcher.update(&config, 10_000_000);
            assert!(!watcher.is_latched());
            if invalid_value {
                // Unknown input was corrected by a confirmed write.
                assert!(!watcher.needs_retry());
            } else {
                assert!(watcher.needs_retry());
                assert_eq!(watcher.confirmed_stopped, None);
                assert!(!watcher.is_stopped());
                fs::remove_dir(&watcher.node).unwrap();
                fs::write(&watcher.node, watcher.start).unwrap();
            }
            watcher.update(&config, 11_000_000);
            assert!(!watcher.needs_retry());
            watcher.update(&config, 19_000_000);
            assert!(!watcher.is_latched());
            watcher.update(&config, 20_000_000);
            assert!(watcher.is_latched());
        }
    }

    #[test]
    fn readback_rejection_is_an_error_and_remains_retryable() {
        let sandbox = Sandbox::new();
        let mut watcher = sandbox.watcher(false);
        assert!(confirm_readback(&watcher.node, watcher.stop).is_err());
        // /dev/null accepts writes but cannot return the requested state.
        watcher.node = PathBuf::from("/dev/null");
        watcher.target_stopped = true;
        watcher.apply_target();
        assert!(watcher.needs_retry());
        assert!(watcher.last_write_error().unwrap().contains("rejected"));
        watcher.node = sandbox.0.join("battery/input_suspend");
        watcher.apply_target();
        assert!(!watcher.needs_retry());
    }

    #[test]
    fn sandbox_full_evidence_temperature_and_replug() {
        let sandbox = Sandbox::new();
        let mut watcher = sandbox.watcher(false);
        let mut config = Config::default();
        config.charge.stop_at_percent = 100;
        fs::write(watcher.battery.join("current_now"), "80000").unwrap();
        watcher.update(&config, 0);
        fs::write(watcher.battery.join("temp"), "460").unwrap();
        watcher.update(&config, 4_000_000);
        assert!(watcher.is_stopped());
        fs::write(watcher.battery.join("current_now"), "0").unwrap();
        watcher.update(&config, 20_000_000);
        assert!(!watcher.is_latched());
        fs::write(watcher.battery.join("temp"), "390").unwrap();
        watcher.update(&config, 21_000_000);
        assert!(!watcher.is_stopped());
        fs::write(watcher.battery.join("current_now"), "-80000").unwrap();
        watcher.update(&config, 24_000_000);
        watcher.update(&config, 32_000_000);
        assert!(!watcher.is_latched());
        watcher.update(&config, 33_000_000);
        assert!(watcher.is_latched());
        assert!(watcher.is_stopped());
        fs::write(sandbox.0.join("usb/online"), "0").unwrap();
        watcher.update(&config, 34_000_000);
        assert!(!watcher.is_latched());
        assert!(!watcher.is_stopped());
        fs::write(sandbox.0.join("usb/online"), "1").unwrap();
        watcher.update(&config, 35_000_000);
        assert!(!watcher.is_latched());
        fs::write(watcher.battery.join("status"), "Full").unwrap();
        watcher.update(&config, 36_000_000);
        assert!(watcher.is_latched());
    }

    #[test]
    fn plan_content_changes_visible_without_timestamp_delay() {
        let sandbox = Sandbox::new();
        let mut watcher = sandbox.watcher(false);
        fs::write(
            &watcher.plan_path,
            r#"{"enabled":true,"windows":[{"start":"23:00","end":"07:00"}]}"#,
        )
        .unwrap();
        watcher.refresh_plan();
        assert!(watcher.plan.enabled);
        fs::write(
            &watcher.plan_path,
            r#"{"enabled":false,"windows":[{"start":"23:00","end":"07:00"}]}"#,
        )
        .unwrap();
        watcher.refresh_plan();
        assert!(!watcher.plan.enabled);
    }
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
