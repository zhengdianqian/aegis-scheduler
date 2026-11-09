// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Tiered adaptive controller (v3).
//!
//! Every cluster is reduced to a 6..10 entry frequency ladder derived from
//! `scaling_available_frequencies`. The controller walks that ladder instead of
//! switching between "hardware minimum" and "hardware maximum":
//!
//! * upward moves need a short confirmation window (load driven) or are event
//!   driven (touch / launch / over-budget frame);
//! * downward moves are rate limited to one tier per `down_step_ms`;
//! * a one tier hysteresis keeps the ladder from flapping;
//! * the scene floor (reading / video) anchors the operating point, so a video
//!   does not run at the hardware minimum just because its CPU demand is low;
//! * the writer's upper cap is independent of the target: only thermal limits
//!   and explicitly configured per-cluster hardware-max ratios restrict it.
//!
//! The controller is a pure state machine: explicit monotonic timestamps in,
//! decisions out. It never reads sysfs and installs no repeating timers.

use serde::Serialize;

use crate::config::Config;
use crate::profile::{DeviceProfile, Role};

const US_PER_MS: u64 = 1_000;
// FAS 每 2 秒反馈一次，额外留出 500 毫秒调度抖动余量。
const OVER_BUDGET_SAMPLE_GAP_US: u64 = 2_500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    Quiet,
    ScreenOff,
    TouchLift,
    FrameBurst,
    Foreground,
    Launch,
    Load,
    Thermal,
    Unchanged,
}

/// Workload class inferred from the foreground package name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    Default,
    Reading,
    Video,
    Ime,
}

impl Scene {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Reading => "reading",
            Self::Video => "video",
            Self::Ime => "ime",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub cluster: usize,
    /// Lower bound for `scaling_min_freq`. The historical name is kept for compatibility.
    pub ceiling_khz: u32,
    /// Upper bound for `scaling_max_freq`; `ceiling_khz <= cap_khz <= hw max`.
    pub cap_khz: u32,
    pub tier: u32,
    pub tier_count: u32,
    pub reason: DecisionReason,
    pub changed: bool,
    /// True when the cluster sits on its lowest tier with no touch, launch or
    /// load pressure. Only then may the writer lower the vendor boost floor.
    pub idle: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DemandSample {
    pub ts_us: u64,
    /// Total busy ratio per cluster, 0..1.
    pub cluster_busy: Vec<f32>,
    /// Foreground-only busy ratio per cluster, 0..1.
    pub foreground_busy: Vec<f32>,
    pub foreground_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameSample {
    pub ts_us: u64,
    /// Time from frame start to submission, in milliseconds.
    pub render_ms: f32,
    /// Frame budget for the current refresh mode, in milliseconds.
    pub budget_ms: f32,
    pub foreground: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineKind {
    LiftEnd,
    BurstEnd,
    QuietEntry,
    FloorExit,
    /// Next confirmation / upward step.
    Ramp,
    /// Next downward step while the controller walks back to the target.
    Decay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub at_us: u64,
    pub kind: DeadlineKind,
}

#[derive(Debug, Clone)]
struct ClusterState {
    tier: usize,
    user_cap_khz: u32,
    last_cap_khz: u32,
    busy_ema: f32,
    fg_ema: f32,
    busy_raw: f32,
    fg_raw: f32,
    up_since_us: Option<u64>,
    last_step_us: u64,
    touch_target: usize,
}

impl ClusterState {
    fn new(user_cap_khz: u32) -> Self {
        Self {
            tier: 0,
            user_cap_khz,
            last_cap_khz: u32::MAX,
            busy_ema: 0.0,
            fg_ema: 0.0,
            busy_raw: 0.0,
            fg_raw: 0.0,
            up_since_us: None,
            last_step_us: 0,
            touch_target: 0,
        }
    }
}

pub struct Controller {
    profile: DeviceProfile,
    config: Config,
    /// Frequency ladder per cluster, ascending, 6..10 entries.
    tiers: Vec<Vec<u32>>,
    clusters: Vec<ClusterState>,
    screen_on: bool,
    quiet: bool,
    last_touch_us: u64,
    touch_active: bool,
    low_demand_since_us: Option<u64>,
    lift_until_us: Option<u64>,
    launch_until_us: Option<u64>,
    burst_until_us: Option<u64>,
    pressure_until_us: Option<u64>,
    last_over_budget_us: u64,
    over_budget_hits: u32,
    last_demand_us: Option<u64>,
    filter_updated_us: Option<u64>,
    thermal_cap: Option<f32>,
    scene: Scene,
    /// Raised when a video/reading scene still produces over-budget frames; it
    /// is the automatic form of the "walk down until zero drops" calibration.
    adaptive_floor: usize,
}

impl Controller {
    pub fn new(profile: DeviceProfile, config: Config) -> Self {
        let tiers = profile
            .clusters
            .iter()
            .map(|cluster| {
                build_ladder(
                    &cluster.freq_khz,
                    &cluster.min_khz,
                    &cluster.max_khz,
                    config.control.tier_table_max,
                )
            })
            .collect();
        let clusters = profile
            .clusters
            .iter()
            .map(|cluster| {
                let ratio = config
                    .control
                    .cluster_cap_ratio
                    .get(&format!("cpu{}", cluster.cpus[0]))
                    .copied()
                    .unwrap_or(1.0);
                let cap_khz = if ratio >= 1.0 {
                    cluster.max_khz
                } else {
                    let limit = f64::from(cluster.max_khz) * f64::from(ratio);
                    let end = cluster
                        .freq_khz
                        .partition_point(|&khz| f64::from(khz) <= limit);
                    if end == 0 {
                        cluster.min_khz
                    } else {
                        cluster.freq_khz[end - 1]
                    }
                };
                ClusterState::new(cap_khz)
            })
            .collect();
        Self {
            profile,
            config,
            tiers,
            clusters,
            screen_on: true,
            quiet: false,
            last_touch_us: 0,
            touch_active: false,
            low_demand_since_us: None,
            lift_until_us: None,
            launch_until_us: None,
            burst_until_us: None,
            pressure_until_us: None,
            last_over_budget_us: 0,
            over_budget_hits: 0,
            last_demand_us: None,
            filter_updated_us: None,
            thermal_cap: None,
            scene: Scene::Default,
            adaptive_floor: 0,
        }
    }

    pub fn profile(&self) -> &DeviceProfile {
        &self.profile
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    pub fn ceiling_khz(&self, index: usize) -> u32 {
        let cluster = &self.clusters[index];
        self.tiers[index][cluster.tier.min(self.tiers[index].len() - 1)].min(self.cap_khz(index))
    }

    pub fn cap_khz(&self, index: usize) -> u32 {
        let user_cap = self.clusters[index].user_cap_khz;
        if self.thermal_cap.is_some() {
            user_cap.min(self.tiers[index][self.thermal_cap_tier(index)])
        } else {
            user_cap
        }
    }

    pub fn tier(&self, index: usize) -> usize {
        self.clusters[index].tier
    }

    pub fn tier_count(&self, index: usize) -> usize {
        self.tiers[index].len()
    }

    pub fn tier_table(&self, index: usize) -> &[u32] {
        &self.tiers[index]
    }

    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub fn is_screen_on(&self) -> bool {
        self.screen_on
    }

    pub fn scene(&self) -> Scene {
        self.scene
    }

    pub fn on_screen(&mut self, ts_us: u64, on: bool) -> Vec<Decision> {
        self.screen_on = on;
        if on {
            // A screen-on is a warm start: hold a mid-tier target for
            // the launch window so the lock screen and the first frame do not
            // run at the hardware minimum.
            self.launch_until_us = Some(ts_us + self.config.control.launch_lift_ms * US_PER_MS);
            self.quiet = false;
            self.low_demand_since_us = None;
            self.evaluate(ts_us, DecisionReason::Launch)
        } else {
            self.quiet = false;
            self.low_demand_since_us = None;
            self.lift_until_us = None;
            self.touch_active = false;
            self.launch_until_us = None;
            self.burst_until_us = None;
            self.pressure_until_us = None;
            self.over_budget_hits = 0;
            self.last_demand_us = None;
            self.filter_updated_us = None;
            for cluster in self.clusters.iter_mut() {
                cluster.up_since_us = None;
                cluster.busy_ema = 0.0;
                cluster.fg_ema = 0.0;
                cluster.busy_raw = 0.0;
                cluster.fg_raw = 0.0;
                cluster.touch_target = 0;
            }
            self.evaluate(ts_us, DecisionReason::ScreenOff)
        }
    }

    /// Restore screen state after configuration reload without a synthetic launch.
    pub fn restore_screen_state(&mut self, ts_us: u64, on: bool) -> Vec<Decision> {
        if !on {
            return self.on_screen(ts_us, false);
        }
        self.screen_on = true;
        self.launch_until_us = None;
        self.evaluate(ts_us, DecisionReason::Load)
    }

    pub fn on_touch(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        // 同一接触会话只锁定一次目标，长按停顿后移动也不能叠加抬频。
        if !self.touch_active {
            self.touch_active = true;
            let steps = self.config.control.touch_lift_tiers.max(1) as usize;
            for (index, cluster) in self.clusters.iter_mut().enumerate() {
                cluster.touch_target = (cluster.tier + steps).min(self.tiers[index].len() - 1);
            }
        }
        self.last_touch_us = ts_us;
        self.quiet = false;
        self.low_demand_since_us = None;
        for cluster in self.clusters.iter_mut() {
            cluster.up_since_us = None;
        }
        self.lift_until_us = Some(ts_us + self.config.control.touch_lift_ms * US_PER_MS);
        self.evaluate(ts_us, DecisionReason::TouchLift)
    }

    pub fn on_touch_up(&mut self, ts_us: u64) -> Vec<Decision> {
        self.touch_active = false;
        self.lift_until_us = None;
        if !self.screen_on {
            return Vec::new();
        }
        self.last_touch_us = ts_us;
        self.evaluate(ts_us, DecisionReason::Load)
    }

    pub fn on_frame(&mut self, sample: FrameSample) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        let over = sample.render_ms > sample.budget_ms + self.config.control.frame_slack_ms;
        if !sample.foreground {
            return Vec::new();
        }
        if !over {
            self.over_budget_hits = 0;
            return Vec::new();
        }

        let recent = self.over_budget_hits > 0
            && sample.ts_us.saturating_sub(self.last_over_budget_us) <= OVER_BUDGET_SAMPLE_GAP_US;
        self.over_budget_hits = if recent {
            self.over_budget_hits.saturating_add(1)
        } else {
            1
        };
        self.last_over_budget_us = sample.ts_us;
        if self.over_budget_hits >= 3 {
            self.pressure_until_us =
                Some(sample.ts_us + self.config.control.pressure_hold_ms * US_PER_MS);
        }

        self.quiet = false;
        self.low_demand_since_us = None;
        for cluster in self.clusters.iter_mut() {
            cluster.up_since_us = None;
        }
        self.burst_until_us = Some(sample.ts_us + self.config.control.frame_burst_ms * US_PER_MS);

        // 单个弹幕或切页尖峰只给短时救援；持续超预算才提高会话地板。
        if self.over_budget_hits >= 3 && matches!(self.scene, Scene::Video | Scene::Reading) {
            let ceiling = self
                .tiers
                .iter()
                .map(Vec::len)
                .min()
                .unwrap_or(1)
                .saturating_sub(1);
            let max_floor = (ceiling / 3).max(1);
            self.adaptive_floor = self.adaptive_floor.saturating_add(1).min(max_floor);
        }

        self.evaluate(sample.ts_us, DecisionReason::FrameBurst)
    }

    pub fn on_foreground_changed(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        for cluster in self.clusters.iter_mut() {
            cluster.up_since_us = None;
        }
        self.launch_until_us = Some(ts_us + self.config.control.launch_lift_ms * US_PER_MS);
        self.quiet = false;
        self.low_demand_since_us = None;
        self.evaluate(ts_us, DecisionReason::Foreground)
    }

    pub fn on_app_launch(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        for cluster in self.clusters.iter_mut() {
            cluster.up_since_us = None;
        }
        self.launch_until_us = Some(ts_us + self.config.control.launch_lift_ms * US_PER_MS);
        self.quiet = false;
        self.low_demand_since_us = None;
        self.evaluate(ts_us, DecisionReason::Launch)
    }

    pub fn on_app_displayed(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        let Some(until) = self.launch_until_us else {
            return Vec::new();
        };
        if ts_us >= until {
            return Vec::new();
        }
        self.launch_until_us = Some(until.min(ts_us.saturating_add(80 * US_PER_MS)));
        self.evaluate(ts_us, DecisionReason::Load)
    }

    pub fn on_scene(&mut self, ts_us: u64, scene: Scene) -> Vec<Decision> {
        if scene == self.scene {
            return Vec::new();
        }
        self.scene = scene;
        self.adaptive_floor = 0;
        self.low_demand_since_us = None;
        for cluster in self.clusters.iter_mut() {
            cluster.up_since_us = None;
        }
        self.evaluate(ts_us, DecisionReason::Load)
    }

    pub fn on_demand(&mut self, sample: DemandSample) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        let ts = sample.ts_us;
        if self.last_demand_us.is_some_and(|last| ts <= last) {
            return self.evaluate(ts, DecisionReason::Load);
        }
        let dt_us = self.filter_updated_us.map(|last| ts.saturating_sub(last));
        let tau_ms = self.config.control.demand_smooth_ms.max(20);
        let alpha = dt_us.map_or(1.0, |dt| {
            1.0 - (-(dt as f32) / (tau_ms as f32 * US_PER_MS as f32)).exp()
        });
        self.last_demand_us = Some(ts);
        self.filter_updated_us = Some(ts);

        let mut instant_fg = 0.0f32;
        let mut instant_busy = 0.0f32;
        for (index, cluster) in self.clusters.iter_mut().enumerate() {
            let busy = sample
                .cluster_busy
                .get(index)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let fg = sample
                .foreground_busy
                .get(index)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            cluster.busy_raw = busy;
            cluster.fg_raw = fg;
            if alpha >= 1.0 {
                cluster.busy_ema = busy;
                cluster.fg_ema = fg;
            } else {
                cluster.busy_ema += (busy - cluster.busy_ema) * alpha;
                cluster.fg_ema += (fg - cluster.fg_ema) * alpha;
            }
            if (busy - cluster.busy_ema).abs() <= 0.001 {
                cluster.busy_ema = busy;
            }
            if (fg - cluster.fg_ema).abs() <= 0.001 {
                cluster.fg_ema = fg;
            }
            instant_fg = instant_fg.max(fg);
            instant_busy = instant_busy.max(busy);
        }

        // Foreground detection can be unavailable; total load must still release quiet.
        let instant_demand = instant_fg.max(instant_busy);
        if self.quiet {
            if instant_demand >= self.config.control.floor_exit_ratio {
                self.quiet = false;
                self.low_demand_since_us = None;
            }
        } else if instant_demand <= self.config.control.floor_enter_ratio {
            self.low_demand_since_us.get_or_insert(ts);
        } else {
            self.low_demand_since_us = None;
        }

        self.evaluate(ts, DecisionReason::Load)
    }

    pub fn on_thermal_cap(&mut self, ts_us: u64, cap: Option<f32>) -> Vec<Decision> {
        if cap.is_some_and(f32::is_nan) {
            return Vec::new();
        }
        let cap = cap
            .filter(|&value| value < 1.0)
            .map(|value| value.max(0.10));
        if cap == self.thermal_cap {
            return Vec::new();
        }
        self.thermal_cap = cap;
        self.evaluate(ts_us, DecisionReason::Thermal)
    }

    pub fn on_deadline(&mut self, ts_us: u64) -> Vec<Decision> {
        self.evaluate(ts_us, DecisionReason::Load)
    }

    /// The next one-shot window, or `None` when the daemon may block forever.
    pub fn next_deadline(&self, now_us: u64) -> Option<Deadline> {
        if !self.screen_on {
            return None;
        }
        let mut best: Option<Deadline> = None;
        let mut consider = |at: u64, kind: DeadlineKind| {
            if at > now_us {
                best = match best {
                    Some(current) if current.at_us <= at => Some(current),
                    _ => Some(Deadline { at_us: at, kind }),
                };
            }
        };

        if self.demand_filter_pending() {
            if let Some(updated) = self.filter_updated_us {
                let interval = self.config.control.demand_smooth_ms.clamp(20, 100) * US_PER_MS;
                consider(
                    updated
                        .saturating_add(interval)
                        .max(now_us.saturating_add(1)),
                    DeadlineKind::Ramp,
                );
            }
        }

        for window in [
            self.lift_until_us,
            self.launch_until_us,
            self.burst_until_us,
            self.pressure_until_us,
        ] {
            if let Some(until) = window {
                if until > now_us {
                    consider(until, DeadlineKind::LiftEnd);
                }
            }
        }

        if !self.quiet && self.low_demand_since_us.is_some() {
            if let Some(since) = self.low_demand_since_us {
                let settle = since + self.config.control.floor_settle_ms * US_PER_MS;
                let decay = self.last_touch_us + self.config.events.quiet_decay_ms * US_PER_MS;
                consider(settle.max(decay), DeadlineKind::QuietEntry);
            }
        }

        for (index, cluster) in self.clusters.iter().enumerate() {
            let desired = self.desired_tier(index, now_us);
            if desired > cluster.tier {
                if let Some(since) = cluster.up_since_us {
                    consider(
                        since + self.config.control.up_confirm_ms * US_PER_MS,
                        DeadlineKind::Ramp,
                    );
                }
            } else if desired < cluster.tier && self.down_allowed(index, now_us) {
                consider(
                    cluster.last_step_us + self.config.control.down_step_ms * US_PER_MS,
                    DeadlineKind::Decay,
                );
            }
        }
        best
    }

    fn pressure_active(&self, ts_us: u64) -> bool {
        self.pressure_until_us.is_some_and(|until| ts_us < until)
            && ts_us.saturating_sub(self.last_over_budget_us)
                <= self.config.control.pressure_hold_ms * US_PER_MS
    }

    fn ratio_tier(&self, ratio: f32, count: usize) -> usize {
        if count <= 1 {
            return 0;
        }
        (ratio.clamp(0.0, 1.0) * (count - 1) as f32).round() as usize
    }

    fn scene_floor(&self, count: usize) -> usize {
        let configured = match self.scene {
            Scene::Reading => self.config.scene.reading_floor_tier,
            Scene::Video => self.config.scene.video_floor_tier,
            _ => self.config.scene.default_floor_tier,
        };
        configured
            .max(self.adaptive_floor)
            .min(count.saturating_sub(1))
    }

    fn down_allowed(&self, index: usize, ts_us: u64) -> bool {
        let cluster = &self.clusters[index];
        let desired = self.desired_tier(index, ts_us);
        // Upward confirmation and step-limited decay provide hysteresis without
        // pinning a lightly loaded cluster one full tier above its demand.
        desired < cluster.tier
    }

    fn launch_active(&self, ts_us: u64) -> bool {
        self.launch_until_us.is_some_and(|until| ts_us < until)
    }

    fn launch_target_tier(&self, count: usize) -> usize {
        let top = count - 1;
        // Keep the existing warm-start target below speculative top-tier boosts.
        let band = if top <= 1 {
            top
        } else {
            self.ratio_tier(
                (self.config.control.launch_tier_ratio + 0.20).min(1.0),
                count,
            )
            .clamp(1, top - 1)
        };
        self.ratio_tier(self.config.control.launch_tier_ratio, count)
            .min(band.saturating_sub(1))
    }

    fn thermal_cap_tier(&self, index: usize) -> usize {
        let ladder = &self.tiers[index];
        let Some(ratio) = self.thermal_cap else {
            return ladder.len() - 1;
        };
        // A ratio of the frequency span, rounded down to a real step, also
        // bounds sparse or uneven ladders without rounding up to a hot OPP.
        let min = f64::from(ladder[0]);
        let max = f64::from(ladder[ladder.len() - 1]);
        let limit = min + (max - min) * f64::from(ratio);
        ladder.partition_point(|&khz| f64::from(khz) <= limit) - 1
    }

    fn desired_tier(&self, index: usize, ts_us: u64) -> usize {
        self.target_tier(index, ts_us, true)
    }

    fn target_tier(&self, index: usize, ts_us: u64, include_load: bool) -> usize {
        let count = self.tiers[index].len();
        if count <= 1 {
            return 0;
        }
        if !self.screen_on {
            return 0;
        }
        let cluster = &self.clusters[index];
        let thermal_limit = self.thermal_cap_tier(index);
        let floor = self.scene_floor(count);
        if self.quiet {
            return floor.min(thermal_limit);
        }

        let demand = cluster.busy_ema.max(cluster.fg_ema).clamp(0.0, 1.0);
        // Leave low demand at the bottom instead of amplifying small background
        // activity with a concave curve. Sustained 75% demand still opens the
        // full ladder; touch, launch and frame pressure remain immediate paths.
        let load = ((demand - self.config.control.floor_enter_ratio)
            / (0.75 - self.config.control.floor_enter_ratio).max(0.01))
        .clamp(0.0, 1.0);
        let load_tier = if include_load {
            self.ratio_tier(load, count)
        } else {
            0
        };
        let mut tier = load_tier.max(floor);

        if self.burst_until_us.is_some_and(|until| ts_us < until) {
            tier = tier.max(self.ratio_tier(self.config.control.frame_burst_level, count));
        }
        if self.pressure_active(ts_us) {
            tier = count - 1;
        }
        if self.launch_active(ts_us) {
            tier = tier.max(self.launch_target_tier(count));
        }
        if self.lift_until_us.is_some_and(|until| ts_us < until)
            && self.profile.clusters[index].role != Role::Prime
        {
            tier = tier.max(cluster.touch_target);
        }
        if self.scene == Scene::Ime {
            tier = tier.min(self.ratio_tier(self.config.control.ime_cap_ratio, count));
        }
        if self.profile.clusters[index].role == Role::Prime && !self.pressure_active(ts_us) {
            // Limit speculative boosts, not measured CPU demand. Load-driven
            // climbs still pass through the confirmation window and step limit.
            let prime_cap = self.ratio_tier(self.config.control.prime_cap_ratio, count);
            tier = tier.min(prime_cap.max(load_tier));
        }
        tier.min(thermal_limit)
    }

    /// Display tier rounded down from the actual thermal/user upper bound.
    pub fn cap_tier(&self, index: usize, _ts_us: u64) -> usize {
        let cap = self.cap_khz(index);
        self.tiers[index]
            .partition_point(|&khz| khz <= cap)
            .saturating_sub(1)
    }

    fn check_quiet_entry(&mut self, ts_us: u64) {
        // 复用最近一次低负载观测，到期时无需等待新的采样事件。
        if self.screen_on
            && !self.quiet
            && self.low_demand_since_us.is_some_and(|since| {
                ts_us.saturating_sub(since) >= self.config.control.floor_settle_ms * US_PER_MS
            })
            && ts_us >= self.last_touch_us + self.config.events.quiet_decay_ms * US_PER_MS
            && self.lift_until_us.is_none_or(|until| ts_us >= until)
            && self.burst_until_us.is_none_or(|until| ts_us >= until)
            && self.launch_until_us.is_none_or(|until| ts_us >= until)
            && !self.pressure_active(ts_us)
        {
            self.quiet = true;
        }
    }

    fn demand_filter_pending(&self) -> bool {
        self.clusters
            .iter()
            .any(|cluster| cluster.busy_ema != cluster.busy_raw || cluster.fg_ema != cluster.fg_raw)
    }

    fn advance_demand_filter(&mut self, ts_us: u64) {
        let Some(last) = self.filter_updated_us else {
            return;
        };
        if ts_us <= last {
            return;
        }
        let tau_us = self.config.control.demand_smooth_ms.max(20) * US_PER_MS;
        let alpha = 1.0 - (-(ts_us.saturating_sub(last) as f32) / tau_us as f32).exp();
        for cluster in &mut self.clusters {
            for (ema, raw) in [
                (&mut cluster.busy_ema, cluster.busy_raw),
                (&mut cluster.fg_ema, cluster.fg_raw),
            ] {
                *ema += (raw - *ema) * alpha;
                if (raw - *ema).abs() <= 0.001 {
                    *ema = raw;
                }
            }
        }
        self.filter_updated_us = Some(ts_us);
    }

    fn evaluate(&mut self, ts_us: u64, base_reason: DecisionReason) -> Vec<Decision> {
        self.advance_demand_filter(ts_us);
        self.check_quiet_entry(ts_us);
        if self.clusters.is_empty() {
            return Vec::new();
        }
        for cluster in self.clusters.iter_mut() {
            if cluster.last_step_us == 0 || cluster.last_step_us > ts_us {
                cluster.last_step_us = ts_us;
            }
        }
        let mut decisions = Vec::with_capacity(self.clusters.len());
        for index in 0..self.clusters.len() {
            let count = self.tiers[index].len();
            let old_tier = self.clusters[index].tier;
            let old_cap = self.clusters[index].last_cap_khz;
            let desired = self.desired_tier(index, ts_us);
            let event = matches!(
                base_reason,
                DecisionReason::TouchLift
                    | DecisionReason::Launch
                    | DecisionReason::Foreground
                    | DecisionReason::FrameBurst
            );

            let mut new_tier = old_tier;
            let mut reason = base_reason;

            let thermal_limit = self.thermal_cap_tier(index);
            if !self.screen_on || old_tier > thermal_limit {
                new_tier = if self.screen_on { thermal_limit } else { 0 };
                self.clusters[index].up_since_us = None;
            } else if desired > old_tier {
                let immediate = if event {
                    let target = match base_reason {
                        DecisionReason::Launch | DecisionReason::Foreground => {
                            self.launch_target_tier(count)
                        }
                        DecisionReason::TouchLift
                            if self.profile.clusters[index].role != Role::Prime =>
                        {
                            self.clusters[index].touch_target
                        }
                        DecisionReason::FrameBurst if self.pressure_active(ts_us) => count - 1,
                        DecisionReason::FrameBurst => {
                            self.ratio_tier(self.config.control.frame_burst_level, count)
                        }
                        _ => 0,
                    };
                    target.min(self.target_tier(index, ts_us, false))
                } else {
                    0
                };
                if immediate > old_tier {
                    let step = if matches!(
                        base_reason,
                        DecisionReason::Launch | DecisionReason::Foreground
                    ) {
                        immediate - old_tier
                    } else {
                        self.config.control.max_up_step.max(1) as usize
                    };
                    new_tier = immediate.min(old_tier + step);
                    self.clusters[index].up_since_us = (desired > new_tier).then_some(ts_us);
                } else if self.quiet {
                    new_tier = desired;
                    reason = DecisionReason::Quiet;
                } else {
                    match self.clusters[index].up_since_us {
                        Some(since)
                            if ts_us.saturating_sub(since)
                                >= self.config.control.up_confirm_ms * US_PER_MS =>
                        {
                            let step = self.config.control.max_up_step.max(1) as usize;
                            new_tier = desired.min(old_tier + step);
                            self.clusters[index].up_since_us = Some(ts_us);
                            reason = DecisionReason::Load;
                        }
                        Some(_) => {
                            reason = DecisionReason::Unchanged;
                        }
                        None => {
                            self.clusters[index].up_since_us = Some(ts_us);
                            reason = DecisionReason::Unchanged;
                        }
                    }
                }
            } else if desired < old_tier {
                self.clusters[index].up_since_us = None;
                if self.down_allowed(index, ts_us)
                    && ts_us.saturating_sub(self.clusters[index].last_step_us)
                        >= self.config.control.down_step_ms * US_PER_MS
                {
                    new_tier = old_tier - 1;
                    reason = if self.quiet {
                        DecisionReason::Quiet
                    } else {
                        DecisionReason::Load
                    };
                } else if self.quiet {
                    reason = DecisionReason::Quiet;
                } else {
                    reason = DecisionReason::Unchanged;
                }
            } else {
                self.clusters[index].up_since_us = None;
            }

            if new_tier != old_tier {
                self.clusters[index].tier = new_tier;
                self.clusters[index].last_step_us = ts_us;
            }
            let cap_khz = self.cap_khz(index);
            self.clusters[index].last_cap_khz = cap_khz;
            let changed = new_tier != old_tier || cap_khz != old_cap;

            decisions.push(Decision {
                cluster: index,
                ceiling_khz: self.tiers[index][new_tier].min(cap_khz),
                cap_khz,
                tier: new_tier as u32,
                tier_count: count as u32,
                reason,
                changed,
                idle: self.quiet && new_tier == 0,
            });
        }
        decisions
    }
}

/// Reduce the hardware frequency list to a 6..10 entry ladder by picking
/// evenly spaced entries. Small clusters (fewer frequencies) use every step.
pub fn build_ladder(freqs: &[u32], min_khz: &u32, max_khz: &u32, max_tiers: usize) -> Vec<u32> {
    let mut sorted: Vec<u32> = freqs.iter().copied().filter(|khz| *khz > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        sorted = vec![*min_khz, *max_khz];
        sorted.sort_unstable();
        sorted.dedup();
    }
    let count = sorted.len();
    let limit = max_tiers.clamp(6, 10);
    if count <= limit {
        return sorted;
    }
    let mut ladder = Vec::with_capacity(limit);
    for slot in 0..limit {
        let index = slot * (count - 1) / (limit - 1);
        if ladder.last() != Some(&sorted[index]) {
            ladder.push(sorted[index]);
        }
    }
    ladder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::profile::{CapacitySource, ClusterProfile, DeviceProfile};

    fn profile() -> DeviceProfile {
        DeviceProfile {
            schema: 1,
            platform: "test".to_owned(),
            sdk: 34,
            clusters: vec![
                ClusterProfile {
                    cpus: vec![0, 1, 2, 3],
                    role: Role::Efficiency,
                    min_khz: 307_200,
                    max_khz: 2_016_000,
                    capacity: 256,
                    freq_khz: vec![
                        307_200, 441_600, 576_000, 691_200, 806_400, 940_800, 1_056_000, 1_171_200,
                        1_286_400, 1_401_600, 1_536_000, 1_708_800, 2_016_000,
                    ],
                    capacity_source: CapacitySource::Sysfs,
                },
                ClusterProfile {
                    cpus: vec![4, 5, 6],
                    role: Role::Balanced,
                    min_khz: 499_200,
                    max_khz: 2_803_200,
                    capacity: 768,
                    freq_khz: vec![
                        499_200, 633_600, 768_000, 902_400, 1_036_800, 1_171_200, 1_305_600,
                        1_440_000, 1_574_400, 1_708_800, 1_881_600, 2_227_200, 2_496_000,
                        2_649_600, 2_803_200,
                    ],
                    capacity_source: CapacitySource::Sysfs,
                },
                ClusterProfile {
                    cpus: vec![7],
                    role: Role::Prime,
                    min_khz: 595_200,
                    max_khz: 3_360_000,
                    capacity: 1024,
                    freq_khz: vec![
                        595_200, 729_600, 864_000, 998_400, 1_132_800, 1_267_200, 1_401_600,
                        1_536_000, 1_670_400, 1_804_800, 1_977_600, 2_150_400, 2_323_200,
                        2_496_000, 2_841_600, 3_072_000, 3_360_000,
                    ],
                    capacity_source: CapacitySource::Sysfs,
                },
            ],
        }
    }

    fn controller() -> Controller {
        Controller::new(profile(), Config::default())
    }

    fn demand(ts_ms: u64, busy: f32) -> DemandSample {
        DemandSample {
            ts_us: ts_ms * US_PER_MS,
            cluster_busy: vec![busy; 3],
            foreground_busy: vec![busy; 3],
            foreground_active: busy > 0.0,
        }
    }

    #[test]
    fn ladder_has_six_to_ten_steps_and_ends_at_hw_max() {
        let c = controller();
        for index in 0..3 {
            let table = c.tier_table(index);
            assert!(
                (6..=10).contains(&table.len()),
                "cluster {index} has {} tiers",
                table.len()
            );
            assert_eq!(table[0], c.profile.clusters[index].min_khz);
            assert_eq!(*table.last().unwrap(), c.profile.clusters[index].max_khz);
        }
    }

    #[test]
    fn upward_move_is_confirmed_and_step_limited() {
        let mut c = controller();
        let first = c.on_demand(demand(100, 0.95));
        assert_eq!(first[0].tier, 0, "must wait for the confirm window");

        let after = c.on_demand(demand(260, 0.95));
        for decision in &after {
            assert!(decision.changed);
            assert!(
                decision.tier <= 2,
                "max_up_step limits the first climb: {:?}",
                decision
            );
            assert!(
                decision.tier < decision.tier_count - 1,
                "never jumps to the top"
            );
        }
    }

    #[test]
    fn downward_move_is_rate_limited_to_one_tier() {
        let mut c = controller();
        // Climb first.
        c.on_demand(demand(100, 0.9));
        c.on_demand(demand(300, 0.9));
        c.on_demand(demand(500, 0.9));
        c.on_demand(demand(700, 0.9));
        let high = c.tier(1);
        assert!(high >= 3);

        // Drop demand; the controller must not collapse in one sample.
        let step = c.on_demand(demand(900, 0.0));
        let tier = step[1].tier as usize;
        assert_eq!(tier, high - 1, "one tier per down_step_ms");
    }

    #[test]
    fn video_scene_follows_load_without_frame_feedback() {
        let mut c = controller();
        c.on_scene(1000, Scene::Video);
        for ms in 1..10 {
            c.on_demand(demand(1000 + ms * 100, 0.45));
        }
        for index in 0..3 {
            assert!(c.tier(index) > 0, "视频没有帧反馈时仍应响应负载");
            assert!(c.ceiling_khz(index) > c.profile.clusters[index].min_khz);
        }
        assert!(c.cap_khz(0) > c.ceiling_khz(0), "cap keeps headroom");
    }

    #[test]
    fn ime_caps_the_ladder_and_never_lifts_prime_on_touch() {
        let mut c = controller();
        c.on_scene(100, Scene::Ime);
        for ms in 1..12 {
            c.on_demand(demand(100 + ms * 100, 0.9));
            c.on_touch(100 + ms * 100);
        }
        for index in 0..3 {
            let count = c.tier_count(index);
            let cap = (c.config().control.ime_cap_ratio * (count - 1) as f32).round() as usize;
            assert!(c.tier(index) <= cap, "IME stays at or below mid tier");
        }
    }

    #[test]
    fn cooldown_removes_the_thermal_cap_without_removing_user_cap() {
        let profile = profile();
        let mut config = Config::default();
        config.control.cluster_cap_ratio.insert("cpu0".into(), 0.4);
        let mut controller = Controller::new(profile.clone(), config);
        let user_cap = controller.cap_khz(0);
        controller.on_thermal_cap(1, Some(0.10));
        assert!(controller.cap_khz(0) < user_cap);
        controller.on_thermal_cap(2, Some(1.0));
        assert!(controller.thermal_cap.is_none());
        assert_eq!(controller.cap_khz(0), user_cap);
        assert_eq!(controller.cap_khz(1), profile.clusters[1].max_khz);
    }

    #[test]
    fn screen_off_returns_to_the_hardware_minimum_and_opens_the_cap() {
        let mut c = controller();
        c.on_demand(demand(100, 0.8));
        c.on_demand(demand(300, 0.8));
        let off = c.on_screen(500 * US_PER_MS, false);
        assert!(off.iter().any(|d| d.changed));
        assert!(c.cap_khz(2) == c.profile.clusters[2].max_khz);
        assert!(off[0].ceiling_khz <= c.profile.clusters[0].freq_khz[2]);
    }

    #[test]
    fn quiet_path_latches_after_settle_and_has_no_deadline() {
        let mut c = controller();
        c.on_screen(0, true);
        let mut ts = 1200;
        for _ in 0..8 {
            ts += 200;
            c.on_demand(demand(ts, 0.0));
        }
        assert!(c.is_quiet());
        assert!(c.next_deadline(ts * US_PER_MS).is_none());
    }

    #[test]
    fn page_turn_touch_lifts_one_tier_only() {
        let mut c = controller();
        c.on_scene(0, Scene::Reading);
        for ms in 1..10 {
            c.on_demand(demand(ms * 200, 0.0));
        }
        let lifted = c.on_touch(2_100 * US_PER_MS);
        assert!(lifted.iter().any(|d| d.changed));
        for index in 0..3 {
            assert!(c.tier(index) <= 1, "touch adds one tier, not a jump");
        }
    }
}
