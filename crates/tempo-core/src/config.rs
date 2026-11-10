// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::collections::BTreeMap;
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
    /// 完全关闭日志：不打开日志文件，也不产生任何追踪记录。
    Off,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Off
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
            Self::Off => "off",
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
    pub aegis_frame: AegisFrameConfig,
    pub aegis_app: AegisAppConfig,
    pub governor: GovernorConfig,
    pub ui: UiConfig,
}

/// 厂商 walt governor 的换档节奏接管。
///
/// 只调整“多久允许换一次档、是否做预测”，不设置任何频率上限，也不改
/// `scaling_min_freq` / `scaling_max_freq`：默认档位仍由调度器自身的档位表决定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GovernorConfig {
    /// 总开关。关闭后完全保持厂商原样。
    pub smooth: bool,
    /// walt 的预测开关（`walt/pl`）。预测会放大短时唤醒，实测是顶频尖峰的主要来源。
    pub prediction: bool,
    /// 升频换档最小间隔（微秒）。只推迟换档时机，不限制能到达的最高频。
    pub up_rate_limit_us: u64,
    /// 降频换档最小间隔（微秒）。0 表示不限制降频，负载一落就立刻降档。
    pub down_rate_limit_us: u64,
    /// 检测到被厂商/其它模块改回默认时的重写周期；0 表示只在启动和重载时写一次。
    pub reapply_ms: u64,
}

/// Frame feedback is opt-in per application. The default is fully off: an empty
/// list means frame feedback never runs, not "every application".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AegisFrameConfig {
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
    /// Optional hardware-max ratios keyed by the cluster's first CPU (e.g. cpu0).
    pub cluster_cap_ratio: BTreeMap<String, f32>,
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
    /// Legacy hard-cap setting retained for config compatibility.
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

    // ---- v3 tiered ladder ------------------------------------------------
    /// Entries in the per-cluster ladder (clamped to 6..=10).
    pub tier_table_max: usize,
    /// How long a load-driven upward move must be sustained before a step.
    pub up_confirm_ms: u64,
    /// Minimum spacing between two downward steps (one tier each).
    pub down_step_ms: u64,
    /// Maximum tiers a load/event step may climb at once.
    pub max_up_step: u32,
    /// Touch adds this many tiers (never the prime cluster).
    pub touch_lift_tiers: u32,
    /// Warm-start tier as a fraction of the ladder. Deliberately below 1.0.
    pub launch_tier_ratio: f32,
    /// Limits speculative prime targets, not sustained load or the governor maximum.
    pub prime_cap_ratio: f32,
    /// Limits the target tier while an input method owns the foreground.
    pub ime_cap_ratio: f32,
    /// Legacy hard-cap setting retained for config compatibility.
    pub safety_ceiling_ratio: f32,
    /// Legacy hard-cap setting retained for config compatibility.
    pub lookahead_tiers: u32,
    /// Read-only guard after daemon start; no sysfs writes inside this window.
    pub startup_guard_ms: u64,
    /// Frame pressure is held this long after the last over-budget frame.
    pub pressure_hold_ms: u64,
    /// EMA time constant of the demand filters.
    pub demand_smooth_ms: u64,
}

/// Per-app-class floor calibration. Reading and video floors are tier indices
/// into the per-cluster ladder; the controller raises a floor automatically if
/// an over-budget frame is seen (the automatic form of "walk down to zero drops").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AegisAppConfig {
    pub reading_floor_tier: usize,
    pub video_floor_tier: usize,
    pub default_floor_tier: usize,
    pub reading_packages: Vec<String>,
    pub video_packages: Vec<String>,
    pub ime_packages: Vec<String>,
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
            aegis_frame: AegisFrameConfig::default(),
            aegis_app: AegisAppConfig::default(),
            governor: GovernorConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            // 实测（设置页轻滑 10 秒 ftrace，Aegis 冻结、纯 governor 对比）：
            // 厂商标配中核换档 1872 次、命中 2707200 共 80 次、平均 1187~1208MHz；
            // 关预测 + 40ms 升频节流 + 不限制降频后降到约 1038MHz，
            // 顶频驻留从 9~10ms 降到 0~1ms，Settings 滑动 0 掉帧，冷启动无回归。
            // 降频不加节流是关键：加了反而把高频“挂”住，平均频率会升高。
            smooth: true,
            prediction: false,
            up_rate_limit_us: 40_000,
            down_rate_limit_us: 0,
            reapply_ms: 60_000,
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Off,
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
            cluster_cap_ratio: BTreeMap::new(),
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
            tier_table_max: 8,
            up_confirm_ms: 120,
            down_step_ms: 45,
            max_up_step: 2,
            touch_lift_tiers: 1,
            launch_tier_ratio: 0.62,
            prime_cap_ratio: 0.70,
            ime_cap_ratio: 0.45,
            safety_ceiling_ratio: 0.35,
            lookahead_tiers: 3,
            startup_guard_ms: 20_000,
            pressure_hold_ms: 1_000,
            demand_smooth_ms: 200,
        }
    }
}

impl Default for AegisAppConfig {
    fn default() -> Self {
        Self {
            reading_floor_tier: 0,
            // Video decode is mostly off-CPU. Start at the hardware minimum and
            // let an over-budget frame raise the adaptive floor; this is the
            // automatic equivalent of the 180 s zero-drop calibration.
            video_floor_tier: 0,
            default_floor_tier: 0,
            reading_packages: vec![
                "com.dragon.read".to_owned(),
                "com.tencent.weread".to_owned(),
                "com.qidian.QDReader".to_owned(),
                "com.ireader".to_owned(),
                "com.duokan.reader".to_owned(),
            ],
            video_packages: vec![
                "tv.danmaku.bili".to_owned(),
                "com.ss.android.ugc.aweme".to_owned(),
                "com.ss.android.ugc.aweme.lite".to_owned(),
                "com.phoenix.read".to_owned(),
                "com.qiyi.video".to_owned(),
                "com.tencent.qqlive".to_owned(),
                "com.youku.phone".to_owned(),
                "com.hunantv.imgo.activity".to_owned(),
                "com.google.android.youtube".to_owned(),
            ],
            ime_packages: vec![
                "com.sohu.inputmethod.sogou".to_owned(),
                "com.baidu.input".to_owned(),
                "com.iflytek.inputmethod".to_owned(),
                "com.google.android.inputmethod.latin".to_owned(),
                "com.miui.inputmethod".to_owned(),
                "com.tencent.wetype".to_owned(),
            ],
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

impl Default for AegisFrameConfig {
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

    /// Canonical, minimal TOML. Arrays stay on one line, there are no trailing
    /// commas and no inline comments, so the same subset the console parses is
    /// always what the daemon emits. The result is parsed back and validated
    /// before it is handed out: an invalid document is never returned.
    pub fn to_toml_string(&self) -> Result<String, CoreError> {
        let text = toml::to_string(self)?;
        let parsed: Self = toml::from_str(&text)?;
        parsed.validate()?;
        Ok(text)
    }

    /// Rewrite `path` in canonical form when the stored file differs. The new
    /// document is written to a temporary file, read back and validated, then
    /// renamed over the original; a failure leaves the old file untouched.
    /// Returns `true` when the file was replaced.
    pub fn normalize_file(&self, path: &Path) -> Result<bool, CoreError> {
        let canonical = self.to_toml_string()?;
        let current = std::fs::read_to_string(path).unwrap_or_default();
        if current.trim() == canonical.trim() {
            return Ok(false);
        }
        let tmp = path.with_extension("toml.apply");
        std::fs::write(&tmp, canonical.as_bytes())?;
        let readback = Self::load(&tmp);
        if let Err(error) = readback {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        std::fs::rename(&tmp, path)?;
        Ok(true)
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
        for (key, ratio) in &c.cluster_cap_ratio {
            let valid_key = key.strip_prefix("cpu").is_some_and(|cpu| {
                !cpu.is_empty()
                    && cpu.bytes().all(|byte| byte.is_ascii_digit())
                    && cpu.parse::<u32>().is_ok()
            });
            if !valid_key {
                bad.push(format!(
                    "control.cluster_cap_ratio key {key:?} must be cpu followed by a u32"
                ));
            }
            if !ratio.is_finite() || *ratio <= 0.0 || *ratio > 1.0 {
                bad.push(format!(
                    "control.cluster_cap_ratio.{key} must be finite and in (0, 1]"
                ));
            }
        }
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

        if !(6..=10).contains(&c.tier_table_max) {
            bad.push("control.tier_table_max must be 6..=10".to_owned());
        }
        if !(40..=400).contains(&c.up_confirm_ms) {
            bad.push("control.up_confirm_ms must be 40..=400".to_owned());
        }
        if !(15..=200).contains(&c.down_step_ms) {
            bad.push("control.down_step_ms must be 15..=200".to_owned());
        }
        if !(1..=4).contains(&c.max_up_step) {
            bad.push("control.max_up_step must be 1..=4".to_owned());
        }
        if !(1..=3).contains(&c.touch_lift_tiers) {
            bad.push("control.touch_lift_tiers must be 1..=3".to_owned());
        }
        if !(0.30..=0.90).contains(&c.launch_tier_ratio) {
            bad.push("control.launch_tier_ratio must be 0.30..=0.90".to_owned());
        }
        if !(0.30..=0.90).contains(&c.prime_cap_ratio) {
            bad.push("control.prime_cap_ratio must be 0.30..=0.90".to_owned());
        }
        if !(0.25..=0.80).contains(&c.ime_cap_ratio) {
            bad.push("control.ime_cap_ratio must be 0.25..=0.80".to_owned());
        }
        if !(0.20..=0.70).contains(&c.safety_ceiling_ratio) {
            bad.push("control.safety_ceiling_ratio must be 0.20..=0.70".to_owned());
        }
        if !(1..=5).contains(&c.lookahead_tiers) {
            bad.push("control.lookahead_tiers must be 1..=5".to_owned());
        }
        if !(5_000..=120_000).contains(&c.startup_guard_ms) {
            bad.push("control.startup_guard_ms must be 5000..=120000".to_owned());
        }
        if !(300..=5_000).contains(&c.pressure_hold_ms) {
            bad.push("control.pressure_hold_ms must be 300..=5000".to_owned());
        }
        if !(50..=1_000).contains(&c.demand_smooth_ms) {
            bad.push("control.demand_smooth_ms must be 50..=1000".to_owned());
        }

        if self.governor.up_rate_limit_us > 100_000 {
            bad.push("governor.up_rate_limit_us must be <= 100000".to_owned());
        }
        if self.governor.down_rate_limit_us > 200_000 {
            bad.push("governor.down_rate_limit_us must be <= 200000".to_owned());
        }
        if self.governor.reapply_ms > 600_000 {
            bad.push("governor.reapply_ms must be <= 600000".to_owned());
        }

        for (name, value) in [
            ("reading_floor_tier", self.aegis_app.reading_floor_tier),
            ("video_floor_tier", self.aegis_app.video_floor_tier),
            ("default_floor_tier", self.aegis_app.default_floor_tier),
        ] {
            if value > 5 {
                bad.push(format!("aegis_app.{name} must be <= 5"));
            }
        }
        for (name, list) in [
            ("reading_packages", &self.aegis_app.reading_packages),
            ("video_packages", &self.aegis_app.video_packages),
            ("ime_packages", &self.aegis_app.ime_packages),
        ] {
            if list.len() > 64 {
                bad.push(format!(
                    "aegis_app.{name} must contain at most 64 entries"
                ));
            }
            for app in list {
                let trimmed = app.trim();
                if trimmed.is_empty()
                    || trimmed.len() > 128
                    || trimmed.split_whitespace().count() != 1
                {
                    bad.push(format!(
                        "aegis_app.{name} entry {app:?} is not a valid package name"
                    ));
                }
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
        if self.aegis_frame.apps.len() > 64 {
            bad.push("aegis_frame.apps must contain at most 64 entries".to_owned());
        }
        for app in &self.aegis_frame.apps {
            let trimmed = app.trim();
            if trimmed.is_empty() || trimmed.len() > 128 || trimmed.split_whitespace().count() != 1
            {
                bad.push(format!(
                    "aegis_frame.apps entry {app:?} is not a valid package name"
                ));
            }
        }
        if !(6.0..=40.0).contains(&self.aegis_frame.frame_budget_ms) {
            bad.push("aegis_frame.frame_budget_ms must be 6..=40".to_owned());
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
    fn packaged_config_is_valid_and_frame_feedback_is_opt_in() {
        let cfg: Config = toml::from_str(include_str!("../../../packaging/config.toml"))
            .expect("模块默认配置必须能被当前版本解析");
        cfg.validate().expect("模块默认配置必须有效");
        assert!(!cfg.aegis_frame.enabled);
    }

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

    /// The console parses one line at a time: arrays must never span lines and
    /// the document must contain no inline comments or trailing commas.
    #[test]
    fn config_serializes_compact_arrays() {
        let cfg = Config::default();
        let text = cfg.to_toml_string().expect("serialize");
        for line in text.lines() {
            let body = line.trim();
            if let Some((_, value)) = body.split_once('=') {
                let value = value.trim();
                if value.starts_with('[') {
                    assert!(value.ends_with(']'), "array must close on one line: {line}");
                    assert!(!value.contains('#'), "no inline comments: {line}");
                }
            }
            assert!(!body.ends_with(','), "no trailing comma: {line}");
        }
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
    fn cluster_cap_ratios_are_optional_and_roundtrip() {
        let mut cfg: Config = toml::from_str("schema = 1\n[control]\nquiet_ratio = 0.5\n").unwrap();
        assert!(cfg.control.cluster_cap_ratio.is_empty());
        cfg.control.cluster_cap_ratio.insert("cpu0".into(), 0.5);
        cfg.control
            .cluster_cap_ratio
            .insert("cpu4294967295".into(), 1.0);
        cfg.validate().unwrap();
        let parsed: Config = toml::from_str(&cfg.to_toml_string().unwrap()).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn cluster_cap_ratios_reject_invalid_keys_and_values() {
        for key in [
            "",
            "cpu",
            "CPU0",
            "prime",
            "cpu-1",
            "cpu+1",
            "cpu1.0",
            "cpu 0",
            "cpu4294967296",
        ] {
            let mut cfg = Config::default();
            cfg.control.cluster_cap_ratio.insert(key.into(), 0.5);
            assert!(cfg.validate().is_err(), "invalid key: {key}");
        }
        for ratio in [0.0, -0.5, 1.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut cfg = Config::default();
            cfg.control.cluster_cap_ratio.insert("cpu0".into(), ratio);
            assert!(cfg.validate().is_err(), "invalid ratio: {ratio}");
        }
    }

    #[test]
    fn config_denies_unknown_fields() {
        let text = "schema = 1\n[control]\nmystery_knob = 1\n";
        let parsed = toml::from_str::<Config>(text);
        assert!(parsed.is_err());
    }
}
