// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Single-mode adaptive controller with an event-driven Quiet Path.
//!
//! The controller is a pure state machine: every method receives an explicit
//! monotonic timestamp and returns decisions. It never reads sysfs and never
//! installs repeating timers. `next_deadline` exposes only one-shot windows;
//! when it returns `None` the caller may block indefinitely.

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub cluster: usize,
    pub ceiling_khz: u32,
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
    /// Next stage of a staged lift (or the next decay step).
    Ramp,
    /// Next downward step while the controller walks back to the target.
    Decay,
}

/// A staged lift: the ceiling climbs from 1/N to N/N of `peak` in `N` steps
/// spaced `step_ms` apart. Time is derived from the monotonic timestamp, so no
/// extra state is needed beyond the window start.
#[derive(Debug, Clone, Copy)]
struct Ramp {
    start_us: u64,
    peak: f32,
    stages: u32,
    step_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub at_us: u64,
    pub kind: DeadlineKind,
}

#[derive(Debug, Clone)]
struct ClusterRuntime {
    ceiling_khz: u32,
    last_change_us: u64,
    busy: f32,
    foreground_busy: f32,
}

pub struct Controller {
    profile: DeviceProfile,
    config: Config,
    clusters: Vec<ClusterRuntime>,
    screen_on: bool,
    screen_since_us: u64,
    last_touch_us: Option<u64>,
    last_frame_us: Option<u64>,
    last_demand_us: Option<u64>,
    low_demand_since_us: Option<u64>,
    /// Latched "foreground has no effective work" state. Entered on a
    /// sustained low instantaneous ratio, left only on a sustained fast-filtered
    /// rise or on an input/frame/foreground event. This separates static-page
    /// micro-motion from a real burst.
    floor_low: bool,
    /// First sample of the current above-exit streak while `floor_low` is set.
    floor_exit_since_us: Option<u64>,
    /// Fast foreground filter (per cluster). Used only to confirm a floor exit,
    /// so a single noisy sample cannot yank the device out of the floor path.
    fg_fast: Vec<f32>,
    fg_filter_ts_us: Option<u64>,
    lift_until_us: Option<u64>,
    lift_all_roles: bool,
    lift_ramp: Option<Ramp>,
    burst_until_us: Option<u64>,
    burst_ramp: Option<Ramp>,
    /// When the next downward step may happen. Downward moves are rate-limited
    /// so a lift is released stage by stage instead of slamming back to min.
    decay_at_us: Option<u64>,
    quiet: bool,
    demands_seen: bool,
    thermal_cap: Option<f32>,
}

impl Controller {
    pub fn new(profile: DeviceProfile, config: Config) -> Self {
        let clusters = profile
            .clusters
            .iter()
            .map(|c| ClusterRuntime {
                ceiling_khz: c.max_khz,
                last_change_us: 0,
                busy: 0.0,
                foreground_busy: 0.0,
            })
            .collect();
        let fg_fast = vec![0.0; profile.clusters.len()];
        Self {
            profile,
            config,
            clusters,
            fg_fast,
            fg_filter_ts_us: None,
            floor_low: false,
            floor_exit_since_us: None,
            screen_on: false,
            screen_since_us: 0,
            last_touch_us: None,
            last_frame_us: None,
            last_demand_us: None,
            low_demand_since_us: None,
            lift_until_us: None,
            lift_all_roles: false,
            lift_ramp: None,
            burst_until_us: None,
            burst_ramp: None,
            decay_at_us: None,
            quiet: false,
            demands_seen: false,
            thermal_cap: None,
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
        self.clusters[index].ceiling_khz
    }

    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub fn is_screen_on(&self) -> bool {
        self.screen_on
    }

    /// Screen on/off. Screen off floors every cluster immediately and leaves no
    /// pending deadline; screen on opens a single one-shot lift window.
    pub fn on_screen(&mut self, ts_us: u64, on: bool) -> Vec<Decision> {
        self.screen_on = on;
        self.screen_since_us = ts_us;
        self.last_touch_us = None;
        self.last_frame_us = None;
        self.last_demand_us = None;
        self.low_demand_since_us = None;
        self.lift_until_us = None;
        self.lift_ramp = None;
        self.burst_until_us = None;
        self.burst_ramp = None;
        self.decay_at_us = None;
        self.demands_seen = false;
        self.reset_floor();

        if on {
            self.quiet = false;
            self.lift_all_roles = true;
            self.lift_until_us =
                Some(ts_us.saturating_add(self.config.control.touch_lift_ms * US_PER_MS));
            self.lift_ramp = Some(self.new_ramp(ts_us, self.config.control.touch_lift_level));
            self.evaluate(ts_us, DecisionReason::Foreground)
        } else {
            self.quiet = true;
            self.evaluate(ts_us, DecisionReason::ScreenOff)
        }
    }

    /// A new touch contact (down or move). Refreshes the one-shot lift window.
    pub fn on_touch(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        // A new contact restarts the ramp; a move inside the same contact
        // continues it, so a swipe does not fall back to the first stage every
        // few milliseconds.
        let fresh = self
            .lift_until_us
            .map(|until| ts_us >= until)
            .unwrap_or(true);
        self.last_touch_us = Some(ts_us);
        self.quiet = false;
        // A contact lifts the interactive clusters immediately, but it does not
        // by itself prove real load: the floor latch stays set until fresh
        // demand samples (or a frame overrun) confirm a burst. This keeps the
        // prime cluster at min for a lone tap and avoids an over-reaction.
        self.floor_exit_since_us = None;
        self.lift_all_roles = false;
        if fresh {
            self.lift_ramp = Some(self.new_ramp(ts_us, self.config.control.touch_lift_level));
        }
        self.lift_until_us =
            Some(ts_us.saturating_add(self.config.control.touch_lift_ms * US_PER_MS));
        self.evaluate(ts_us, DecisionReason::TouchLift)
    }

    pub fn on_touch_up(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        self.last_touch_us = Some(ts_us);
        self.evaluate(ts_us, DecisionReason::Load)
    }

    /// A frame produced by the foreground application. Only over-budget frames
    /// affect decisions; on-time frames are informational.
    pub fn on_frame(&mut self, sample: FrameSample) -> Vec<Decision> {
        if !self.screen_on || !sample.foreground {
            return Vec::new();
        }
        self.last_frame_us = Some(sample.ts_us);
        let limit = sample.budget_ms + self.config.control.frame_slack_ms;
        if sample.render_ms > limit {
            self.quiet = false;
            self.low_demand_since_us = None;
            self.floor_low = false;
            self.floor_exit_since_us = None;
            let fresh_burst = self
                .burst_until_us
                .map(|until| sample.ts_us >= until)
                .unwrap_or(true);
            if fresh_burst {
                self.burst_ramp =
                    Some(self.new_ramp(sample.ts_us, self.config.control.frame_burst_level));
            }
            self.burst_until_us = Some(
                sample
                    .ts_us
                    .saturating_add(self.config.control.frame_burst_ms * US_PER_MS),
            );
            self.evaluate(sample.ts_us, DecisionReason::FrameBurst)
        } else {
            Vec::new()
        }
    }

    /// Foreground application changed. Old demand is discarded; a one-shot
    /// warm-start lift covers the transition.
    pub fn on_foreground_changed(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        self.quiet = false;
        self.demands_seen = false;
        self.low_demand_since_us = None;
        self.last_frame_us = None;
        self.decay_at_us = None;
        self.burst_until_us = None;
        self.burst_ramp = None;
        self.reset_floor();
        for cluster in &mut self.clusters {
            cluster.busy = 0.0;
            cluster.foreground_busy = 0.0;
        }
        self.lift_all_roles = true;
        self.lift_until_us =
            Some(ts_us.saturating_add(self.config.control.touch_lift_ms * US_PER_MS));
        self.lift_ramp = Some(self.new_ramp(ts_us, self.config.control.touch_lift_level));
        self.evaluate(ts_us, DecisionReason::Foreground)
    }

    /// An application process became active after an idle period. Unlike a
    /// touch, this is the real cold/warm start edge: the app's first frames and
    /// dex/IO work land inside this window. Lift every cluster (prime included)
    /// to the launch level for a few hundred milliseconds, then decay normally.
    pub fn on_app_launch(&mut self, ts_us: u64) -> Vec<Decision> {
        if !self.screen_on {
            return Vec::new();
        }
        self.quiet = false;
        self.demands_seen = false;
        self.low_demand_since_us = None;
        self.last_frame_us = None;
        self.decay_at_us = None;
        self.burst_until_us = None;
        self.burst_ramp = None;
        self.reset_floor();
        for cluster in &mut self.clusters {
            cluster.busy = 0.0;
            cluster.foreground_busy = 0.0;
        }
        self.lift_all_roles = true;
        self.lift_until_us =
            Some(ts_us.saturating_add(self.config.control.launch_lift_ms * US_PER_MS));
        self.lift_ramp = Some(Ramp {
            start_us: ts_us,
            peak: self.config.control.launch_lift_level,
            // Launch states cannot wait for a staged ramp: the first frames
            // arrive inside the first 16-60 ms.
            stages: 1,
            step_ms: 1,
        });
        self.evaluate(ts_us, DecisionReason::Launch)
    }

    /// Demand sample produced by the runtime probe. Low foreground demand lets
    /// the controller enter the Quiet Path even when background work continues.
    pub fn on_demand(&mut self, sample: DemandSample) -> Vec<Decision> {
        let ts = sample.ts_us;
        self.last_demand_us = Some(ts);
        for (index, cluster) in self.clusters.iter_mut().enumerate() {
            cluster.busy = sample
                .cluster_busy
                .get(index)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            cluster.foreground_busy = sample
                .foreground_busy
                .get(index)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
        }
        self.demands_seen = true;
        self.update_floor_state(ts);

        self.evaluate(ts, DecisionReason::Load)
    }

    /// Updates the foreground floor latch. Entry uses the instantaneous ratio
    /// (fast response on a quiet page); exit requires the fast-filtered ratio
    /// to stay above `floor_exit_ratio` for `floor_exit_ms`, so a single noisy
    /// sample or a rendering blip cannot pull the device out of the floor.
    fn update_floor_state(&mut self, ts_us: u64) {
        let enter = self.config.control.floor_enter_ratio;
        let exit = self.config.control.floor_exit_ratio;
        let settle = self.config.control.floor_settle_ms * US_PER_MS;
        let exit_hold = self.config.control.floor_exit_ms * US_PER_MS;

        // The probe publishes on change, not on a clock, so the filter uses the
        // actual sample gap: a long gap decays the old value completely.
        let dt = self
            .fg_filter_ts_us
            .map(|previous| ts_us.saturating_sub(previous))
            .unwrap_or(0);
        self.fg_filter_ts_us = Some(ts_us);
        let tau = self.config.control.fg_fast_ms.max(1) * US_PER_MS;
        let alpha = if dt == 0 {
            1.0
        } else {
            (1.0 - (-(dt as f32) / tau as f32).exp()).clamp(0.0, 1.0)
        };
        for index in 0..self.clusters.len() {
            let value = self.clusters[index].foreground_busy;
            let filtered = self.fg_fast.get(index).copied().unwrap_or(0.0);
            if let Some(slot) = self.fg_fast.get_mut(index) {
                *slot = filtered + alpha * (value - filtered);
            }
        }

        let max_instant = self
            .clusters
            .iter()
            .map(|cluster| cluster.foreground_busy)
            .fold(0.0_f32, f32::max);
        let max_fast = self.fg_fast.iter().copied().fold(0.0_f32, f32::max);

        if self.floor_low {
            if max_fast >= exit {
                let since = *self.floor_exit_since_us.get_or_insert(ts_us);
                if ts_us >= since.saturating_add(exit_hold) {
                    self.floor_low = false;
                    self.floor_exit_since_us = None;
                    self.low_demand_since_us = None;
                    self.quiet = false;
                }
            } else {
                self.floor_exit_since_us = None;
            }
        } else if max_instant <= enter {
            if self.low_demand_since_us.is_none() {
                self.low_demand_since_us = Some(ts_us);
            }
            if self
                .low_demand_since_us
                .is_some_and(|start| ts_us >= start.saturating_add(settle))
            {
                self.floor_low = true;
            }
        } else {
            self.low_demand_since_us = None;
            if self.quiet {
                self.quiet = false;
            }
        }
    }

    /// Clears every floor-related filter and latch. Used on screen and
    /// foreground transitions, where past demand is meaningless.
    fn reset_floor(&mut self) {
        self.floor_low = false;
        self.floor_exit_since_us = None;
        self.low_demand_since_us = None;
        self.fg_filter_ts_us = None;
        for slot in &mut self.fg_fast {
            *slot = 0.0;
        }
    }

    fn new_ramp(&self, start_us: u64, peak: f32) -> Ramp {
        let stages = self.config.control.touch_ramp_stages.max(2) as u32;
        let step_ms = (self.config.control.touch_ramp_ms / stages as u64).max(1);
        Ramp {
            start_us,
            peak,
            stages,
            step_ms,
        }
    }

    /// Level of a staged lift at `ts_us`: 1/N of the peak on arrival, then one
    /// step every `step_ms`, reaching the peak after `touch_ramp_ms`.
    fn ramp_level(ramp: Option<Ramp>, ts_us: u64) -> Option<f32> {
        let ramp = ramp?;
        let step_us = ramp.step_ms.max(1) * US_PER_MS;
        let elapsed = ts_us.saturating_sub(ramp.start_us);
        let stage = (1 + elapsed / step_us).min(ramp.stages as u64) as f32;
        Some(ramp.peak * stage / ramp.stages as f32)
    }

    /// First monotonic instant at which a ramp is still above its start and
    /// has another stage to climb.
    fn ramp_next(ramp: Option<Ramp>, now_us: u64) -> Option<u64> {
        let ramp = ramp?;
        let step_us = ramp.step_ms.max(1) * US_PER_MS;
        let elapsed = now_us.saturating_sub(ramp.start_us);
        let stage = 1 + elapsed / step_us;
        if stage >= ramp.stages as u64 {
            return None;
        }
        Some(ramp.start_us + stage * step_us)
    }

    pub fn on_thermal_cap(&mut self, ts_us: u64, cap: Option<f32>) -> Vec<Decision> {
        self.thermal_cap = cap.map(|value| value.clamp(0.05, 1.0));
        self.evaluate(ts_us, DecisionReason::Thermal)
    }

    /// A one-shot deadline expired.
    pub fn on_deadline(&mut self, ts_us: u64) -> Vec<Decision> {
        if self.lift_until_us.is_some_and(|until| ts_us >= until) {
            self.lift_until_us = None;
            self.lift_ramp = None;
        }
        if self.burst_until_us.is_some_and(|until| ts_us >= until) {
            self.burst_until_us = None;
            self.burst_ramp = None;
        }
        if self.decay_at_us.is_some_and(|at| ts_us >= at) {
            self.decay_at_us = None;
        }
        if !self.screen_on {
            return Vec::new();
        }
        if self.floor_low {
            if let Some(since) = self.floor_exit_since_us {
                if ts_us >= since.saturating_add(self.config.control.floor_exit_ms * US_PER_MS) {
                    // The filtered ratio said "burst", but by the confirmation
                    // moment the latest observation is already below the exit
                    // threshold: this was one noisy sample, not a real burst.
                    let instant_max = self
                        .clusters
                        .iter()
                        .map(|cluster| cluster.foreground_busy)
                        .fold(0.0_f32, f32::max);
                    if instant_max < self.config.control.floor_exit_ratio {
                        self.floor_exit_since_us = None;
                    } else {
                        self.floor_low = false;
                        self.floor_exit_since_us = None;
                        self.low_demand_since_us = None;
                        self.quiet = false;
                    }
                }
            }
        }
        self.evaluate(ts_us, DecisionReason::Load)
    }

    /// The next one-shot window, or `None` when the daemon may block forever.
    pub fn next_deadline(&self, now_us: u64) -> Option<Deadline> {
        if !self.screen_on {
            return None;
        }
        let mut best: Option<Deadline> = None;

        // Staged lifts: one wakeup per climb step. These come before the lift
        // end because they are always earlier.
        if self.lift_until_us.is_some_and(|until| now_us < until) {
            if let Some(at) = Self::ramp_next(self.lift_ramp, now_us) {
                if at > now_us {
                    best = Some(Deadline {
                        at_us: at,
                        kind: DeadlineKind::Ramp,
                    });
                }
            }
        }
        if self.burst_until_us.is_some_and(|until| now_us < until) {
            if let Some(at) = Self::ramp_next(self.burst_ramp, now_us) {
                if at > now_us {
                    let candidate = Deadline {
                        at_us: at,
                        kind: DeadlineKind::Ramp,
                    };
                    best = match best {
                        Some(current) if current.at_us <= candidate.at_us => Some(current),
                        _ => Some(candidate),
                    };
                }
            }
        }
        // Downward steps: keep walking to the target one stage at a time. This
        // is also the only wakeup that can exist while the quiet path is latched.
        if let Some(at) = self.decay_at_us {
            if at > now_us {
                let candidate = Deadline {
                    at_us: at,
                    kind: DeadlineKind::Decay,
                };
                best = match best {
                    Some(current) if current.at_us <= candidate.at_us => Some(current),
                    _ => Some(candidate),
                };
            }
        }

        if let Some(until) = self.lift_until_us {
            if until > now_us {
                let candidate = Deadline {
                    at_us: until,
                    kind: DeadlineKind::LiftEnd,
                };
                best = match best {
                    Some(current) if current.at_us <= candidate.at_us => Some(current),
                    _ => Some(candidate),
                };
            }
        }
        if let Some(until) = self.burst_until_us {
            if until > now_us {
                let candidate = Deadline {
                    at_us: until,
                    kind: DeadlineKind::BurstEnd,
                };
                best = match best {
                    Some(current) if current.at_us <= candidate.at_us => Some(current),
                    _ => Some(candidate),
                };
            }
        }

        let lift_active = self.lift_until_us.is_some_and(|until| now_us < until);
        let burst_active = self.burst_until_us.is_some_and(|until| now_us < until);
        if self.floor_low {
            if let Some(since) = self.floor_exit_since_us {
                let at = since.saturating_add(self.config.control.floor_exit_ms * US_PER_MS);
                if at > now_us {
                    let candidate = Deadline {
                        at_us: at,
                        kind: DeadlineKind::FloorExit,
                    };
                    best = match best {
                        Some(current) if current.at_us <= candidate.at_us => Some(current),
                        _ => Some(candidate),
                    };
                }
            }
        }
        if !self.quiet && !lift_active && !burst_active {
            if let Some(low_since) = self.low_demand_since_us {
                let mut entry =
                    low_since.saturating_add(self.config.control.floor_settle_ms * US_PER_MS);
                if let Some(touch) = self.last_touch_us {
                    entry = entry
                        .max(touch.saturating_add(self.config.events.quiet_decay_ms * US_PER_MS));
                }
                if entry > now_us {
                    let candidate = Deadline {
                        at_us: entry,
                        kind: DeadlineKind::QuietEntry,
                    };
                    best = match best {
                        Some(current) if current.at_us <= candidate.at_us => Some(current),
                        _ => Some(candidate),
                    };
                }
            }
        }

        best
    }

    fn evaluate(&mut self, ts_us: u64, default_reason: DecisionReason) -> Vec<Decision> {
        if !self.screen_on {
            self.quiet = true;
        } else if !self.quiet && self.quiet_ready(ts_us) {
            self.quiet = true;
            // Quiet is the floor state by definition; keep the latch aligned
            // so the exit filter starts from a clean baseline.
            self.floor_low = true;
            self.floor_exit_since_us = None;
        }

        let quiet = self.quiet;
        // A pending decay step whose instant has already passed is consumed
        // here, so an event that arrives before the timer wakeup cannot make the
        // controller fall faster than the configured interval.
        if self.decay_at_us.is_some_and(|at| ts_us >= at) {
            self.decay_at_us = None;
        }
        let decay_gate = self.decay_at_us.is_some_and(|at| ts_us < at);
        let factor = self.config.control.drop_factor.max(1.0);
        let mut stepped_down = false;
        let mut decisions = Vec::with_capacity(self.clusters.len());

        for index in 0..self.clusters.len() {
            let min_khz = self.profile.clusters[index].min_khz;
            let current = self.clusters[index].ceiling_khz;

            let (mut chosen, mut reason) = if quiet {
                let reason = if self.screen_on {
                    DecisionReason::Quiet
                } else {
                    DecisionReason::ScreenOff
                };
                (min_khz, reason)
            } else {
                let (fraction, mut reason) = self.compute_fraction(index, ts_us, default_reason);
                let candidate = self.profile.snap_khz(index, fraction);
                let current_fraction = self.profile.clusters[index].fraction_of(current);
                let up = candidate > current;
                let threshold = if up {
                    self.config.control.step_hysteresis
                } else {
                    self.config.control.step_hysteresis / factor
                };
                let diff = (fraction - current_fraction).abs();
                let mut accept = diff >= threshold || fraction < self.config.control.drop_ratio;
                if up
                    && current == min_khz
                    && self.config.control.floor_residence_ms > 0
                    && !is_event_driven(reason)
                {
                    let elapsed = ts_us.saturating_sub(self.clusters[index].last_change_us);
                    if elapsed < self.config.control.floor_residence_ms * US_PER_MS {
                        accept = false;
                    }
                }
                if accept {
                    (candidate, reason)
                } else {
                    reason = DecisionReason::Unchanged;
                    (current, reason)
                }
            };

            // Release a lift stage by stage: one step may not fall by more than
            // `drop_factor`, and a second step waits for `touch_decay_ms`. A
            // screen-off still floors instantly (battery before smoothness).
            if chosen < current && reason != DecisionReason::ScreenOff {
                if decay_gate {
                    chosen = current;
                    reason = DecisionReason::Unchanged;
                } else {
                    let current_fraction = self.profile.clusters[index].fraction_of(current);
                    let lower = self
                        .profile
                        .snap_khz(index, current_fraction / factor);
                    if chosen < lower {
                        chosen = lower;
                        reason = if quiet {
                            DecisionReason::Quiet
                        } else {
                            reason
                        };
                        stepped_down = true;
                    }
                }
            }

            let changed = chosen != current;
            if changed {
                let cluster = &mut self.clusters[index];
                cluster.ceiling_khz = chosen;
                cluster.last_change_us = ts_us;
            }
            decisions.push(Decision {
                cluster: index,
                ceiling_khz: chosen,
                reason,
                changed,
            });
        }

        if decay_gate {
            // Keep the pending step: the next wakeup is already armed.
        } else if stepped_down {
            self.decay_at_us =
                Some(ts_us.saturating_add(self.config.control.touch_decay_ms * US_PER_MS));
        } else {
            self.decay_at_us = None;
        }

        decisions
    }

    fn compute_fraction(
        &self,
        index: usize,
        ts_us: u64,
        default_reason: DecisionReason,
    ) -> (f32, DecisionReason) {
        let cluster = &self.profile.clusters[index];
        let demand = self.clusters[index].busy;
        let mut fraction = load_curve(
            demand,
            self.config.control.quiet_ratio,
            self.config.control.reserve_ratio,
        );
        let mut reason = default_reason;

        if self.floor_low {
            fraction = fraction.min(self.config.control.quiet_cap);
            // The prime cluster is not driven by background or idle foreground
            // work. It participates only in foreground load, warm start and
            // frame bursts.
            if cluster.role == Role::Prime {
                fraction = 0.0;
            }
        }

        if self.lift_until_us.is_some_and(|until| ts_us < until) {
            let applies = self.lift_all_roles || cluster.role != Role::Prime;
            let level = Self::ramp_level(self.lift_ramp, ts_us)
                .unwrap_or(self.config.control.touch_lift_level);
            if applies {
                if self.floor_low {
                    // Leaving the floor: the staged level itself is the
                    // allowance. The latched quiet cap must not jump above the
                    // first stages, and stale busy values must not either.
                    fraction = level;
                    reason = if self.lift_all_roles {
                        DecisionReason::Foreground
                    } else {
                        DecisionReason::TouchLift
                    };
                } else if fraction < level {
                    fraction = level;
                    reason = if self.lift_all_roles {
                        DecisionReason::Foreground
                    } else {
                        DecisionReason::TouchLift
                    };
                }
            }
        }

        if self.burst_until_us.is_some_and(|until| ts_us < until) {
            let level = Self::ramp_level(self.burst_ramp, ts_us)
                .unwrap_or(self.config.control.frame_burst_level);
            if fraction < level {
                fraction = level;
                reason = DecisionReason::FrameBurst;
            }
        }

        fraction = fraction.min(self.config.control.role_cap.for_role(cluster.role));

        if let Some(cap) = self.thermal_cap {
            let before = fraction;
            fraction = fraction.min(cap);
            if fraction < before && !is_event_driven(reason) {
                reason = DecisionReason::Thermal;
            }
        }

        (fraction, reason)
    }

    fn quiet_ready(&self, ts_us: u64) -> bool {
        if !self.screen_on || !self.demands_seen {
            return false;
        }
        if self.lift_until_us.is_some_and(|until| ts_us < until) {
            return false;
        }
        if self.burst_until_us.is_some_and(|until| ts_us < until) {
            return false;
        }
        if let Some(touch) = self.last_touch_us {
            let decay = self.config.events.quiet_decay_ms * US_PER_MS;
            if ts_us < touch.saturating_add(decay) {
                return false;
            }
        }
        let Some(low_since) = self.low_demand_since_us else {
            return false;
        };
        let settle = self.config.control.floor_settle_ms * US_PER_MS;
        if ts_us < low_since.saturating_add(settle) {
            return false;
        }
        // `floor_low` is the latched view; the timestamp comparison covers the
        // case where the settle window completed between two demand samples
        // (the probe is change-driven, so a quiet device may publish no sample
        // at the exact boundary).
        self.floor_low || ts_us >= low_since.saturating_add(settle)
    }
}

fn is_event_driven(reason: DecisionReason) -> bool {
    matches!(
        reason,
        DecisionReason::TouchLift
            | DecisionReason::FrameBurst
            | DecisionReason::Foreground
            | DecisionReason::Launch
            | DecisionReason::Thermal
            | DecisionReason::Quiet
            | DecisionReason::ScreenOff
    )
}

/// Concave load curve: compress the low band, keep reserve at the top.
/// `g(0) = 0`, `g(1) = reserve`, continuous at 0.5.
fn load_curve(demand: f32, quiet_ratio: f32, reserve_ratio: f32) -> f32 {
    let demand = demand.clamp(0.0, 1.0);
    if demand <= 0.5 {
        quiet_ratio * demand.sqrt()
    } else {
        let anchor = quiet_ratio * 0.5_f32.sqrt();
        anchor + (demand - 0.5) * 2.0 * (reserve_ratio - anchor)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn load_curve_is_continuous_and_monotone() {
        let curve = |d: f32| load_curve(d, 0.5, 1.05);
        let mut previous = curve(0.0);
        let mut d = 0.01f32;
        while d <= 1.0 {
            let value = curve(d);
            assert!(value >= previous, "curve must be monotone");
            previous = value;
            d += 0.01;
        }
        assert!((curve(0.5) - 0.5 * 0.5f32.sqrt()).abs() < 1e-6);
        assert!((curve(1.0) - 1.05).abs() < 1e-5);
    }
}
