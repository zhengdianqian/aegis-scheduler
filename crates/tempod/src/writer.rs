// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Tiered cpufreq writer (v3).
//!
//! v2 pinned `scaling_min_freq` with a bind-mount and drove only the ceiling.
//! That is what made the device feel like two schedulers fighting: static pages
//! sat on the hardware minimum, one touch released everything, and the core
//! landed on the hardware maximum.
//!
//! v3 writes both nodes from the tier ladder:
//!   * `scaling_min_freq` = the ladder target (a real intermediate step);
//!   * `scaling_max_freq` = a cap a few tiers above it, and never below the
//!     safety band, so the governor still has headroom;
//!   * launch / burst / frame pressure open the cap to the hardware maximum,
//!     which keeps cold starts and heavy frames at official speed.
//!
//! No bind-mount is used for normal operation. A legacy mask left by v2 is
//! unmounted once at startup; a write rejected by a vendor QoS floor is
//! respected and retried only after a back-off instead of being fought.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempo_core::controller::Decision;
use tempo_core::DeviceProfile;

pub const CPUFREQ_ROOT: &str = "/sys/devices/system/cpu/cpufreq";

struct Policy {
    cluster: usize,
    dir: PathBuf,
    /// Pristine hardware floor and ceiling used by `restore`.
    pristine_min: u32,
    pristine_max: u32,
    original_governor: String,
    /// Effective hardware range: `hw_max` is what the platform actually
    /// accepts, not a theoretical `cpuinfo` value.
    hw_min: u32,
    hw_max: u32,
    /// Hardware step list, ascending, used to snap a target onto a real step.
    freqs: Vec<u32>,
    /// Last floor target applied. Used by the idle reconcile pass.
    last_target: u32,
    /// Last ceiling this daemon wrote (or the pristine ceiling before any write).
    last_cap: u32,
    /// Last time a ceiling write was issued for this cluster.
    last_cap_write: Option<Instant>,
    /// Last time the quiet floor was compared with the live value.
    last_reconcile: Option<Instant>,
    /// The controller settled on the lowest tier with no recent touch, launch
    /// or load: only then may the floor be pulled down.
    idle_floor: bool,
    /// Last time a `scaling_min_freq` write was issued for this cluster.
    last_floor_write: Option<Instant>,
    /// Lowest floor this platform actually accepted (0 = not yet learned).
    /// This device clamps the efficiency cluster to 672 MHz, so writing the
    /// ladder target there would be rejected on every pass.
    floor_effective: u32,
    /// Consecutive floor attempts that did not reach the request.
    floor_tries: u32,
    /// Live floor observed on the previous attempt.
    floor_seen: u32,
    /// Request used by the previous attempt.
    floor_request: u32,
    /// True once a floor at or below the ladder target has been observed.
    floor_achieved: bool,
    /// Last ceiling value this daemon requested and when.
    last_cap_request: Option<(u32, Instant)>,
}

/// Outcome of one idle floor evaluation.
enum FloorStep {
    /// Nothing to do.
    Idle,
    /// Write this value to `scaling_min_freq`.
    Write(u32),
    /// The platform clamps the floor at this value; stop asking for less.
    Learned(u32),
}

impl Policy {
    /// Decides the next floor action for an idle cluster.
    ///
    /// The platform may clamp `scaling_min_freq` above the request (this
    /// device pins the efficiency cluster at 672 MHz). Asking for less every
    /// pass would be a permanent fight, so the first two attempts discover the
    /// accepted floor and later passes request it directly. A vendor boost is
    /// retried with a short bounded back-off instead of tight-looping.
    fn floor_step(&mut self, target: u32, now: Instant) -> FloorStep {
        let Some(current) = read_u32(&self.dir.join("scaling_min_freq")) else {
            return FloorStep::Idle;
        };
        let desired = target.max(self.floor_effective);
        if current <= desired {
            self.floor_tries = 0;
            self.floor_seen = 0;
            self.floor_achieved = true;
            return FloorStep::Idle;
        }
        let wait = 500u64
            .saturating_mul(1 << self.floor_tries.min(3))
            .min(5000);
        if self
            .last_floor_write
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(wait))
        {
            return FloorStep::Idle;
        }
        if self.floor_request == desired && current == self.floor_seen {
            self.floor_tries = self.floor_tries.saturating_add(1);
        } else {
            self.floor_tries = 1;
            self.floor_seen = current;
        }
        self.floor_request = desired;
        if self.floor_tries >= 2 && self.floor_effective == 0 && !self.floor_achieved {
            // The platform rejected the ladder target twice with the same
            // higher value: that value is the real floor of this cluster.
            self.floor_effective = current;
            self.floor_tries = 0;
            return FloorStep::Learned(current);
        }
        FloorStep::Write(desired)
    }
}

/// Minimum spacing between write pairs for the same cluster, so two commits in
/// one event cannot write the same node twice.
const WRITE_SPACING: Duration = Duration::from_millis(30);
/// How often the idle state re-checks nodes a vendor writer may have moved.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Ceiling writes smaller than this fraction of the hardware span are skipped.
/// Each write runs the whole cpufreq notifier chain.
const CAP_DEADBAND: f32 = 0.04;
/// A ceiling request that does not move by at least the deadband is not
/// repeated within this window.
const CAP_RETRY: Duration = Duration::from_secs(10);

/// Whether normal control also writes `scaling_min_freq` while the device is
/// idle.
///
/// A vendor perf HAL raises the floor to 787 / 1056 / 1248 MHz on this device
/// on every touch and app launch, and it does not decay on its own: measured
/// idle, the ladder target 307 / 499 / 595 MHz was restored by a single write
/// and stayed for 45 s. The floor is therefore written back, but only while
/// the controller reports an idle cluster, and with a bounded back-off, so a
/// boost window is never fought mid-interaction.
/// `AEGIS_WRITE_FLOOR=0` turns it off for an A/B run on the same binary.
const WRITE_FLOOR_ENV: &str = "AEGIS_WRITE_FLOOR";

fn write_floor_enabled() -> bool {
    std::env::var(WRITE_FLOOR_ENV)
        .map(|value| !matches!(value.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

pub struct CpufreqWriter {
    policies: Vec<Policy>,
    applied: u64,
    cap_writes: u64,
    floor_writes: u64,
    rejected_floor: u64,
    /// Fraction of the hardware span that must always stay available as an
    /// upper cap. The controller already emits a cap; this is a second,
    /// independent floor of protection against a bad decision.
    safety_ratio: f32,
    /// Whether the ladder target is also written to `scaling_min_freq`.
    write_floor: bool,
    /// Read-only guard. No sysfs write happens before this instant, so a slow
    /// boot cannot ever be made slower by the module.
    guard_until: Option<Instant>,
    pub last_error: Option<String>,
}

impl CpufreqWriter {
    pub fn discover(profile: &DeviceProfile, safety_ratio: f32) -> Result<Self, String> {
        unmount_legacy_floors();
        let mut policies = Vec::with_capacity(profile.clusters.len());
        for (cluster, entry) in profile.clusters.iter().enumerate() {
            let cpu = *entry
                .cpus
                .first()
                .ok_or_else(|| format!("cluster {cluster} has no cpus"))?;
            let dir = policy_dir(cpu)
                .ok_or_else(|| format!("no cpufreq policy for cpu {cpu}"))?;
            let live_min = read_u32(&dir.join("scaling_min_freq"))
                .ok_or_else(|| format!("{} scaling_min_freq unreadable", dir.display()))?;
            let live_max = read_u32(&dir.join("scaling_max_freq"))
                .ok_or_else(|| format!("{} scaling_max_freq unreadable", dir.display()))?;
            let original_governor = fs::read_to_string(dir.join("scaling_governor"))
                .map(|text| text.trim().to_owned())
                .unwrap_or_default();
            let hw_min = read_u32(&dir.join("cpuinfo_min_freq")).unwrap_or(live_min);
            let hw_ceiling = read_u32(&dir.join("cpuinfo_max_freq")).unwrap_or(live_max);
            let mut freqs: Vec<u32> = fs::read_to_string(dir.join("scaling_available_frequencies"))
                .map(|text| {
                    text.split_whitespace()
                        .filter_map(|part| part.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            freqs.sort_unstable();
            freqs.dedup();

            let hw_max = hw_ceiling.max(live_max);
            let pristine_min = hw_min.min(hw_max);
            policies.push(Policy {
                cluster,
                dir,
                pristine_min,
                pristine_max: hw_max,
                original_governor,
                hw_min: pristine_min,
                hw_max,
                freqs,
                last_target: 0,
                last_cap: hw_max,
                last_cap_write: None,
                last_reconcile: None,
                idle_floor: false,
                last_floor_write: None,
                floor_effective: 0,
                floor_tries: 0,
                floor_seen: 0,
                floor_request: 0,
                floor_achieved: false,
                last_cap_request: None,
            });
        }
        Ok(Self {
            policies,
            applied: 0,
            cap_writes: 0,
            floor_writes: 0,
            rejected_floor: 0,
            safety_ratio: safety_ratio.clamp(0.20, 0.70),
            write_floor: write_floor_enabled(),
            guard_until: None,
            last_error: None,
        })
    }

    /// Keep every sysfs node untouched for `window`. Called once at startup so
    /// the module can never slow down the boot or the first unlock.
    pub fn arm_guard(&mut self, window: Duration) {
        self.guard_until = Some(Instant::now() + window);
    }

    fn guarded(&self) -> bool {
        self.guard_until.is_some_and(|until| Instant::now() < until)
    }

    /// True once the startup guard has expired but nothing has been written
    /// yet; the event loop uses this to push the first ladder state.
    pub fn needs_initial_apply(&self) -> bool {
        self.applied == 0 && !self.guarded()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The read-only startup guard is still running: no cpufreq node has been
    /// touched by this daemon yet.
    pub fn guard_active(&self) -> bool {
        self.guarded()
    }

    /// Re-derives the cap from the last target without waiting for a scheduling
    /// event. Vendor perf services move nodes while the screen is static.
    pub fn reconcile(&mut self) -> Result<(), String> {
        if self.guarded() {
            return Ok(());
        }
        let now = Instant::now();
        let mut failure: Option<String> = None;
        for policy in &mut self.policies {
            if policy.last_target == 0 {
                continue;
            }
            let live_max = read_u32(&policy.dir.join("scaling_max_freq")).unwrap_or(policy.hw_max);
            if live_max <= policy.hw_min {
                continue;
            }
            if policy
                .last_reconcile
                .is_some_and(|last| now.duration_since(last) < RECONCILE_INTERVAL)
            {
                continue;
            }
            policy.last_reconcile = Some(now);
            // A vendor perf service can raise the floor without any scheduling
            // event, so the idle state repairs it here as well.
            if self.write_floor && policy.idle_floor {
                match policy.floor_step(policy.last_target, now) {
                    FloorStep::Write(value) => {
                        match write_khz(&policy.dir.join("scaling_min_freq"), value) {
                            Ok(()) => {
                                policy.last_floor_write = Some(now);
                                self.floor_writes += 1;
                                self.applied += 1;
                            }
                            Err(problem) => {
                                failure.get_or_insert(problem);
                            }
                        }
                    }
                    FloorStep::Learned(_) => self.rejected_floor += 1,
                    FloorStep::Idle => {}
                }
            }
            let cap = cap_for(policy, policy.last_target, live_max, self.safety_ratio);
            if !cap_needs_write(policy, live_max, cap, policy.last_target)
                || cap_repeat_suppressed(policy, cap, policy.last_target, live_max, now)
            {
                continue;
            }
            if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), cap) {
                failure.get_or_insert(problem);
                continue;
            }
            policy.last_cap = cap;
            policy.last_cap_write = Some(now);
            policy.last_cap_request = Some((cap, now));
            self.cap_writes += 1;
            self.applied += 1;
        }
        self.last_error = failure.clone();
        match failure {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Applies each decision as a ladder **target** on `scaling_min_freq` and a
    /// matching **cap** on `scaling_max_freq`. A live ceiling below the request
    /// (usually a vendor thermal clamp) is respected, never fought.
    pub fn apply(&mut self, decisions: &[Decision]) -> Result<(), String> {
        if self.guarded() {
            return Ok(());
        }
        let mut failure: Option<String> = None;
        for decision in decisions {
            let Some(policy) = self
                .policies
                .iter_mut()
                .find(|policy| policy.cluster == decision.cluster)
            else {
                continue;
            };
            let now = Instant::now();
            let live_max = read_u32(&policy.dir.join("scaling_max_freq")).unwrap_or(policy.hw_max);
            if live_max <= policy.hw_min {
                // Locked by a vendor range; there is nothing left to schedule.
                continue;
            }

            let mut target = decision.ceiling_khz.clamp(policy.hw_min, policy.hw_max);
            if let Some(step) = snap_down(&policy.freqs, target) {
                target = step.max(policy.hw_min);
            }
            target = target.min(live_max);

            let cap = cap_for(policy, target, live_max, self.safety_ratio)
                .max(target)
                .min(policy.hw_max);

            // A ceiling the vendor clamped below our target is not ours to
            // raise. Only a cap this daemon owns is opened again.
            let vendor_clamped = live_max < cap && live_max < policy.last_cap;
            let write_cap = if vendor_clamped { live_max } else { cap };

            policy.last_target = target;
            if write_cap != live_max
                && cap_needs_write(policy, live_max, write_cap, target)
                && !cap_repeat_suppressed(policy, write_cap, target, live_max, now)
                && !policy
                    .last_cap_write
                    .is_some_and(|last| now.duration_since(last) < WRITE_SPACING)
            {
                if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), write_cap) {
                    failure.get_or_insert(problem);
                    continue;
                }
                policy.last_cap = write_cap;
                policy.last_cap_write = Some(now);
                policy.last_cap_request = Some((write_cap, now));
                self.cap_writes += 1;
                self.applied += 1;
            }

            // Low tier with no touch, launch or load pressure: the vendor boost
            // floor can be lowered without fighting an interaction window.
            policy.idle_floor = decision.idle;

            if !self.write_floor || !policy.idle_floor {
                tracing::debug!(cluster = policy.cluster, target, write_cap, live_max, "cap applied");
                continue;
            }
            match policy.floor_step(target, now) {
                FloorStep::Write(value) => match write_khz(&policy.dir.join("scaling_min_freq"), value)
                {
                    Ok(()) => {
                        policy.last_floor_write = Some(now);
                        self.floor_writes += 1;
                        self.applied += 1;
                        tracing::debug!(cluster = policy.cluster, value, "floor lowered");
                    }
                    Err(problem) => {
                        failure.get_or_insert(problem);
                    }
                },
                FloorStep::Learned(value) => {
                    self.rejected_floor += 1;
                    tracing::debug!(cluster = policy.cluster, value, "platform floor learned");
                }
                FloorStep::Idle => {}
            }
        }
        self.last_error = failure.clone();
        match failure {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// True when a policy is locked with `max <= min` below the hardware
    /// ceiling, which would mean the cluster cannot scale at all.
    pub fn is_locked(&self) -> bool {
        self.policies.iter().any(|policy| {
            let min = read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(0);
            let max = read_u32(&policy.dir.join("scaling_max_freq")).unwrap_or(0);
            max > 0 && max <= min && max < policy.hw_max
        })
    }

    /// Restores every policy to the pristine hardware range observed at
    /// discovery. Ceiling first, so an intermediate state never has min > max.
    pub fn restore(&mut self) -> Result<(), String> {
        let mut error: Option<String> = None;
        for policy in &mut self.policies {
            if let Err(problem) =
                write_khz(&policy.dir.join("scaling_max_freq"), policy.pristine_max)
            {
                error.get_or_insert(problem);
            }
            if let Err(problem) =
                write_khz(&policy.dir.join("scaling_min_freq"), policy.pristine_min)
            {
                error.get_or_insert(problem);
            }
            if !policy.original_governor.is_empty() {
                if let Err(problem) =
                    write_text(&policy.dir.join("scaling_governor"), &policy.original_governor)
                {
                    error.get_or_insert(problem);
                }
            }
        }
        self.guard_until = None;
        self.applied = 0;
        match error {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    pub fn applied(&self) -> u64 {
        self.applied
    }

    pub fn cap_writes(&self) -> u64 {
        self.cap_writes
    }

    pub fn floor_writes(&self) -> u64 {
        self.floor_writes
    }

    pub fn rejected_floor(&self) -> u64 {
        self.rejected_floor
    }

    pub fn backup_json(&self) -> String {
        let entries: Vec<_> = self
            .policies
            .iter()
            .map(|policy| {
                serde_json::json!({
                    "cluster": policy.cluster,
                    "dir": policy.dir.display().to_string(),
                    "min_freq": policy.pristine_min,
                    "max_freq": policy.pristine_max,
                    "governor": policy.original_governor,
                })
            })
            .collect();
        serde_json::json!({
            "version": 1,
            "policies": entries,
        })
        .to_string()
    }
}

impl Drop for CpufreqWriter {
    fn drop(&mut self) {
        if self.applied > 0 {
            let _ = self.restore();
        }
    }
}

pub fn policy_dir(cpu: u32) -> Option<PathBuf> {
    let direct = Path::new(CPUFREQ_ROOT).join(format!("policy{cpu}"));
    if direct.join("scaling_max_freq").exists() {
        return Some(direct);
    }
    let entries = fs::read_dir(CPUFREQ_ROOT).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("scaling_max_freq").exists() {
            continue;
        }
        let related = fs::read_to_string(path.join("related_cpus")).ok()?;
        let ids: Vec<u32> = related
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        if ids.contains(&cpu) {
            return Some(path);
        }
    }
    None
}

pub fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// v2 installed a bind-mount mask over `scaling_min_freq`. v3 does not use it,
/// so drop any leftover mount once at startup; otherwise every floor write
/// would only edit the mask file and the kernel floor would stay pinned.
fn unmount_legacy_floors() {
    let Ok(table) = fs::read_to_string("/proc/mounts") else {
        return;
    };
    for line in table.lines() {
        let Some(node) = line.split_whitespace().nth(1) else {
            continue;
        };
        if node.ends_with("/scaling_min_freq") && node.starts_with(CPUFREQ_ROOT) {
            let _ = Command::new("umount").arg(node).output();
            tracing::info!(node, "unmounted legacy v2 floor mask");
        }
    }
}

fn write_khz(path: &Path, value: u32) -> Result<(), String> {
    write_text(path, &value.to_string())
}

/// Upper cap for a cluster: the controller's ladder cap, clamped so at least
/// `safety_ratio` of the span always stays available, and never below the
/// target. A vendor clamp below the request is respected by the caller.
fn cap_for(policy: &Policy, target: u32, live_max: u32, safety_ratio: f32) -> u32 {
    let span = policy.hw_max.saturating_sub(policy.hw_min);
    let safety = policy.hw_min.saturating_add((span as f32 * safety_ratio) as u32);
    let ceiling_limit = if live_max <= policy.hw_min {
        policy.hw_max
    } else {
        live_max
    };
    target
        .max(safety)
        .min(policy.hw_max)
        .clamp(target, ceiling_limit.max(target))
}

/// Whether the live ceiling is far enough from the computed one to be worth a
/// write. Reads are free, writes run the whole cpufreq notifier chain.
fn cap_needs_write(policy: &Policy, live_max: u32, cap: u32, target: u32) -> bool {
    if cap == live_max {
        return false;
    }
    if live_max < target {
        // The ceiling must at least allow the floor target.
        return true;
    }
    let span = policy.hw_max.saturating_sub(policy.hw_min);
    let deadband = (span as f32 * CAP_DEADBAND) as u32;
    cap.abs_diff(live_max) > deadband
}

fn cap_repeat_suppressed(
    policy: &Policy,
    cap: u32,
    target: u32,
    live_max: u32,
    now: Instant,
) -> bool {
    if live_max < target {
        return false;
    }
    let span = policy.hw_max.saturating_sub(policy.hw_min);
    let deadband = (span as f32 * CAP_DEADBAND) as u32;
    match policy.last_cap_request {
        Some((last, at)) => now.duration_since(at) < CAP_RETRY && cap.abs_diff(last) <= deadband,
        None => false,
    }
}

/// Largest hardware step that does not exceed `value`.
fn snap_down(freqs: &[u32], value: u32) -> Option<u32> {
    freqs.iter().copied().rev().find(|step| *step <= value)
}

fn write_text(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, format!("{value}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}
