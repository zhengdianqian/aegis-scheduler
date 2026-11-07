// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use crate::config::ChargeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeAction {
    None,
    Stop,
    Resume,
}

impl ChargeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Stop => "stop",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeReason {
    TempHigh,
    FullLatch,
    UpperBound,
    PlanWindow,
}

impl ChargeReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::TempHigh => "temp_high",
            Self::FullLatch => "full_latch",
            Self::UpperBound => "upper_bound",
            Self::PlanWindow => "plan_window",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChargeSample {
    pub ts_us: u64,
    /// Battery percentage. `None` means "unreadable": the state machine never
    /// guesses a value from a failed read.
    pub percent: Option<u8>,
    /// Battery temperature in Celsius. `None` means "unreadable".
    pub temp_c: Option<f32>,
    pub plugged: bool,
}

/// One schedule window in minutes after midnight. A window whose end is
/// greater than its start is a same-day window; an end smaller than the start
/// is a cross-midnight window attributed to its start day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanWindow {
    pub start_min: u16,
    pub end_min: u16,
}

impl PlanWindow {
    pub fn parse(start: &str, end: &str) -> Option<Self> {
        let start_min = parse_clock(start)?;
        let end_min = parse_clock(end)?;
        if start_min == end_min {
            return None;
        }
        Some(Self { start_min, end_min })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    Everyday,
    Workdays,
    Custom,
}

/// Normalised reservation plan. An empty or disabled plan never blocks
/// charging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargePlan {
    pub enabled: bool,
    pub mode: PlanMode,
    pub weekdays: Vec<u8>,
    pub windows: Vec<PlanWindow>,
}

impl Default for ChargePlan {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: PlanMode::Everyday,
            weekdays: vec![1, 2, 3, 4, 5, 6, 7],
            windows: vec![PlanWindow {
                start_min: 23 * 60,
                end_min: 7 * 60,
            }],
        }
    }
}

impl ChargePlan {
    pub fn normalize(
        enabled: bool,
        mode: &str,
        weekdays: &[u8],
        windows: &[(String, String)],
    ) -> Self {
        let mode = match mode {
            "workdays" => PlanMode::Workdays,
            "custom" => PlanMode::Custom,
            _ => PlanMode::Everyday,
        };
        let mut days: Vec<u8> = weekdays
            .iter()
            .copied()
            .filter(|day| (1..=7).contains(day))
            .collect();
        days.sort_unstable();
        days.dedup();
        if days.is_empty() {
            days = vec![1, 2, 3, 4, 5, 6, 7];
        }
        let windows: Vec<PlanWindow> = windows
            .iter()
            .filter_map(|(start, end)| PlanWindow::parse(start, end))
            .take(6)
            .collect();
        Self {
            enabled,
            mode,
            weekdays: days,
            windows,
        }
    }

    /// Days on which a window may start, for the configured mode.
    pub fn days(&self) -> Vec<u8> {
        match self.mode {
            PlanMode::Everyday => vec![1, 2, 3, 4, 5, 6, 7],
            PlanMode::Workdays => vec![1, 2, 3, 4, 5],
            PlanMode::Custom => self.weekdays.clone(),
        }
    }

    /// Whether the current local instant is inside an allowed charging window.
    /// `day` is 1=Monday..7=Sunday; `minute` is minutes after local midnight.
    pub fn allows(&self, day: u8, minute: u16) -> bool {
        if !self.enabled || self.windows.is_empty() {
            return true;
        }
        let days = self.days();
        if days.is_empty() {
            return true;
        }
        let previous = if day == 1 { 7 } else { day - 1 };
        let minute = minute as i32;
        self.windows.iter().any(|window| {
            let start = window.start_min as i32;
            let end = window.end_min as i32;
            if end > start {
                days.contains(&day) && minute >= start && minute < end
            } else {
                (minute >= start && days.contains(&day))
                    || (minute < end && days.contains(&previous))
            }
        })
    }

    /// Minutes until the "allowed" flag flips. Used to arm the one-shot
    /// boundary timer; no periodic polling is involved.
    pub fn next_change_after(&self, day: u8, minute: u16) -> Option<u32> {
        if !self.enabled || self.windows.is_empty() {
            return None;
        }
        let days = self.days();
        if days.is_empty() {
            return None;
        }
        let now_allowed = self.allows(day, minute);
        for step in 1..=(8 * 1440u32) {
            let total = minute as u32 + step;
            let next_day = ((day as u32 - 1 + total / 1440) % 7) as u8 + 1;
            let next_minute = (total % 1440) as u16;
            if self.allows(next_day, next_minute) != now_allowed {
                return Some(step);
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct ChargeDecision {
    pub allow_charge: bool,
    pub action: ChargeAction,
    /// Fixed priority order: temperature > 100% latch > upper bound > plan.
    pub reasons: Vec<ChargeReason>,
}

impl ChargeDecision {
    pub fn reasons_joined(&self) -> String {
        self.reasons
            .iter()
            .map(|reason| reason.code())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn primary(&self) -> Option<ChargeReason> {
        self.reasons.first().copied()
    }
}

/// Pure charge state machine. It mirrors `webui/CHARGE-CONTROL.md` and the
/// sandboxed `webui/src/charge-model.js` step function one-for-one.
#[derive(Debug, Default)]
pub struct ChargeGovernor {
    temp_blocked: bool,
    upper_latched: bool,
    upper_hold: bool,
    armed: bool,
    stopped: bool,
    last_percent: Option<u8>,
    last_plugged: Option<bool>,
    stop_at: Option<u8>,
    resume_at: Option<u8>,
    temp_stop: Option<f32>,
    temp_resume: Option<f32>,
    latch: Option<bool>,
}

impl ChargeGovernor {
    pub fn new() -> Self {
        Self {
            armed: true,
            ..Self::default()
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Runs one decision step. `plan_allowed` is computed from
    /// `charge_plan.json`; `None` means the plan does not restrict charging.
    pub fn update(
        &mut self,
        sample: &ChargeSample,
        cfg: &ChargeConfig,
        plan_allowed: Option<bool>,
    ) -> ChargeDecision {
        // Only a 100% cap latches until the next plug-in. 80/90 use hysteresis.
        let latch = cfg.stop_at_percent == 100;

        let stop_at = Some(cfg.stop_at_percent);
        let resume_at = Some(cfg.resume_at_percent);
        let temp_stop = Some(cfg.temp_stop_c);
        let temp_resume = Some(cfg.temp_resume_c);

        let first_run = self.stop_at.is_none()
            && self.temp_stop.is_none()
            && self.last_percent.is_none()
            && self.latch.is_none();
        let config_changed = !first_run
            && (self.stop_at != stop_at
                || self.resume_at != resume_at
                || self.temp_stop != temp_stop
                || self.temp_resume != temp_resume
                || self.latch.is_some_and(|previous| previous != latch));

        if first_run || config_changed {
            self.temp_blocked = false;
            self.upper_latched = false;
            self.upper_hold = false;
        }

        let replugged = self
            .last_plugged
            .is_some_and(|previous| previous != sample.plugged && sample.plugged);
        let unplugged = self
            .last_plugged
            .is_some_and(|previous| previous != sample.plugged && !sample.plugged);
        if replugged || unplugged {
            self.temp_blocked = false;
            self.upper_latched = false;
            self.upper_hold = false;
        }

        if !sample.plugged {
            self.armed = true;
            self.stopped = false;
            self.last_percent = sample.percent;
            self.last_plugged = Some(false);
            self.stop_at = stop_at;
            self.resume_at = resume_at;
            self.temp_stop = temp_stop;
            self.temp_resume = temp_resume;
            self.latch = Some(latch);
            return ChargeDecision {
                allow_charge: true,
                action: ChargeAction::None,
                reasons: Vec::new(),
            };
        }

        if first_run || replugged || config_changed {
            self.armed = match sample.percent {
                Some(percent) => percent < cfg.stop_at_percent,
                None => true,
            };
        }

        if let (Some(temp), Some(limit)) = (sample.temp_c, temp_stop) {
            if temp >= limit {
                self.temp_blocked = true;
            } else if temp_resume.is_some_and(|resume| temp <= resume) {
                self.temp_blocked = false;
            }
        }

        if latch {
            self.upper_hold = false;
            if let (Some(percent), Some(stop_at)) = (sample.percent, stop_at) {
                if percent < stop_at {
                    self.armed = true;
                } else if self.armed {
                    self.upper_latched = true;
                    self.armed = false;
                }
            }
        } else {
            self.upper_latched = false;
            self.armed = true;
            if let (Some(percent), Some(stop_at)) = (sample.percent, stop_at) {
                if percent >= stop_at {
                    self.upper_hold = true;
                } else if resume_at.is_some_and(|resume| percent <= resume) {
                    self.upper_hold = false;
                }
            }
        }

        let plan_blocked = plan_allowed == Some(false);
        let mut reasons = Vec::new();
        if self.temp_blocked {
            reasons.push(ChargeReason::TempHigh);
        }
        if self.upper_latched {
            reasons.push(ChargeReason::FullLatch);
        }
        if self.upper_hold {
            reasons.push(ChargeReason::UpperBound);
        }
        if plan_blocked {
            reasons.push(ChargeReason::PlanWindow);
        }

        let allow_charge = reasons.is_empty();
        let was_stopped = self.stopped;
        let action = if allow_charge {
            if was_stopped {
                ChargeAction::Resume
            } else {
                ChargeAction::None
            }
        } else if was_stopped {
            ChargeAction::None
        } else {
            ChargeAction::Stop
        };

        self.stopped = !allow_charge;
        self.last_percent = sample.percent;
        self.last_plugged = Some(true);
        self.stop_at = stop_at;
        self.resume_at = resume_at;
        self.temp_stop = temp_stop;
        self.temp_resume = temp_resume;
        self.latch = Some(latch);

        ChargeDecision {
            allow_charge,
            action,
            reasons,
        }
    }
}

fn parse_clock(value: &str) -> Option<u16> {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let hours: u16 = value[0..2].parse().ok()?;
    let minutes: u16 = value[3..5].parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChargeConfig;

    fn cfg(stop: u8, resume: u8) -> ChargeConfig {
        ChargeConfig {
            stop_at_percent: stop,
            resume_at_percent: resume,
            temp_stop_c: 45.0,
            temp_resume_c: 40.0,
            latch_per_plug: false,
        }
    }

    fn step(gov: &mut ChargeGovernor, percent: u8, temp: f32, cfg: &ChargeConfig) -> ChargeDecision {
        gov.update(
            &ChargeSample {
                ts_us: 0,
                percent: Some(percent),
                temp_c: Some(temp),
                plugged: true,
            },
            cfg,
            None,
        )
    }

    #[test]
    fn upper_bound_hysteresis() {
        let config = cfg(80, 70);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 79, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 80, 33.0, &config).action, ChargeAction::Stop);
        assert_eq!(step(&mut gov, 75, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 71, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 70, 33.0, &config).action, ChargeAction::Resume);
        assert_eq!(step(&mut gov, 69, 33.0, &config).action, ChargeAction::None);
    }

    #[test]
    fn full_latch_survives_same_plug_and_clears_on_replug() {
        let config = cfg(100, 90);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 99, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::Stop);
        assert_eq!(step(&mut gov, 99, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 95, 33.0, &config).action, ChargeAction::None);

        gov.update(
            &ChargeSample {
                ts_us: 0,
                percent: Some(95),
                temp_c: Some(33.0),
                plugged: false,
            },
            &config,
            None,
        );
        // A new plug at 95% is allowed; 100% must be crossed again to latch.
        assert_eq!(step(&mut gov, 95, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::Stop);
    }

    #[test]
    fn replug_at_full_does_not_latch_immediately() {
        let config = cfg(100, 90);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::None);
        gov.update(
            &ChargeSample {
                ts_us: 0,
                percent: Some(100),
                temp_c: Some(33.0),
                plugged: false,
            },
            &config,
            None,
        );
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 99, 33.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::Stop);
    }

    #[test]
    fn restart_at_full_does_not_latch() {
        let config = cfg(100, 90);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 100, 33.0, &config).action, ChargeAction::None);
    }

    #[test]
    fn restart_with_upper_bound_stops() {
        let config = cfg(80, 70);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 85, 33.0, &config).action, ChargeAction::Stop);
    }

    #[test]
    fn config_change_recomputes_state() {
        let mut gov = ChargeGovernor::new();
        let config100 = cfg(100, 90);
        assert_eq!(step(&mut gov, 90, 33.0, &config100).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 100, 33.0, &config100).action, ChargeAction::Stop);
        let config80 = cfg(80, 70);
        // Still above the new 80% cap, so the stop is re-evaluated and kept.
        assert_eq!(step(&mut gov, 100, 33.0, &config80).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 70, 33.0, &config80).action, ChargeAction::Resume);
    }

    #[test]
    fn temperature_hysteresis_and_priority() {
        let config = cfg(100, 90);
        let mut gov = ChargeGovernor::new();
        assert_eq!(step(&mut gov, 60, 46.0, &config).action, ChargeAction::Stop);
        // Hysteresis: 44 °C is not hot enough to enter but not cool enough to clear.
        assert_eq!(step(&mut gov, 60, 44.0, &config).action, ChargeAction::None);
        assert_eq!(step(&mut gov, 60, 39.0, &config).action, ChargeAction::Resume);

        let decision = step(&mut gov, 100, 46.0, &config);
        assert_eq!(decision.action, ChargeAction::Stop);
        assert_eq!(decision.primary(), Some(ChargeReason::TempHigh));
        assert_eq!(
            decision.reasons,
            vec![ChargeReason::TempHigh, ChargeReason::FullLatch]
        );
    }

    #[test]
    fn plan_cross_midnight_belongs_to_start_day() {
        let plan = ChargePlan::normalize(
            true,
            "custom",
            &[1],
            &[("23:00".to_owned(), "07:00".to_owned())],
        );
        assert!(plan.allows(1, 23 * 60 + 30));
        assert!(plan.allows(2, 2 * 60));
        assert!(!plan.allows(2, 23 * 60 + 30));
        assert!(!plan.allows(1, 12 * 60));
        assert_eq!(plan.next_change_after(1, 23 * 60 + 30), Some(7 * 60 + 30));
    }

    #[test]
    fn plan_end_boundary_is_exclusive_start_inclusive() {
        let plan = ChargePlan::normalize(
            true,
            "everyday",
            &[],
            &[("23:00".to_owned(), "07:00".to_owned())],
        );
        assert!(plan.allows(1, 23 * 60));
        assert!(!plan.allows(2, 7 * 60));
        assert_eq!(plan.next_change_after(2, 7 * 60), Some(16 * 60));
    }

    #[test]
    fn plan_combines_with_upper_bound() {
        let plan = ChargePlan::normalize(
            true,
            "everyday",
            &[],
            &[("23:00".to_owned(), "07:00".to_owned())],
        );
        let config = cfg(80, 70);
        let mut gov = ChargeGovernor::new();
        let decision = gov.update(
            &ChargeSample {
                ts_us: 0,
                percent: Some(80),
                temp_c: Some(33.0),
                plugged: true,
            },
            &config,
            Some(plan.allows(1, 12 * 60)),
        );
        assert_eq!(
            decision.reasons,
            vec![ChargeReason::UpperBound, ChargeReason::PlanWindow]
        );
    }
}
