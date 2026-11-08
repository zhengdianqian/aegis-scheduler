// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::profile::Role;
use crate::CoreError;

/// Schema version of `config.toml` and `device.toml`.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Warn
    }
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub daemon: DaemonConfig,
    pub events: EventsConfig,
    pub control: ControlConfig,
    pub charge: ChargeConfig,
    pub thermal: ThermalConfig,
    pub fas: FasConfig,
    pub ui: UiConfig,
}

/// FAS is opt-in per application. The default is fully off: an empty list means
/// FAS never runs, not "every application".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FasConfig {
    pub enabled: bool,
    pub apps: Vec<String>,
    pub frame_budget_ms: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub log_level: LogLevel,
    pub profile_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    pub runtime_quantum_us: u64,
    pub demand_step: f32,
    pub screen_path: String,
    pub frame_stride: u32,
    pub quiet_decay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlConfig {
    pub quiet_ratio: f32,
    pub quiet_cap: f32,
    pub floor_enter_ratio: f32,
    pub floor_exit_ratio: f32,
    pub floor_settle_ms: u64,
    pub floor_exit_ms: u64,
    pub fg_fast_ms: u64,
    pub floor_residence_ms: u64,
    pub drop_ratio: f32,
    pub drop_factor: f32,
    pub touch_lift_ms: u64,
    pub touch_lift_level: f32,
    pub launch_lift_ms: u64,
    pub launch_lift_level: f32,
    /// Event lifts (launch / foreground change) cap the prime cluster here so
    /// a window animation or IME overlay cannot slam it to the top tier.
    /// Sustained load and over-budget frames can still climb higher.
    pub prime_lift_level: f32,
    /// Even under sustained UI load the prime cluster is budgeted below its
    /// top tier; only genuinely heavy frame pressure earns the last steps.
    pub prime_load_cap: f32,
    /// Span above the current floor that the governor may still use. The
    /// writer caps `scaling_max_freq` at `floor + span * ceiling_headroom`,
    /// so a vendor boost or an input hint can no longer slam a cluster to its
    /// top tier while the measured demand stays low. Real frame pressure
    /// raises the floor first, which lifts the cap with it.
    pub ceiling_headroom: f32,
    /// Time to climb through all touch ramp stages. Three stages within this
    /// window keep the perceived response under ~50 ms.
    pub touch_ramp_ms: u64,
    pub touch_ramp_stages: u8,
    /// One-step decay interval after the finger lifts.
    pub touch_decay_ms: u64,
    pub frame_slack_ms: f32,
    pub frame_burst_ms: u64,
    pub frame_burst_level: f32,
    pub reserve_ratio: f32,
    pub step_hysteresis: f32,
    /// Headroom kept above the cluster floor while parked. The floor is a
    /// target, never a hard lock: the governor may always use this much span.
    pub parked_headroom: f32,
    pub role_cap: RoleCaps,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleCaps {
    pub efficiency: f32,
    pub balanced: f32,
    pub prime: f32,
}

impl RoleCaps {
    pub fn for_role(&self, role: Role) -> f32 {
        match role {
            Role::Efficiency => self.efficiency,
            Role::Balanced => self.balanced,
            Role::Prime => self.prime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChargeConfig {
    pub stop_at_percent: u8,
    pub resume_at_percent: u8,
    pub temp_stop_c: f32,
    pub temp_resume_c: f32,
    pub latch_per_plug: bool,
    /// 涓流满充电流阈值（mA，绝对值）。到 100% 不停充，电流连续低于该值
    /// 达到 `full_hold_seconds` 才判定充满并锁存停充。
    pub full_current_ma: u32,
    /// 满充信号需要持续的秒数。守护只在涓流阶段按 3 秒一拍采样。
    pub full_hold_seconds: u32,
    /// 可选满充电压阈值（mV）。默认关闭，仅配置后参与判定。
    pub full_voltage_mv: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThermalConfig {
    pub guard_c: f32,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub language: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            daemon: DaemonConfig::default(),
            events: EventsConfig::default(),
            control: ControlConfig::default(),
            charge: ChargeConfig::default(),
            thermal: ThermalConfig::default(),
            fas: FasConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Warn,
            profile_path: "/data/adb/tempo/device.toml".to_owned(),
        }
    }
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            runtime_quantum_us: 50_000,
            // A demand move smaller than this is below the controller's own
            // stepping resolution, so publishing it would only cost a wake-up.
            demand_step: 0.08,
            // Empty means "discover the active backlight panel at runtime".
            screen_path: String::new(),
            frame_stride: 2,
            quiet_decay_ms: 60,
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            quiet_ratio: 0.50,
            quiet_cap: 0.28,
            // Foreground micro-motion on a static page (status bar, cursor
            // blink, text caret) stays in a narrow band well below 5% of a
            // cluster; a real interaction or scroll crosses 12% within a few
            // frames. The band between the two thresholds is hysteresis: it
            // cannot enter the floor on noise, and cannot leave it on noise.
            // A page repaint was measured at 8.5% on the balanced cluster,
            // hence the 12% exit threshold.
            floor_enter_ratio: 0.05,
            floor_exit_ratio: 0.12,
            floor_settle_ms: 60,
            floor_exit_ms: 60,
            // Time constant of the foreground fast filter used only for floor
            // exit. Entry uses the instantaneous ratio, so it stays responsive.
            fg_fast_ms: 100,
            // Event-driven lifts must not be blocked at min frequency; a non-zero
            // value is available for devices that need stronger anti-flapping.
            floor_residence_ms: 0,
            drop_ratio: 0.08,
            // Gentle frame-driven lift: two 2x steps instead of a 4x slam.
            drop_factor: 2.0,
            touch_lift_ms: 150,
            // Mid tier, not the top of the prime cluster. 0.30 lands near
            // 0.82 / 1.19 / 1.42 GHz for the three clusters on babylon.
            touch_lift_level: 0.30,
            touch_ramp_ms: 60,
            touch_ramp_stages: 3,
            touch_decay_ms: 45,
            launch_lift_ms: 1000,
            launch_lift_level: 1.00,
            prime_lift_level: 0.45,
            prime_load_cap: 0.78,
            ceiling_headroom: 0.18,
            frame_slack_ms: 3.0,
            frame_burst_ms: 150,
            frame_burst_level: 0.70,
            reserve_ratio: 1.05,
            step_hysteresis: 0.03,
            parked_headroom: 0.08,
            role_cap: RoleCaps::default(),
        }
    }
}

impl Default for RoleCaps {
    fn default() -> Self {
        Self {
            efficiency: 1.00,
            balanced: 1.00,
            prime: 1.00,
        }
    }
}

impl Default for FasConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            apps: Vec::new(),
            frame_budget_ms: 16.0,
        }
    }
}

impl Default for ChargeConfig {
    fn default() -> Self {
        Self {
            stop_at_percent: 80,
            resume_at_percent: 70,
            temp_stop_c: 45.0,
            temp_resume_c: 40.0,
            latch_per_plug: false,
            full_current_ma: 100,
            full_hold_seconds: 9,
            full_voltage_mv: None,
        }
    }
}

impl Default for ThermalConfig {
    fn default() -> Self {
        Self {
            guard_c: 45.0,
            profile: "auto".to_owned(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "zh".to_owned(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml_string(&self) -> Result<String, CoreError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Reject configurations that could produce unsafe or ambiguous behaviour.
    pub fn validate(&self) -> Result<(), CoreError> {
        let mut bad = Vec::new();

        if self.schema != SCHEMA_VERSION {
            bad.push(format!(
                "schema must be {SCHEMA_VERSION}, got {}",
                self.schema
            ));
        }
        if self.daemon.profile_path.trim().is_empty() {
            bad.push("daemon.profile_path must not be empty".to_owned());
        }
        if !(500..=300_000).contains(&self.events.runtime_quantum_us) {
            bad.push("events.runtime_quantum_us must be 500..=300000".to_owned());
        }
        if !(0.01..=0.20).contains(&self.events.demand_step) {
            bad.push("events.demand_step must be 0.01..=0.20".to_owned());
        }
        if !self.events.screen_path.is_empty() && !self.events.screen_path.starts_with('/') {
            bad.push("events.screen_path must be empty or an absolute path".to_owned());
        }
        if !(1..=16).contains(&self.events.frame_stride) {
            bad.push("events.frame_stride must be 1..=16".to_owned());
        }
        if !(20..=2_000).contains(&self.events.quiet_decay_ms) {
            bad.push("events.quiet_decay_ms must be 20..=2000".to_owned());
        }

        let c = &self.control;
        if !(0.05..=0.95).contains(&c.quiet_ratio) {
            bad.push("control.quiet_ratio must be 0.05..=0.95".to_owned());
        }
        if !(0.05..=0.90).contains(&c.quiet_cap) {
            bad.push("control.quiet_cap must be 0.05..=0.90".to_owned());
        }
        if !(0.005..=0.20).contains(&c.floor_enter_ratio) {
            bad.push("control.floor_enter_ratio must be 0.005..=0.20".to_owned());
        }
        if !(0.02..=0.40).contains(&c.floor_exit_ratio) {
            bad.push("control.floor_exit_ratio must be 0.02..=0.40".to_owned());
        }
        if c.floor_exit_ratio <= c.floor_enter_ratio {
            bad.push("control.floor_exit_ratio must be > control.floor_enter_ratio".to_owned());
        }
        if c.floor_enter_ratio > c.drop_ratio {
            bad.push("control.floor_enter_ratio must be <= control.drop_ratio".to_owned());
        }
        if !(10..=1_000).contains(&c.floor_settle_ms) {
            bad.push("control.floor_settle_ms must be 10..=1000".to_owned());
        }
        if !(20..=500).contains(&c.floor_exit_ms) {
            bad.push("control.floor_exit_ms must be 20..=500".to_owned());
        }
        if !(20..=500).contains(&c.fg_fast_ms) {
            bad.push("control.fg_fast_ms must be 20..=500".to_owned());
        }
        if c.floor_residence_ms > 2_000 {
            bad.push("control.floor_residence_ms must be <= 2000".to_owned());
        }
        if self.events.quiet_decay_ms + c.floor_settle_ms > 300 {
            bad.push(
                "quiet entry budget (quiet_decay_ms + floor_settle_ms) must be <= 300".to_owned(),
            );
        }
        if !(0.01..=0.30).contains(&c.drop_ratio) {
            bad.push("control.drop_ratio must be 0.01..=0.30".to_owned());
        }
        if !(1.0..=16.0).contains(&c.drop_factor) {
            bad.push("control.drop_factor must be 1.0..=16.0".to_owned());
        }
        if !(20..=600).contains(&c.touch_lift_ms) {
            bad.push("control.touch_lift_ms must be 20..=600".to_owned());
        }
        if !(0.10..=1.0).contains(&c.touch_lift_level) {
            bad.push("control.touch_lift_level must be 0.10..=1.0".to_owned());
        }
        if !(200..=1500).contains(&c.launch_lift_ms) {
            bad.push("control.launch_lift_ms must be 200..=1500".to_owned());
        }
        if !(0.50..=1.0).contains(&c.launch_lift_level) {
            bad.push("control.launch_lift_level must be 0.50..=1.0".to_owned());
        }
        if !(10..=300).contains(&c.touch_ramp_ms) {
            bad.push("control.touch_ramp_ms must be 10..=300".to_owned());
        }
        if !(2..=4).contains(&c.touch_ramp_stages) {
            bad.push("control.touch_ramp_stages must be 2..=4".to_owned());
        }
        if !(10..=200).contains(&c.touch_decay_ms) {
            bad.push("control.touch_decay_ms must be 10..=200".to_owned());
        }
        if !(0.0..=0.30).contains(&c.parked_headroom) {
            bad.push("control.parked_headroom must be 0.0..=0.30".to_owned());
        }
        if !(0.05..=0.50).contains(&c.ceiling_headroom) {
            bad.push("control.ceiling_headroom must be 0.05..=0.50".to_owned());
        }
        if !(0.5..=20.0).contains(&c.frame_slack_ms) {
            bad.push("control.frame_slack_ms must be 0.5..=20.0".to_owned());
        }
        if !(40..=1_000).contains(&c.frame_burst_ms) {
            bad.push("control.frame_burst_ms must be 40..=1000".to_owned());
        }
        if !(0.10..=1.0).contains(&c.frame_burst_level) {
            bad.push("control.frame_burst_level must be 0.10..=1.0".to_owned());
        }
        if !(1.0..=1.25).contains(&c.reserve_ratio) {
            bad.push("control.reserve_ratio must be 1.0..=1.25".to_owned());
        }
        if !(0.002..=0.20).contains(&c.step_hysteresis) {
            bad.push("control.step_hysteresis must be 0.002..=0.20".to_owned());
        }
        for (name, value) in [
            ("efficiency", c.role_cap.efficiency),
            ("balanced", c.role_cap.balanced),
            ("prime", c.role_cap.prime),
        ] {
            if !(0.20..=1.0).contains(&value) {
                bad.push(format!("control.role_cap.{name} must be 0.20..=1.0"));
            }
        }

        let ch = &self.charge;
        if !(50..=100).contains(&ch.stop_at_percent) {
            bad.push("charge.stop_at_percent must be 50..=100".to_owned());
        }
        if ch.resume_at_percent >= ch.stop_at_percent {
            bad.push("charge.resume_at_percent must be < charge.stop_at_percent".to_owned());
        }
        if !(20..=ch.stop_at_percent).contains(&ch.resume_at_percent) {
            bad.push("charge.resume_at_percent must be 20..=stop_at_percent".to_owned());
        }
        if !(20.0..=70.0).contains(&ch.temp_stop_c) {
            bad.push("charge.temp_stop_c must be 20..=70".to_owned());
        }
        if ch.temp_resume_c >= ch.temp_stop_c {
            bad.push("charge.temp_resume_c must be < charge.temp_stop_c".to_owned());
        }
        if !(20..=1000).contains(&ch.full_current_ma) {
            bad.push("charge.full_current_ma must be 20..=1000".to_owned());
        }
        if !(1..=60).contains(&ch.full_hold_seconds) {
            bad.push("charge.full_hold_seconds must be 1..=60".to_owned());
        }
        if let Some(voltage) = ch.full_voltage_mv {
            if !(3000..=5000).contains(&voltage) {
                bad.push("charge.full_voltage_mv must be 3000..=5000".to_owned());
            }
        }
        if self.fas.apps.len() > 64 {
            bad.push("fas.apps must contain at most 64 entries".to_owned());
        }
        for app in &self.fas.apps {
            let trimmed = app.trim();
            if trimmed.is_empty() || trimmed.len() > 128 || trimmed.split_whitespace().count() != 1 {
                bad.push(format!("fas.apps entry {app:?} is not a valid package name"));
            }
        }
        if !(6.0..=40.0).contains(&self.fas.frame_budget_ms) {
            bad.push("fas.frame_budget_ms must be 6..=40".to_owned());
        }

        if !(20.0..=80.0).contains(&self.thermal.guard_c) {
            bad.push("thermal.guard_c must be 20..=80".to_owned());
        }
        if self.thermal.profile.trim().is_empty() {
            bad.push("thermal.profile must not be empty".to_owned());
        }
        if self.ui.language.trim().is_empty() {
            bad.push("ui.language must not be empty".to_owned());
        }

        if bad.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Config(bad.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_roundtrip() {
        let cfg = Config::default();
        cfg.validate().expect("defaults must be valid");
        let text = cfg.to_toml_string().expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("parse");
        assert_eq!(parsed, cfg);
        assert_eq!(parsed.schema, SCHEMA_VERSION);
        assert_eq!(parsed.events.quiet_decay_ms, 60);
        assert_eq!(parsed.control.floor_settle_ms, 60);
        assert_eq!(parsed.control.floor_enter_ratio, 0.05);
        assert_eq!(parsed.control.floor_exit_ratio, 0.12);
        assert_eq!(parsed.control.fg_fast_ms, 100);
    }

    #[test]
    fn config_rejects_bad_ranges() {
        let mut cfg = Config::default();
        cfg.control.quiet_ratio = 0.99;
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.charge.resume_at_percent = 85;
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.control.floor_exit_ratio = cfg.control.floor_enter_ratio;
        assert!(cfg.validate().is_err(), "exit must exceed enter");

        let mut cfg = Config::default();
        cfg.events.quiet_decay_ms = 500;
        cfg.control.floor_settle_ms = 500;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_denies_unknown_fields() {
        let text = "schema = 1\n[control]\nmystery_knob = 1\n";
        let parsed = toml::from_str::<Config>(text);
        assert!(parsed.is_err());
    }
}
