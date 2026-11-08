// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Controlled cpufreq writer.
//!
//! The daemon publishes a per-cluster **floor** through `scaling_min_freq` and
//! a soft **ceiling** through `scaling_max_freq`. The floor is a target the
//! governor may sit on, never a hard lock. The ceiling is `floor +
//! span * ceiling_headroom`, so the hardware maximum stays available whenever
//! the measured demand earns it, while a vendor boost or an input hint can no
//! longer slam a cluster to its top tier on a static screen. Before every write
//! the live ceiling is compared with the target; a lower live ceiling (for
//! example a vendor thermal limit) is respected instead of fought.

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
    /// Span of the ceiling above the floor, as a fraction of the hardware span.
    cap_headroom: f32,
    /// Last floor target applied. Used by the idle reconcile pass.
    last_target: u32,
    /// Last time a ceiling write was issued for this cluster.
    last_cap_write: Option<Instant>,
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
    pub fn discover(profile: &DeviceProfile, ceiling_headroom: f32) -> Result<Self, String> {
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

            // The theoretical hardware ceiling is the reference for the cap.
            // A vendor thermal clamp is respected per write by clamping to the
            // live ceiling right before writing.
            let hw_max = hw_ceiling.max(live_max);
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
                cap_headroom: ceiling_headroom,
                last_target: 0,
                last_cap_write: None,
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

    /// Re-derives the soft ceiling from the last floor target without waiting
    /// for a scheduling event. Vendor perf services move floors while the
    /// screen is static; this keeps the cap honest at a slow cadence.
    pub fn reconcile(&mut self) -> Result<(), String> {
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
            let current_min =
                read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(policy.hw_min);
            let cap = soft_cap(policy, policy.last_target.min(live_max), live_max, current_min);
            if live_max == cap {
                continue;
            }
            if let Some(last) = policy.last_cap_write {
                if now.duration_since(last) < WRITE_SPACING {
                    continue;
                }
            }
            if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), cap) {
                failure.get_or_insert(problem);
                continue;
            }
            policy.last_cap_write = Some(now);
            self.applied += 1;
        }
        self.last_error = failure.clone();
        match failure {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Applies a live ceiling headroom change from a configuration reload.
    pub fn set_ceiling_headroom(&mut self, ceiling_headroom: f32) {
        for policy in &mut self.policies {
            policy.cap_headroom = ceiling_headroom;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Applies each decision as a **floor** on `scaling_min_freq` and a soft
    /// ceiling on `scaling_max_freq`. A live ceiling below the requested value
    /// (usually a vendor thermal clamp) is respected, never fought.
    pub fn apply(&mut self, decisions: &[Decision]) -> Result<(), String> {
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
            let quiet = matches!(
                decision.reason,
                DecisionReason::Quiet | DecisionReason::ScreenOff
            );
            // The quiet state is reconciled about once every few seconds, so a
            // vendor perf service that raised the floor or the ceiling is pulled
            // back without fighting it on every sample. A rejected floor write
            // must not block the ceiling from being re-derived: the two nodes
            // are reconciled independently.
            if quiet && !decision.changed {
                if policy
                    .last_reconcile
                    .is_some_and(|last| now.duration_since(last) < RECONCILE_INTERVAL)
                {
                    continue;
                }
                policy.last_reconcile = Some(now);
            }
            let live_max = read_u32(&policy.dir.join("scaling_max_freq")).unwrap_or(policy.hw_max);
            if live_max <= policy.hw_min {
                // Locked by a vendor range; there is nothing left to schedule.
                continue;
            }
            let mut target = decision.ceiling_khz.clamp(policy.hw_min, policy.hw_max);
            if let Some(step) = snap_down(&policy.freqs, target) {
                target = step.max(policy.hw_min);
            }
            // Never let the floor exceed the live ceiling.
            target = target.min(live_max);

            let current_min = read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(policy.hw_min);
            policy.last_target = target;
            // The soft ceiling starts above the live floor, otherwise the kernel
            // locks the cluster (min == max) and it can never ramp again.
            let cap = soft_cap(policy, target, live_max, current_min);

            // Raise the ceiling before the floor climbs, so a min write never
            // exceeds the live max and gets rejected by the kernel.
            if cap > live_max {
                if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), cap) {
                    failure.get_or_insert(problem);
                    continue;
                }
                policy.last_cap_write = Some(now);
                self.applied += 1;
            } else if live_max != cap {
                if let Some(last) = policy.last_cap_write {
                    if now.duration_since(last) < WRITE_SPACING {
                        continue;
                    }
                }
                if let Err(problem) = write_khz(&policy.dir.join("scaling_max_freq"), cap) {
                    failure.get_or_insert(problem);
                    continue;
                }
                policy.last_cap_write = Some(now);
                self.applied += 1;
                tracing::debug!(
                    cluster = policy.cluster,
                    target,
                    cap,
                    live_max,
                    "floor and ceiling applied"
                );
            }

            if current_min != target && !policy.retry_after.is_some_and(|retry| now < retry) {
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
                if let Err(problem) = write_khz(&policy.dir.join("scaling_min_freq"), target) {
                    failure.get_or_insert(problem);
                    continue;
                }
                let readback = read_u32(&policy.dir.join("scaling_min_freq")).unwrap_or(target);
                if readback > target {
                    // A vendor QoS request (perfservice's screen-on floor) rejected
                    // the write. Back off instead of fighting it.
                    policy.retry_after = Some(now + BLOCKED_RETRY);
                } else {
                    policy.retry_after = None;
                }
                policy.last_write = Some(now);
                policy.quiet_pending_since = None;
                self.applied += 1;
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

/// Soft ceiling for a cluster: enough room above the floor for a frame to
/// ramp, but never the whole hardware span.
fn soft_cap(policy: &Policy, target: u32, live_max: u32, current_min: u32) -> u32 {
    let span = policy.hw_max.saturating_sub(policy.hw_min);
    let ceiling_limit = if live_max <= current_min {
        policy.hw_max
    } else {
        live_max
    };
    let headroom = (span as f32 * policy.cap_headroom) as u32;
    let base = target.max(current_min).min(ceiling_limit);
    base.saturating_add(headroom)
        .clamp(policy.hw_min, ceiling_limit)
        .max(base)
}

/// Largest hardware step that does not exceed `value`.
fn snap_down(freqs: &[u32], value: u32) -> Option<u32> {
    freqs.iter().copied().rev().find(|step| *step <= value)
}

fn write_text(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, format!("{value}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}
