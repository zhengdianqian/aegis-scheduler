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
//! * the writer receives a matching upper cap a few tiers above the target so a
//!   vendor boost cannot slam the cluster to its top step while demand is low.
//!
//! The controller is a pure state machine: explicit monotonic timestamps in,
//! decisions out. It never reads sysfs and installs no repeating timers.

use serde::Serialize;

use crate::config::Config;
use crate::profile::{DeviceProfile, Role};

const US_PER_MS: u64 = 1_000;

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
    /// Operating floor (a tier frequency). Kept under the historical name so
    /// older WebUI builds keep rendering; v3 labels it as "目标".
    pub ceiling_khz: u32,
    /// Upper cap for `scaling_max_freq`; `ceiling_khz <= cap_khz <= hw max`.
    pub cap_khz: u32,
    pub tier: u32,
    pub tier_count: u32,
    pub reason: DecisionReason,
    pub changed: bool,
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
    cap_tier: usize,
    busy_ema: f32,
    fg_ema: f32,
    up_since_us: Option<u64>,
    last_step_us: u64,
}

impl ClusterState {
    fn new() -> Self {
        Self {
            tier: 0,
            cap_tier: usize::MAX,
            busy_ema: 0.0,
            fg_ema: 0.0,
            up_since_us: None,
            last_step_us: 0,
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
    low_demand_since_us: Option<u64>,
    lift_until_us: Option<u64>,
    launch_until_us: Option<u64>,
    burst_until_us: Option<u64>,
    pressure_until_us: Option<u64>,
    last_over_budget_us: u64,
    over_budget_hits: u32,
    last_demand_us: u64,
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
            .map(|cluster| build_ladder(&cluster.freq_khz, &cluster.min_khz, &cluster.max_khz,
                                          config.control.tier_table_max))
            .collect();
        let clusters = profile.clusters.iter().map(|_| ClusterState::new()).collect();
        Self {
            profile,
            config,
            tiers,
            clusters,
            screen_on: true,
            quiet: false,
            last_touch_us: 0,
            low_demand_since_us: None,
            lift_until_us: None,
            launch_until_us: None,
            burst_until_us: None,
            pressure_until_us: None,
            last_over_budget_us: 0,
            over_budget_hits: 0,
            last_demand_us: 0,
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
        self.tiers[index][cluster.tier.min(self.tiers[index].len() - 1)]
    }

    pub fn cap_khz(&self, index: usize) -> u32 {
        let cluster = &self.clusters[index];
        let cap = cluster.cap_tier.min(self.tiers[index].len() - 1);
        self.tiers[index][cap]
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
            // A screen-on is a warm start: open the cap and hold a mid tier for
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
            self.launch_until_us = None;
            self.burst_until_us = None;
            self.pressure_until_us = None;
            self.over_budget_hits = 0;
            for cluster in self.clusters.iter_mut() {
                cluster.up_since_us = None;
            }
            self.evaluate(ts_us, DecisionReason::ScreenOff)
        }
    }

    pub fn on_touch(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
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
        if !over || !sample.foreground {
            return Vec::new();
        }

        let recent = sample.ts_us.saturating_sub(self.last_over_budget_us) <= US_PER_MS;
        self.over_budget_hits = if recent { self.over_budget_hits.saturating_add(1) } else { 1 };
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

        // Video and reading are calibrated at their floor. One dropped frame is
        // the signal that the floor is too low for this stream, so raise it one
        // tier for the rest of the session (capped, reversible on restart).
        if matches!(self.scene, Scene::Video | Scene::Reading) {
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
        let dt_us = ts.saturating_sub(self.last_demand_us);
        let tau_ms = self.config.control.demand_smooth_ms.max(20);
        let alpha = if self.last_demand_us == 0 || dt_us == 0 {
            1.0
        } else {
            (1.0 - (-(dt_us as f32) / (tau_ms as f32 * US_PER_MS as f32)).exp()).clamp(0.08, 1.0)
        };
        self.last_demand_us = ts;

        let mut instant_fg = 0.0f32;
        for (index, cluster) in self.clusters.iter_mut().enumerate() {
            let busy = sample.cluster_busy.get(index).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let fg = sample
                .foreground_busy
                .get(index)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            if alpha >= 1.0 {
                cluster.busy_ema = busy;
                cluster.fg_ema = fg;
            } else {
                cluster.busy_ema += (busy - cluster.busy_ema) * alpha;
                cluster.fg_ema += (fg - cluster.fg_ema) * alpha;
            }
            instant_fg = instant_fg.max(fg);
        }

        // Quiet path: no touch, no window and a settled low foreground demand.
        if self.quiet {
            if instant_fg >= self.config.control.floor_exit_ratio {
                self.quiet = false;
                self.low_demand_since_us = None;
            }
        } else if instant_fg <= self.config.control.floor_enter_ratio {
            match self.low_demand_since_us {
                None => self.low_demand_since_us = Some(ts),
                Some(since) => {
                    if ts.saturating_sub(since) >= self.config.control.floor_settle_ms * US_PER_MS
                        && ts >= self.last_touch_us + self.config.events.quiet_decay_ms * US_PER_MS
                        && self.lift_until_us.is_none_or(|until| ts >= until)
                        && self.burst_until_us.is_none_or(|until| ts >= until)
                        && self.launch_until_us.is_none_or(|until| ts >= until)
                        && !self.pressure_active(ts)
                    {
                        self.quiet = true;
                    }
                }
            }
        } else {
            self.low_demand_since_us = None;
        }

        self.evaluate(ts, DecisionReason::Load)
    }

    pub fn on_thermal_cap(&mut self, ts_us: u64, cap: Option<f32>) -> Vec<Decision> {
        let cap = cap.map(|value| value.clamp(0.10, 1.0));
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

        for window in [self.lift_until_us, self.launch_until_us, self.burst_until_us, self.pressure_until_us] {
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
            } else if desired < cluster.tier && self.down_allowed(index) {
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

    fn down_allowed(&self, index: usize) -> bool {
        let cluster = &self.clusters[index];
        let desired = self.desired_tier(index, self.last_demand_us);
        desired < cluster.tier
            && (self.quiet
                || cluster.tier >= desired + 2
                || cluster.tier == 1)
    }

    fn desired_tier(&self, index: usize, ts_us: u64) -> usize {
        let count = self.tiers[index].len();
        if count <= 1 {
            return 0;
        }
        if !self.screen_on {
            return 0;
        }
        let cluster = &self.clusters[index];
        let floor = self.scene_floor(count);
        if self.quiet {
            return floor;
        }

        let demand = cluster.busy_ema.max(cluster.fg_ema).clamp(0.0, 1.0);
        // Concave curve: 3% demand -> tier 1/7, 15% -> 2/7, 40% -> 4/7.
        let mut tier = self.ratio_tier(demand.powf(0.6), count).max(floor);

        // Video decode is mostly off-CPU; hold the calibrated floor unless a
        // frame actually misses, which is handled by the burst/pressure paths.
        if self.scene == Scene::Video && !self.pressure_active(ts_us) {
            tier = floor;
        }

        if self.burst_until_us.is_some_and(|until| ts_us < until) {
            tier = tier.max(self.ratio_tier(self.config.control.frame_burst_level, count));
        }
        if self.pressure_active(ts_us) {
            tier = count - 1;
        }
        if self.launch_until_us.is_some_and(|until| ts_us < until) {
            tier = tier.max(self.ratio_tier(self.config.control.launch_tier_ratio, count));
        }
        if self.lift_until_us.is_some_and(|until| ts_us < until)
            && self.profile.clusters[index].role != Role::Prime
        {
            let steps = self.config.control.touch_lift_tiers.max(1) as usize;
            tier = tier.max((cluster.tier + steps).min(count - 1));
        }
        if self.scene == Scene::Ime {
            tier = tier.min(self.ratio_tier(self.config.control.ime_cap_ratio, count));
        }
        if self.profile.clusters[index].role == Role::Prime && !self.pressure_active(ts_us) {
            tier = tier.min(self.ratio_tier(self.config.control.prime_cap_ratio, count));
        }
        if let Some(cap) = self.thermal_cap {
            tier = tier.min(self.ratio_tier(cap, count));
        }
        tier.min(count - 1)
    }

    /// Upper cap for the governor. It is always at least `safety_ceiling_ratio`
    /// of the ladder, and fully open during launch, frame bursts and pressure.
    pub fn cap_tier(&self, index: usize, ts_us: u64) -> usize {
        let count = self.tiers[index].len();
        if count <= 1 || !self.screen_on {
            return count.saturating_sub(1);
        }
        if self.pressure_active(ts_us)
            || self.launch_until_us.is_some_and(|until| ts_us < until)
            || self.burst_until_us.is_some_and(|until| ts_us < until)
        {
            return count - 1;
        }
        let safety = ((count - 1) as f32 * self.config.control.safety_ceiling_ratio).ceil() as usize;
        if self.quiet {
            return safety.min(count - 1);
        }
        let lookahead = self.config.control.lookahead_tiers as usize;
        (self.clusters[index].tier + lookahead)
            .max(safety)
            .min(count - 1)
    }

    fn evaluate(&mut self, ts_us: u64, base_reason: DecisionReason) -> Vec<Decision> {
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
            let old_cap = self.clusters[index].cap_tier;
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

            if desired > old_tier {
                if event {
                    // Touch adds one tier, launch jumps to its own mid tier
                    // (never the top), frame bursts climb by at most two so the
                    // ladder stays visible instead of slamming to the ceiling.
                    let step = if base_reason == DecisionReason::Launch {
                        count - 1
                    } else {
                        self.config.control.max_up_step.max(1) as usize
                    };
                    new_tier = desired.min(old_tier + step.max(1));
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
                if self.down_allowed(index)
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
            let cap_tier = self.cap_tier(index, ts_us);
            self.clusters[index].cap_tier = cap_tier;
            let changed = new_tier != old_tier || cap_tier != old_cap;

            decisions.push(Decision {
                cluster: index,
                ceiling_khz: self.tiers[index][new_tier],
                cap_khz: self.tiers[index][cap_tier],
                tier: new_tier as u32,
                tier_count: count as u32,
                reason,
                changed,
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
                        307_200, 441_600, 576_000, 691_200, 806_400, 940_800, 1_056_000,
                        1_171_200, 1_286_400, 1_401_600, 1_536_000, 1_708_800, 2_016_000,
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
            assert!((6..=10).contains(&table.len()), "cluster {index} has {} tiers", table.len());
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
            assert!(decision.tier <= 2, "max_up_step limits the first climb: {:?}", decision);
            assert!(decision.tier < decision.tier_count - 1, "never jumps to the top");
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
    fn video_scene_holds_the_floor_instead_of_chasing_busy() {
        let mut c = controller();
        c.on_scene(1000, Scene::Video);
        for ms in 1..10 {
            c.on_demand(demand(1000 + ms * 100, 0.45));
        }
        for index in 0..3 {
            assert_eq!(c.tier(index), 0, "video holds the calibrated floor");
            assert_eq!(c.ceiling_khz(index), c.profile.clusters[index].min_khz);
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
