// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Controlled cpufreq writer.
//!
//! The daemon is the only writer. It publishes a per-cluster **floor** through
//! `scaling_min_freq` only: the hardware maximum is never lowered, so heavy
//! work (cold starts, bursts, games) always has the full hardware range
//! available. A floor is a target the governor may sit on, never a hard lock,
//! and `scaling_max_freq` is only repaired upward at startup if an older build
//! left it capped. Before every write the live ceiling is checked against the
//! effective hardware maximum; a lower ceiling (for example a vendor thermal
//! limit) makes the write be refused and logged instead of fought.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempo_core::controller::{Decision, DecisionReason};
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
    /// accepts after a repair write, not a theoretical `cpuinfo` value.
    hw_min: u32,
    hw_max: u32,
    /// Hardware step list, ascending, used to snap a floor onto a real step.
    freqs: Vec<u32>,
    /// Last time a floor write was issued for this cluster.
    last_write: Option<Instant>,
    /// When a quiet downshift first became pending. A vendor boost hint can
    /// raise `scaling_min_freq` at any moment; we only pull it back after the
    /// target has stayed low for a short while, instead of fighting every
    /// hint with a cpufreq notifier-heavy write.
    quiet_pending_since: Option<Instant>,
    /// Last time the quiet floor was compared with the live value. A vendor
    /// perf service may raise `scaling_min_freq` without telling anyone, so
    /// the quiet state reconciles the floor occasionally instead of never.
    last_reconcile: Option<Instant>,
    /// When a floor write was silently rejected by a vendor QoS limit
    /// (`scaling_min_freq` read back above the target). Further quiet retries
    /// wait until this instant so the daemon does not fight the vendor.
    retry_after: Option<Instant>,
}

/// A quiet downshift is applied only after the low target persists this long.
const QUIET_SETTLE: Duration = Duration::from_millis(300);
/// Minimum spacing between floor writes, so two commits in one event cannot
/// write the same cluster twice.
const WRITE_SPACING: Duration = Duration::from_millis(30);
/// How often the quiet state re-checks a floor that a vendor writer may have
/// raised. Reads are cheap (`scaling_min_freq` is a plain sysfs node), writes
/// happen only when the live floor actually differs from the target.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
/// After a floor write that a vendor QoS limit rejected, stop retrying for a
/// while. Fighting a persistent QoS request only burns CPU and log lines.
const BLOCKED_RETRY: Duration = Duration::from_secs(30);

pub struct CpufreqWriter {
    policies: Vec<Policy>,
    applied: u64,
    pub last_error: Option<String>,
}

impl CpufreqWriter {
    /// Binds one policy directory per cluster. Missing or unreadable policy
    /// nodes are a capability failure: the caller must not schedule.
    pub fn discover(profile: &DeviceProfile, _parked_headroom: f32) -> Result<Self, String> {
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

            // Repair a ceiling left below the hardware maximum by an older
            // build. This is the only ever upward max write; it is never used
            // again afterwards, so a vendor thermal limit is never fought.
            let mut hw_max = live_max;
            if live_max < hw_ceiling {
                write_khz(&dir.join("scaling_max_freq"), hw_ceiling)?;
                // Vendor QoS applies the ceiling asynchronously and may clamp
                // it in more than one step (e.g. 3360000 -> 2726400 ->
                // 2342400). Keep the lowest value observed during a short
                // settle window so a transient read-back is never mistaken
                // for the effective hardware maximum. A too-high `hw_max`
                // would make every later floor write fail the ceiling check.
                let mut settled = hw_ceiling;
                for _ in 0..6 {
                    std::thread::sleep(Duration::from_millis(50));
                    if let Some(value) = read_u32(&dir.join("scaling_max_freq")) {
                        settled = settled.min(value);
                    }
                }
                hw_max = settled;
                tracing::warn!(
                    cluster,
                    live_max,
                    repaired = hw_max,
                    "ceiling was below hardware max; restored"
                );
            }
            // Keep the floor below the effective ceiling at all times.
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
                last_write: None,
                quiet_pending_since: None,
                last_reconcile: None,
                retry_after: None,
            });
        }
        Ok(Self {
            policies,
            applied: 0,
            last_error: None,
        })
    }

    /// Retained for compatibility with the configuration surface. The new
    /// design no longer parks a ceiling, so the headroom has no effect.
    pub fn set_parked_headroom(&mut self, _parked_headroom: f32) {}

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Applies each decision as a **floor** on `scaling_min_freq`.
    /// `scaling_max_freq` is never written here; the hardware ceiling stays
    /// intact. A live ceiling below the effective hardware maximum (usually a
    /// vendor thermal cap) refuses the write and records the reason.
    pub fn apply(&mut self, decisions: &[Decision]) -> Result<(), String> {
        let mut refusal: Option<String> = None;
        for decision in decisions {
            let Some(policy) = self
                .policies
                .iter_mut()
                .find(|policy| policy.cluster == decision.cluster)
            else {
                continue;
            };
            // An unchanged target needs no sysfs read or write. Only a real
            // change (boost, decay step, quiet entry) or a pending quiet
            // downshift pays for a read/write. In the quiet state the floor is
            // re-checked about once a second, because MIUI's perf service may
            // raise `scaling_min_freq` on its own.
            if !decision.changed && policy.quiet_pending_since.is_none() {
                let quiet = matches!(
                    decision.reason,
                    DecisionReason::Quiet | DecisionReason::ScreenOff
                );
                if !quiet {
                    continue;
                }
                let now = Instant::now();
                if policy.retry_after.is_some_and(|retry| now < retry) {
                    continue;
                }
                if policy
                    .last_reconcile
                    .is_some_and(|last| now.duration_since(last) < RECONCILE_INTERVAL)
                {
                    continue;
                }
                policy.last_reconcile = Some(now);
            }
            let live_max = read_u32(&policy.dir.join("scaling_max_freq")).unwrap_or(0);
            if live_max < policy.hw_max {
                let message = format!(
                    "cluster {} live ceiling {live_max} below hardware max {}; write refused",
                    policy.cluster, policy.hw_max
                );
                tracing::error!(cluster = policy.cluster, live_max, hw_max = policy.hw_max, "{message}");
                refusal.get_or_insert(message);
                continue;
            }
            let mut target = decision.ceiling_khz.clamp(policy.hw_min, policy.hw_max);
            // Never let the floor exceed the live ceiling. A vendor writer may
            // raise the floor above the requested target; keep it as is.
            target = target.min(live_max);
            if let Some(step) = snap_down(&policy.freqs, target) {
                target = step.max(policy.hw_min);
            }
            let current_min = read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(0);
            if current_min == target {
                policy.quiet_pending_since = None;
                continue;
            }
            let now = Instant::now();
            let quiet = matches!(decision.reason, DecisionReason::Quiet);
            if quiet && target < current_min {
                let since = *policy.quiet_pending_since.get_or_insert(now);
                if now.duration_since(since) < QUIET_SETTLE {
                    continue;
                }
            } else {
                policy.quiet_pending_since = None;
            }
            if let Some(last) = policy.last_write {
                if now.duration_since(last) < WRITE_SPACING {
                    continue;
                }
            }
            write_khz(&policy.dir.join("scaling_min_freq"), target)?;
            let readback = read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(target);
            if readback > target {
                // A vendor QoS request (for example perfservice's screen-on
                // floor) rejected the write. Back off instead of fighting it.
                policy.retry_after = Some(now + BLOCKED_RETRY);
                tracing::debug!(
                    cluster = policy.cluster,
                    target,
                    readback,
                    "floor rejected by vendor QoS; retry deferred"
                );
            } else {
                policy.retry_after = None;
            }
            policy.last_write = Some(now);
            policy.quiet_pending_since = None;
            self.applied += 1;
            tracing::debug!(cluster = policy.cluster, target, live_max, "floor applied");
        }
        self.last_error = refusal.clone();
        match refusal {
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
            if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), policy.pristine_max)
            {
                error.get_or_insert(problem);
            }
            if let Err(problem) = write_khz(&policy.dir.join("scaling_min_freq"), policy.pristine_min)
            {
                error.get_or_insert(problem);
            }
            if !policy.original_governor.is_empty() {
                if let Err(problem) = write_text(&policy.dir.join("scaling_governor"), &policy.original_governor) {
                    error.get_or_insert(problem);
                }
            }
        }
        self.applied = 0;
        match error {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Original values as JSON, persisted so an external tool can restore the
    /// policies even after a hard kill of the daemon.
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

fn write_khz(path: &Path, value: u32) -> Result<(), String> {
    write_text(path, &value.to_string())
}

/// Largest hardware step that does not exceed `value`.
fn snap_down(freqs: &[u32], value: u32) -> Option<u32> {
    freqs.iter().copied().rev().find(|step| *step <= value)
}

fn write_text(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, format!("{value}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}
