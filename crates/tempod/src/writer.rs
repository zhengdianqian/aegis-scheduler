// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! 执行控制器的每簇频率上下限，不修改 governor 或温控节点。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempo_core::controller::Decision;
use tempo_core::DeviceProfile;

pub const CPUFREQ_ROOT: &str = "/sys/devices/system/cpu/cpufreq";
const RECONCILE_INTERVAL: Duration = Duration::from_millis(500);

/// Stable readbacks stretch safety polling only; event-driven applies never wait.
pub(crate) struct ReconcileBackoff {
    seconds: u64,
}

impl ReconcileBackoff {
    pub(crate) fn new() -> Self {
        Self { seconds: 1 }
    }

    pub(crate) fn reset(&mut self) -> u64 {
        self.seconds = 1;
        self.seconds * 1_000_000
    }

    pub(crate) fn expedite(&mut self) -> bool {
        let stretched = self.seconds > 1;
        self.reset();
        stretched
    }

    pub(crate) fn observe(&mut self, screen_on: bool, disturbed: bool) -> u64 {
        if disturbed {
            return self.reset();
        }
        let maximum = if screen_on { 8 } else { 30 };
        self.seconds = (self.seconds * 2).min(maximum);
        self.seconds * 1_000_000
    }
}

struct Policy {
    cluster: usize,
    dir: PathBuf,
    pristine_min: u32,
    pristine_max: u32,
    original_governor: String,
    hw_min: u32,
    hw_max: u32,
    freqs: Vec<u32>,
    last_target: u32,
    last_cap: u32,
    idle_floor: bool,
    last_reconcile: Option<Instant>,
    /// Last verified range, used to detect another writer (including thermal).
    observed: Option<(u32, u32)>,
    pending_max: Option<u32>,
    /// An external ceiling persists across our own successful range changes.
    external_cap: Option<u32>,
    retries: u32,
    retry_at: Option<Instant>,
    error: Option<String>,
    /// Set before attempting a write: even a failed readback needs restoration.
    touched: bool,
}

pub struct CpufreqWriter {
    policies: Vec<Policy>,
    applied: u64,
    cap_writes: u64,
    floor_writes: u64,
    rejected_floor: u64,
    guard_until: Option<Instant>,
    suspended: bool,
    enabled: bool,
    pub last_error: Option<String>,
}

impl CpufreqWriter {
    pub fn discover(profile: &DeviceProfile) -> Result<Self, String> {
        let mut policies = Vec::with_capacity(profile.clusters.len());
        for (cluster, entry) in profile.clusters.iter().enumerate() {
            let cpu = *entry
                .cpus
                .first()
                .ok_or_else(|| format!("cluster {cluster} has no cpus"))?;
            let dir = policy_dir(cpu).ok_or_else(|| format!("no cpufreq policy for cpu {cpu}"))?;
            let live_min = read_required(&dir.join("scaling_min_freq"))?;
            let live_max = read_required(&dir.join("scaling_max_freq"))?;
            let original_governor = fs::read_to_string(dir.join("scaling_governor"))
                .map(|text| text.trim().to_owned())
                .unwrap_or_default();
            let hw_min = read_u32(&dir.join("cpuinfo_min_freq"))
                .filter(|v| *v > 0)
                .unwrap_or(live_min);
            let hw_max = read_u32(&dir.join("cpuinfo_max_freq"))
                .unwrap_or(live_max)
                .max(live_max);
            let mut freqs: Vec<u32> = fs::read_to_string(dir.join("scaling_available_frequencies"))
                .map(|text| {
                    text.split_whitespace()
                        .filter_map(|part| part.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            freqs.sort_unstable();
            freqs.dedup();
            policies.push(Policy {
                cluster,
                dir,
                pristine_min: live_min,
                pristine_max: live_max,
                original_governor,
                hw_min: hw_min.min(hw_max),
                hw_max,
                freqs,
                last_target: 0,
                last_cap: 0,
                idle_floor: false,
                last_reconcile: None,
                observed: None,
                pending_max: None,
                external_cap: None,
                retries: 0,
                retry_at: None,
                error: None,
                touched: false,
            });
        }
        Ok(Self {
            policies,
            applied: 0,
            cap_writes: 0,
            floor_writes: 0,
            rejected_floor: 0,
            guard_until: None,
            suspended: false,
            enabled: true,
            last_error: None,
        })
    }

    pub fn arm_guard(&mut self, window: Duration) {
        self.guard_until = Some(Instant::now() + window);
    }

    fn guarded(&self) -> bool {
        self.suspended
            || !self.enabled
            || self.guard_until.is_some_and(|until| Instant::now() < until)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn needs_initial_apply(&self) -> bool {
        !self.guarded() && self.policies.iter().any(|policy| policy.last_target == 0)
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    pub fn guard_active(&self) -> bool {
        self.guarded()
    }

    fn result(&mut self) -> Result<(), String> {
        self.last_error = self.policies.iter().find_map(|policy| policy.error.clone());
        self.last_error.clone().map_or(Ok(()), Err)
    }

    /// 周期校验两端；Ok(true) 表示此次读回观察到漂移（即使未写入）。
    /// 尊重外部上限，下限争写或拒绝写入暂时退避；读取/写入错误返回 Err。
    pub fn reconcile(&mut self) -> Result<bool, String> {
        if self.guarded() {
            return Ok(false);
        }
        let mut drifted = false;
        let now = Instant::now();
        for policy in &mut self.policies {
            if policy.last_target == 0
                || policy
                    .last_reconcile
                    .is_some_and(|last| now.duration_since(last) < RECONCILE_INTERVAL)
            {
                continue;
            }
            policy.last_reconcile = Some(now);
            drifted |= sync_policy(
                policy,
                now,
                &mut self.applied,
                &mut self.cap_writes,
                &mut self.floor_writes,
                &mut self.rejected_floor,
            );
        }
        self.result().map(|()| drifted)
    }

    /// ceiling_khz 是历史命名的运行下限，cap_khz 才是上限。
    pub fn apply(&mut self, decisions: &[Decision]) -> Result<(), String> {
        if self.guarded() {
            return Ok(());
        }
        for decision in decisions {
            let Some(policy) = self
                .policies
                .iter_mut()
                .find(|p| p.cluster == decision.cluster)
            else {
                continue;
            };
            let cap = decision.cap_khz.clamp(policy.hw_min, policy.hw_max);
            // 频点表可能省略最高档；解除限频时不能再按该表向下取整。
            let cap = if cap == policy.hw_max {
                cap
            } else {
                snap_down(&policy.freqs, cap).unwrap_or(cap).max(policy.hw_min)
            };
            if cap > policy.last_cap && policy.pending_max.is_none() {
                // A new higher cap gets one immediate attempt; repeated decisions
                // still respect any backoff caused by the kernel rejecting it.
                policy.retry_at = None;
            }
            policy.last_cap = cap;
            let floor = decision.ceiling_khz.clamp(policy.hw_min, policy.last_cap);
            policy.last_target = snap_down(&policy.freqs, floor)
                .unwrap_or(floor)
                .max(policy.hw_min);
            policy.idle_floor = decision.idle;
            sync_policy(
                policy,
                Instant::now(),
                &mut self.applied,
                &mut self.cap_writes,
                &mut self.floor_writes,
                &mut self.rejected_floor,
            );
        }
        self.result()
    }

    pub fn idle_changed(&self, decisions: &[Decision]) -> bool {
        !self.guarded()
            && self.policies.iter().any(|policy| {
                decisions.iter().any(|decision| {
                    decision.cluster == policy.cluster && decision.idle != policy.idle_floor
                })
            })
    }

    /// 恢复启动时的 min/max；不触碰 governor、温控开关或其他限制节点。
    pub fn restore(&mut self) -> Result<(), String> {
        self.suspended = true;
        let mut error = None;
        for policy in &mut self.policies {
            if !policy.touched {
                continue;
            }
            let result = set_range(
                &policy.dir,
                policy.pristine_min,
                policy.pristine_max,
                |path, value| write_verified(path, value),
            );
            match result {
                Ok(()) => {
                    policy.touched = false;
                    policy.error = None;
                }
                Err(problem) => {
                    policy.error = Some(problem.clone());
                    error.get_or_insert(problem);
                }
            }
        }
        self.guard_until = None;
        self.last_error = error.clone();
        if error.is_none() {
            self.applied = 0;
        }
        error.map_or(Ok(()), Err)
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
                    "cluster": policy.cluster, "dir": policy.dir.display().to_string(),
                    "min_freq": policy.pristine_min, "max_freq": policy.pristine_max,
                    "governor": policy.original_governor,
                })
            })
            .collect();
        serde_json::json!({ "version": 1, "policies": entries }).to_string()
    }
}

impl Drop for CpufreqWriter {
    fn drop(&mut self) {
        if self.policies.iter().any(|policy| policy.touched) {
            let _ = self.restore();
        }
    }
}

pub fn policy_dir(cpu: u32) -> Option<PathBuf> {
    let direct = Path::new(CPUFREQ_ROOT).join(format!("policy{cpu}"));
    if direct.join("scaling_max_freq").exists() {
        return Some(direct);
    }
    for entry in fs::read_dir(CPUFREQ_ROOT).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("scaling_max_freq").exists() {
            continue;
        }
        let Ok(related) = fs::read_to_string(path.join("related_cpus")) else {
            continue;
        };
        if related
            .split_whitespace()
            .filter_map(|part| part.parse::<u32>().ok())
            .any(|id| id == cpu)
        {
            return Some(path);
        }
    }
    None
}

pub fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_required(path: &Path) -> Result<u32, String> {
    read_u32(path)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("read {}: invalid or unavailable frequency", path.display()))
}

fn read_range(dir: &Path) -> Result<(u32, u32), String> {
    Ok((
        read_required(&dir.join("scaling_min_freq"))?,
        read_required(&dir.join("scaling_max_freq"))?,
    ))
}

/// Lower min before lowering max past it; otherwise raise max before raising min.
/// Verify each write before proceeding, so a clamp cannot lead to an unsafe second write.
fn set_range(
    dir: &Path,
    floor: u32,
    cap: u32,
    mut write: impl FnMut(&Path, u32) -> Result<(), String>,
) -> Result<(), String> {
    let min_path = dir.join("scaling_min_freq");
    let max_path = dir.join("scaling_max_freq");
    let (min, max) = read_range(dir)?;
    if min > cap {
        write(&min_path, floor)?;
    }
    if max != cap {
        write(&max_path, cap)?;
    }
    if read_required(&min_path)? != floor {
        write(&min_path, floor)?;
    }
    let actual = read_range(dir)?;
    if actual != (floor, cap) {
        return Err(format!(
            "{}: requested {floor}..{cap}, read back {}..{}",
            dir.display(),
            actual.0,
            actual.1
        ));
    }
    Ok(())
}

fn backoff(policy: &mut Policy, now: Instant, problem: String) {
    let wait = Duration::from_millis(500u64.saturating_mul(1 << policy.retries.min(4)).min(5000));
    policy.retries = policy.retries.saturating_add(1);
    policy.retry_at = Some(now + wait);
    policy.error = Some(problem);
}

fn sync_policy(
    policy: &mut Policy,
    now: Instant,
    applied: &mut u64,
    cap_writes: &mut u64,
    floor_writes: &mut u64,
    rejected_floor: &mut u64,
) -> bool {
    let live = match read_range(&policy.dir) {
        Ok(live) => live,
        Err(problem) => {
            if !policy.retry_at.is_some_and(|at| now < at) {
                backoff(policy, now, problem);
            }
            return false;
        }
    };
    // 内核可能异步应用上限：即时读回旧值，不代表后续变化来自外部。
    if policy.pending_max == Some(live.1) {
        policy.observed = Some((policy.observed.map_or(live.0, |range| range.0), live.1));
        policy.pending_max = None;
    }
    let externally_changed = policy.observed.is_some_and(|previous| previous != live);
    // Only a max different from our last successful range is external evidence.
    // In particular, our own lower cap must neither tighten nor release this latch.
    if let Some((_, previous_max)) = policy.observed {
        if live.1 != previous_max {
            if policy.external_cap.is_some_and(|cap| live.1 > cap) {
                policy.external_cap = (live.1 < policy.hw_max).then_some(live.1);
            } else if live.1 < previous_max {
                policy.external_cap = Some(policy.external_cap.unwrap_or(live.1).min(live.1));
            }
        }
    }
    let cap = policy
        .last_cap
        .min(policy.external_cap.unwrap_or(policy.last_cap));
    let desired = (policy.last_target.min(cap), cap);
    let constraint = policy
        .external_cap
        .map(|cap| format!("{}: external frequency cap {cap}", policy.dir.display()));
    if live == desired {
        policy.observed = Some(live);
        policy.error = constraint;
        policy.retry_at = None;
        policy.retries = 0;
        return externally_changed;
    }
    if policy.retry_at.is_some_and(|at| now < at) {
        return true;
    }
    if policy.retry_at.is_none() && policy.observed.is_some_and(|previous| previous != live) {
        backoff(
            policy,
            now,
            format!(
                "{}: external frequency change to {}..{}; retry pending",
                policy.dir.display(),
                live.0,
                live.1
            ),
        );
        return true;
    }
    let dir = policy.dir.clone();
    let result = set_range(&dir, desired.0, desired.1, |path, value| {
        policy.touched = true;
        write_khz(path, value)?;
        *applied += 1;
        let is_floor = path.ends_with("scaling_min_freq");
        if is_floor {
            *floor_writes += 1;
        } else {
            *cap_writes += 1;
            policy.pending_max = Some(value);
        }
        let actual = read_required(path)?;
        if actual != value {
            if is_floor {
                *rejected_floor += 1;
            }
            return Err(format!(
                "{}: requested {value}, read back {actual}",
                path.display()
            ));
        }
        if !is_floor {
            // 后续下限写入即使失败，也不能把已验证的自身上限误判为外部限制。
            policy.observed = Some((live.0, actual));
            policy.pending_max = None;
        }
        Ok(())
    });
    match result {
        Ok(()) => {
            policy.observed = Some(desired);
            policy.retry_at = None;
            policy.error = constraint;
            // Keep conflict history until a later read sees a stable desired range.
        }
        Err(problem) => backoff(policy, now, problem),
    }
    true
}

fn write_verified(path: &Path, value: u32) -> Result<(), String> {
    write_khz(path, value)?;
    let actual = read_required(path)?;
    if actual != value {
        return Err(format!(
            "{}: requested {value}, read back {actual}",
            path.display()
        ));
    }
    Ok(())
}

fn snap_down(freqs: &[u32], value: u32) -> Option<u32> {
    freqs.iter().copied().rev().find(|step| *step <= value)
}

fn write_khz(path: &Path, value: u32) -> Result<(), String> {
    write_text(path, &value.to_string())
}

fn write_text(path: &Path, value: &str) -> Result<(), String> {
    fs::write(path, format!("{value}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempo_core::controller::DecisionReason;

    struct Sysfs(PathBuf);
    impl Sysfs {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aegis-writer-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            write_khz(&dir.join("scaling_min_freq"), 100).unwrap();
            write_khz(&dir.join("scaling_max_freq"), 1000).unwrap();
            Self(dir)
        }
        fn writer(&self) -> CpufreqWriter {
            CpufreqWriter {
                policies: vec![Policy {
                    cluster: 0,
                    dir: self.0.clone(),
                    pristine_min: 100,
                    pristine_max: 1000,
                    original_governor: "schedutil".into(),
                    hw_min: 100,
                    hw_max: 1000,
                    freqs: vec![100, 300, 500, 700, 1000],
                    last_target: 0,
                    last_cap: 0,
                    idle_floor: false,
                    last_reconcile: None,
                    observed: None,
                    pending_max: None,
                    external_cap: None,
                    retries: 0,
                    retry_at: None,
                    error: None,
                    touched: false,
                }],
                applied: 0,
                cap_writes: 0,
                floor_writes: 0,
                rejected_floor: 0,
                guard_until: None,
                suspended: false,
                enabled: true,
                last_error: None,
            }
        }
        fn range(&self) -> (u32, u32) {
            read_range(&self.0).unwrap()
        }
    }
    impl Drop for Sysfs {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn decision(floor: u32, cap: u32, idle: bool) -> Decision {
        Decision {
            cluster: 0,
            ceiling_khz: floor,
            cap_khz: cap,
            tier: 0,
            tier_count: 5,
            reason: DecisionReason::Launch,
            changed: true,
            idle,
        }
    }
    fn sync_at(writer: &mut CpufreqWriter, now: Instant) {
        sync_policy(
            &mut writer.policies[0],
            now,
            &mut writer.applied,
            &mut writer.cap_writes,
            &mut writer.floor_writes,
            &mut writer.rejected_floor,
        );
        let _ = writer.result();
    }

    #[test]
    fn stable_reconcile_backoff_is_bounded_and_disturbance_resets_immediately() {
        let mut backoff = ReconcileBackoff::new();
        assert_eq!(backoff.reset(), 1_000_000);
        for seconds in [2, 4, 8, 8, 8] {
            assert_eq!(backoff.observe(true, false), seconds * 1_000_000);
        }
        for seconds in [16, 30, 30] {
            assert_eq!(backoff.observe(false, false), seconds * 1_000_000);
        }
        assert_eq!(backoff.observe(true, false), 8_000_000);
        assert_eq!(backoff.observe(false, true), 1_000_000);
        assert_eq!(backoff.observe(true, true), 1_000_000);
        assert_eq!(backoff.observe(true, false), 2_000_000);
        assert!(backoff.expedite());
        assert!(!backoff.expedite());
        assert_eq!(backoff.observe(true, false), 2_000_000);
        assert_eq!(backoff.reset(), 1_000_000);
    }

    #[test]
    fn stable_poll_backoff_does_not_delay_event_driven_apply() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(100, 300, true)]).unwrap();
        let mut backoff = ReconcileBackoff::new();
        for _ in 0..6 {
            backoff.observe(false, writer.reconcile().unwrap());
        }
        assert_eq!(backoff.seconds, 30);
        writer.apply(&[decision(700, 1000, false)]).unwrap();
        assert_eq!(sysfs.range(), (700, 1000));
        writer.apply(&[decision(100, 300, true)]).unwrap();
        assert_eq!(sysfs.range(), (100, 300));
    }

    #[test]
    fn reconcile_reports_current_drift_not_cumulative_writes() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        assert!(writer.applied() > 0);
        assert!(!writer.reconcile().unwrap());
        let before = writer.applied();
        write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
        // Simulate an unverified initial range: reconcile repairs it immediately.
        writer.policies[0].observed = None;
        writer.policies[0].last_reconcile = None;
        assert!(writer.reconcile().unwrap());
        assert!(writer.applied() > before);
        writer.policies[0].last_reconcile = None;
        assert!(!writer.reconcile().unwrap());
    }

    #[test]
    fn reconcile_conflict_and_read_error_reset_backoff_without_writes() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        let before = writer.applied();
        write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
        let mut backoff = ReconcileBackoff::new();
        backoff.observe(false, false);
        assert!(writer.reconcile().is_err());
        writer.policies[0].last_reconcile = None;
        assert_eq!(
            backoff.observe(false, writer.reconcile().unwrap_or(true)),
            1_000_000
        );
        assert_eq!(writer.applied(), before);
        fs::remove_file(sysfs.0.join("scaling_max_freq")).unwrap();
        writer.policies[0].last_reconcile = None;
        backoff.observe(false, false);
        assert_eq!(
            backoff.observe(false, writer.reconcile().unwrap_or(true)),
            1_000_000
        );
        assert_eq!(writer.applied(), before);
        write_khz(&sysfs.0.join("scaling_max_freq"), 700).unwrap();
    }

    #[test]
    fn user_cap_release_uses_hardware_max_even_when_missing_from_table() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.policies[0].freqs.retain(|&khz| khz < 1000);
        writer.apply(&[decision(100, 400, false)]).unwrap();
        assert!(sysfs.range().1 <= 400);
        writer.apply(&[decision(100, 1000, false)]).unwrap();
        assert_eq!(sysfs.range(), (100, 1000));
    }

    #[test]
    fn delayed_own_cap_does_not_block_cooldown_release() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 1000, false)]).unwrap();
        writer.policies[0].pending_max = Some(700);
        write_khz(&sysfs.0.join("scaling_max_freq"), 700).unwrap();
        writer.reconcile().unwrap();
        assert_eq!(sysfs.range(), (500, 1000));
        assert_eq!(writer.policies[0].external_cap, None);
    }

    #[test]
    fn reconcile_detects_external_release_even_when_already_at_target() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 1000, false)]).unwrap();
        writer.policies[0].observed = Some((500, 600));
        writer.policies[0].external_cap = Some(600);
        let before = writer.applied();
        assert!(writer.reconcile().unwrap());
        assert_eq!(writer.applied(), before);
        writer.policies[0].last_reconcile = None;
        assert!(!writer.reconcile().unwrap());
    }

    #[test]
    fn active_and_idle_both_apply_floor_and_cap() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        for (floor, cap, idle) in [(500, 700, false), (100, 300, true), (700, 1000, false)] {
            writer.apply(&[decision(floor, cap, idle)]).unwrap();
            assert_eq!(sysfs.range(), (floor, cap));
        }
        assert!(writer.cap_writes() > 0);
        assert!(writer.floor_writes() > 0);
    }

    #[test]
    fn snaps_and_clamps_both_limits_to_hardware() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(650, 850, false)]).unwrap();
        assert_eq!(sysfs.range(), (500, 700));
        writer.apply(&[decision(1500, 50, false)]).unwrap();
        assert_eq!(sysfs.range(), (100, 100));
    }

    #[test]
    fn range_transition_order_is_safe_in_both_directions() {
        let sysfs = Sysfs::new();
        for (floor, cap, expected) in [
            (700, 1000, vec!["scaling_min_freq"]),
            (100, 300, vec!["scaling_min_freq", "scaling_max_freq"]),
            (700, 1000, vec!["scaling_max_freq", "scaling_min_freq"]),
        ] {
            let mut writes = Vec::new();
            set_range(&sysfs.0, floor, cap, |path, value| {
                let live = sysfs.range();
                if path.ends_with("scaling_min_freq") {
                    assert!(value <= live.1);
                } else {
                    assert!(value >= live.0);
                }
                writes.push(path.file_name().unwrap().to_str().unwrap().to_owned());
                write_verified(path, value)
            })
            .unwrap();
            assert_eq!(writes, expected);
        }
    }

    #[test]
    fn readback_failure_stops_before_second_write() {
        let sysfs = Sysfs::new();
        write_khz(&sysfs.0.join("scaling_min_freq"), 700).unwrap();
        let mut writes = 0;
        assert!(set_range(&sysfs.0, 100, 300, |_, _| {
            writes += 1;
            Err("thermal clamp".into())
        })
        .is_err());
        assert_eq!(writes, 1);
        assert_eq!(sysfs.range(), (700, 1000));
    }

    #[test]
    fn thermal_cap_readback_rejection_does_not_raise_floor() {
        let sysfs = Sysfs::new();
        write_khz(&sysfs.0.join("scaling_max_freq"), 300).unwrap();
        let mut writes = Vec::new();
        let result = set_range(&sysfs.0, 700, 1000, |path, value| {
            writes.push(path.file_name().unwrap().to_str().unwrap().to_owned());
            // Simulate a driver that accepts the syscall but clamps its readback.
            write_khz(path, value.min(300))?;
            let actual = read_required(path)?;
            if actual != value {
                return Err(format!("thermal cap: {actual} != {value}"));
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(writes, vec!["scaling_max_freq"]);
        assert_eq!(sysfs.range(), (100, 300));
    }

    #[test]
    fn external_floor_override_backs_off_but_always_retries() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        let mut now = Instant::now();
        for _ in 0..12 {
            write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
            let before = writer.applied();
            sync_at(&mut writer, now);
            assert!(writer.last_error.is_some());
            let retry = writer.policies[0].retry_at.unwrap();
            assert!(retry.duration_since(now) <= Duration::from_secs(5));
            sync_at(&mut writer, retry - Duration::from_millis(1));
            assert_eq!(writer.applied(), before);
            sync_at(&mut writer, retry);
            assert_eq!(sysfs.range(), (500, 700));
            assert!(writer.last_error.is_none());
            now = retry + Duration::from_millis(1);
        }
    }

    #[test]
    fn higher_launch_cap_preserves_external_floor_diagnostics() {
        for pending_backoff in [false, true] {
            let sysfs = Sysfs::new();
            let mut writer = sysfs.writer();
            writer.apply(&[decision(300, 300, true)]).unwrap();
            write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
            if pending_backoff {
                assert!(writer.apply(&[decision(300, 300, true)]).is_err());
                assert!(writer.policies[0].retry_at.is_some());
            }
            assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
            let retry = writer.policies[0].retry_at.unwrap();
            sync_at(&mut writer, retry);
            assert_eq!(sysfs.range(), (700, 1000));
            assert!(writer.policies[0].retry_at.is_none());
            assert!(writer.last_error.is_none());
        }
    }

    #[test]
    fn repeated_same_cap_does_not_bypass_external_override_backoff() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
        write_khz(&sysfs.0.join("scaling_max_freq"), 300).unwrap();
        assert!(writer.apply(&[decision(500, 700, false)]).is_err());
        let before = writer.applied();
        // Keep the test independent of wall-clock pauses between apply calls.
        let retry = Instant::now() + Duration::from_secs(60);
        writer.policies[0].retry_at = Some(retry);
        let retries = writer.policies[0].retries;
        for floor in [500, 300, 500] {
            assert!(writer.apply(&[decision(floor, 700, false)]).is_err());
            assert_eq!(writer.policies[0].retry_at, Some(retry));
            assert_eq!(writer.policies[0].retries, retries);
            assert_eq!(writer.applied(), before);
            assert_eq!(sysfs.range(), (100, 300));
        }
    }

    #[test]
    fn external_cap_survives_retries_and_our_own_lower_ranges_until_release() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        write_khz(&sysfs.0.join("scaling_min_freq"), 100).unwrap();
        write_khz(&sysfs.0.join("scaling_max_freq"), 300).unwrap();
        assert!(writer.apply(&[decision(500, 700, false)]).is_err());
        let retry = writer.policies[0].retry_at.unwrap();
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (300, 300));
        for (floor, cap) in [(700, 1000), (100, 100), (500, 700), (700, 1000)] {
            assert!(writer.apply(&[decision(floor, cap, false)]).is_err());
            assert!(sysfs.range().0 <= 300);
            assert!(sysfs.range().1 <= 300);
            assert_eq!(writer.policies[0].external_cap, Some(300));
            assert!(writer
                .last_error
                .as_ref()
                .unwrap()
                .contains("external frequency cap 300"));
        }
        // Partial external release permits only the newly observed ceiling.
        write_khz(&sysfs.0.join("scaling_max_freq"), 700).unwrap();
        assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
        let retry = writer.policies[0].retry_at.unwrap();
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (700, 700));
        assert_eq!(writer.policies[0].external_cap, Some(700));
        write_khz(&sysfs.0.join("scaling_max_freq"), 1000).unwrap();
        writer.apply(&[decision(700, 1000, false)]).unwrap();
        assert_eq!(sysfs.range(), (700, 1000));
        assert_eq!(writer.policies[0].external_cap, None);
        assert!(writer.last_error.is_none());
    }

    #[test]
    fn thermal_max_first_below_floor_and_hardware_min_is_respected() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(700, 1000, false)]).unwrap();
        // Thermal can update max before the old min has caught up.
        write_khz(&sysfs.0.join("scaling_max_freq"), 50).unwrap();
        assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
        let retry = writer.policies[0].retry_at.unwrap();
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (50, 50));
        for _ in 0..3 {
            assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
            assert_eq!(sysfs.range(), (50, 50));
        }
    }

    #[test]
    fn failed_higher_cap_attempt_keeps_backoff_for_repeated_target() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(100, 300, true)]).unwrap();
        let max = sysfs.0.join("scaling_max_freq");
        fs::remove_file(&max).unwrap();
        assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
        assert!(writer.policies[0].retry_at.is_some());
        let retries = writer.policies[0].retries;
        let before = writer.applied();
        let retry = Instant::now() + Duration::from_secs(60);
        writer.policies[0].retry_at = Some(retry);
        write_khz(&max, 300).unwrap();
        for _ in 0..3 {
            assert!(writer.apply(&[decision(700, 1000, false)]).is_err());
            assert_eq!(writer.policies[0].retry_at, Some(retry));
            assert_eq!(writer.policies[0].retries, retries);
            assert_eq!(writer.applied(), before);
            assert_eq!(sysfs.range(), (100, 300));
        }
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (700, 1000));
        assert!(writer.last_error.is_none());
    }

    #[test]
    fn io_failure_remains_visible_during_backoff_then_recovers() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        let max = sysfs.0.join("scaling_max_freq");
        fs::remove_file(&max).unwrap();
        assert!(writer.apply(&[decision(500, 700, false)]).is_err());
        assert!(writer.apply(&[decision(500, 700, false)]).is_err());
        write_khz(&max, 1000).unwrap();
        let retry = writer.policies[0].retry_at.unwrap();
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (500, 700));
        assert!(writer.last_error.is_none());
    }

    #[test]
    fn restore_original_limits_and_permanently_suspend() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.policies[0].pristine_min = 300;
        writer.policies[0].pristine_max = 700;
        writer.apply(&[decision(100, 100, true)]).unwrap();
        writer.restore().unwrap();
        assert_eq!(sysfs.range(), (300, 700));
        writer.set_enabled(true);
        writer.apply(&[decision(700, 1000, false)]).unwrap();
        writer.reconcile().unwrap();
        assert!(!writer.needs_initial_apply());
        assert_eq!(sysfs.range(), (300, 700));
    }

    #[test]
    fn guard_and_disable_leave_all_nodes_untouched() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.arm_guard(Duration::from_secs(60));
        writer.apply(&[decision(500, 700, false)]).unwrap();
        assert!(!writer.needs_initial_apply());
        writer.guard_until = None;
        writer.set_enabled(false);
        writer.apply(&[decision(500, 700, false)]).unwrap();
        writer.reconcile().unwrap();
        assert_eq!(sysfs.range(), (100, 1000));
        writer.set_enabled(true);
        assert!(writer.needs_initial_apply());
    }

    #[test]
    fn failed_restore_is_visible_and_drop_retries_both_limits() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        let min = sysfs.0.join("scaling_min_freq");
        fs::remove_file(&min).unwrap();
        assert!(writer.restore().is_err());
        assert!(writer.last_error.is_some());
        write_khz(&min, 500).unwrap();
        drop(writer);
        assert_eq!(sysfs.range(), (100, 1000));
    }
}
