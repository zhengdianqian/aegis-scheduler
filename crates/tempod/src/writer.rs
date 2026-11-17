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
    /// AegisAuto：外部把下限抬到我们的目标之上时读回被拒，锁存（地板值,
    /// 拒绝时刻）并让位。时效 2s：瞬时 boost 地板过期即重新下探；持久地板靠
    /// 新拒绝刷新；目标回升超过地板或外部主动下调时立即跟随/解除。
    external_floor: Option<(u32, Instant)>,
    retries: u32,
    retry_at: Option<Instant>,
    error: Option<String>,
    /// Set before attempting a write: even a failed readback needs restoration.
    touched: bool,
    /// AegisAuto 接管方式：`SetSpeed` = userspace governor + scaling_setspeed；
    /// `Pin` = 内核无 userspace 时的等效接管（min=max=目标频率）。
    /// `None` = Standard 模式（或未接管），走原有的上下限约束路径。
    auto: Option<AutoPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPath {
    SetSpeed,
    Pin,
}

impl AutoPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetSpeed => "userspace",
            Self::Pin => "pin",
        }
    }
}

/// 紧急回退所需的原始状态快照（每个策略目录一条）。
#[derive(Debug, Clone)]
pub struct EmergencyPolicy {
    pub dir: PathBuf,
    pub pristine_min: u32,
    pub pristine_max: u32,
    pub original_governor: String,
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
    /// 设备温度低于 `thermal.guard_c` 时为 false：外部（云控/性能服务）写下的
    /// 频率上限不再被永久让位，本模块按退避节奏重新夺回自己的上限；温度达到
    /// 保护线时为 true，恢复“尊重外部上限”，把温控优先权交还给系统。
    respect_external: bool,
    /// 配置选择了 AegisAuto 模式。
    auto_mode: bool,
    /// AegisAuto + 息屏：接管挂起，任何写入都不发生（区间交还内核）。
    standby: bool,
    /// 目标与当前频率差值在该比例以内不写（AegisAuto 防抖死区）。
    deadband: f32,
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
                external_floor: None,
                retries: 0,
                retry_at: None,
                error: None,
                touched: false,
                auto: None,
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
            respect_external: true,
            auto_mode: false,
            standby: false,
            deadband: 0.10,
            last_error: None,
        })
    }

    /// 紧急回退快照：原始 min/max 与原始 governor，panic/看门狗/信号处理共用。
    pub fn emergency_policies(&self) -> Vec<EmergencyPolicy> {
        self.policies
            .iter()
            .map(|policy| EmergencyPolicy {
                dir: policy.dir.clone(),
                pristine_min: policy.pristine_min,
                pristine_max: policy.pristine_max,
                original_governor: policy.original_governor.clone(),
            })
            .collect()
    }

    /// 模式与屏幕状态变化时调用。
    ///
    /// * `mode_auto=false`：Standard，若已接管则立即恢复原始区间/governor。
    /// * `mode_auto=true` 且息屏：挂起接管（写恢复原样，区间交还内核）。
    /// * `mode_auto=true` 且亮屏：由下一次 `apply` 惰性接管。
    pub fn set_auto(&mut self, mode_auto: bool, screen_on: bool, deadband: f32) {
        self.auto_mode = mode_auto;
        self.deadband = deadband.clamp(0.0, 0.30);
        self.standby = mode_auto && !screen_on;
        if !(mode_auto && screen_on) {
            self.disengage_auto();
        }
    }

    /// 当前 AegisAuto 接管方式（全部策略的最短路径；未接管为 None）。
    pub fn auto_path(&self) -> Option<AutoPath> {
        if !self.auto_mode || self.standby {
            return None;
        }
        self.policies.iter().filter_map(|p| p.auto).min_by_key(|p| match p {
            AutoPath::Pin => 0,
            AutoPath::SetSpeed => 1,
        })
    }

    /// 尝试把每个策略切到 userspace governor；内核不提供时退化为 Pin。
    /// 只在亮屏、非保护窗口、尚未接管时执行。
    fn engage_auto(&mut self) {
        for policy in &mut self.policies {
            if policy.auto.is_some() {
                continue;
            }
            let supports_userspace = fs::read_to_string(policy.dir.join("scaling_available_governors"))
                .map(|text| {
                    text.split_whitespace().any(|word| word == "userspace")
                })
                .unwrap_or(false)
                && policy.dir.join("scaling_setspeed").exists();
            let path = if supports_userspace && governor_write(&policy.dir, "userspace") {
                AutoPath::SetSpeed
            } else {
                AutoPath::Pin
            };
            tracing::info!(
                cluster = policy.cluster,
                path = path.as_str(),
                "AegisAuto takeover engaged"
            );
            policy.auto = Some(path);
        }
    }

    /// 解除接管：恢复原始区间，再把 governor 写回备份值（userspace 必须回退）。
    fn disengage_auto(&mut self) {
        let mut error = None;
        let mut writes = 0u64;
        for policy in &mut self.policies {
            if policy.auto.is_none() && !governor_changed(policy) {
                continue;
            }
            if let Err(problem) = restore_policy(policy, &mut writes) {
                error.get_or_insert(problem);
            }
            policy.auto = None;
            policy.last_target = 0;
            policy.last_cap = 0;
            policy.observed = None;
            policy.pending_max = None;
            policy.retry_at = None;
            policy.retries = 0;
        }
        self.applied += writes;
        if let Some(problem) = error {
            tracing::warn!(%problem, "AegisAuto takeover released with errors");
        }
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

    /// 见 `respect_external` 字段说明；由事件循环按当前温度调用。
    pub fn set_respect_external(&mut self, respect: bool) {
        self.respect_external = respect;
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
        // AegisAuto 息屏挂起：没有我们写入的状态需要校验。
        if self.auto_mode && self.standby {
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
            if policy.auto == Some(AutoPath::SetSpeed) {
                // setspeed 路径不碰 min/max；只校验 governor 仍为 userspace。
                // 被厂商改回原 governor 时按既有退避节奏重新接管。
                if policy.retry_at.is_some_and(|at| now < at) {
                    continue;
                }
                if read_governor(&policy.dir).as_deref() != Some("userspace") {
                    if governor_write(&policy.dir, "userspace") {
                        policy.error = None;
                        policy.retry_at = None;
                        policy.retries = 0;
                    } else {
                        backoff(
                            policy,
                            now,
                            format!(
                                "{}: external governor change; userspace rewrite pending",
                                policy.dir.display()
                            ),
                        );
                    }
                    drifted = true;
                } else {
                    policy.error = None;
                    policy.retry_at = None;
                    policy.retries = 0;
                }
                continue;
            }
            drifted |= sync_policy(
                policy,
                now,
                self.respect_external,
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
        // AegisAuto 息屏挂起：区间已交还内核，本守护不写任何节点。
        if self.auto_mode && self.standby {
            return Ok(());
        }
        if self.auto_mode {
            self.engage_auto();
        }
        let respect_external = self.respect_external;
        for decision in decisions {
            let Some(policy) = self
                .policies
                .iter_mut()
                .find(|p| p.cluster == decision.cluster)
            else {
                continue;
            };
            if let Some(path) = policy.auto {
                apply_auto(
                    policy,
                    decision,
                    path,
                    self.deadband,
                    respect_external,
                    &mut self.applied,
                    &mut self.cap_writes,
                    &mut self.floor_writes,
                    &mut self.rejected_floor,
                );
                continue;
            }
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
                self.respect_external,
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

    /// 恢复启动时的 min/max 并把 governor 写回备份值；不触碰温控开关或其他限制节点。
    ///
    /// 外部下限争写（powerkeeper 类）可能让单次恢复的某个节点读回失败；
    /// 失败的节点最多再试 2 轮（间隔 200ms），成功即退出，仍失败才上报错误
    /// （由 shell 看门狗继续兜底重试）。
    pub fn restore(&mut self) -> Result<(), String> {
        self.suspended = true;
        let mut writes = 0u64;
        let mut error;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut failed = false;
            let mut pass_error: Option<String> = None;
            for policy in &mut self.policies {
                if !policy.touched && !governor_changed(policy) {
                    continue;
                }
                match restore_policy(policy, &mut writes) {
                    Ok(()) => {
                        policy.touched = false;
                        policy.error = None;
                        policy.auto = None;
                    }
                    Err(problem) => {
                        policy.error = Some(problem.clone());
                        pass_error.get_or_insert(problem);
                        failed = true;
                    }
                }
            }
            if !failed || attempt >= 3 {
                error = pass_error;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.applied += writes;
        self.guard_until = None;
        self.last_error = error.clone();
        if error.is_none() {
            // 恢复成功不计作调度写入；接管状态与目标一并清掉。
            self.applied = 0;
            for policy in &mut self.policies {
                policy.last_target = 0;
                policy.last_cap = 0;
                policy.observed = None;
                policy.pending_max = None;
            }
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
    // AegisAuto 锁频必须同时压住 min 与 max：外部（场景/省电档案）每隔数百毫秒
    // 改写一侧时，指数退避会让内核在数秒内重获自由。接管态固定 500ms 重夺，
    // 有界且更快收敛；Standard 模式保持原有指数退避，避免与厂商高频互写。
    let wait = if policy.auto.is_some() {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(500u64.saturating_mul(1 << policy.retries.min(4)).min(5000))
    };
    policy.retries = policy.retries.saturating_add(1);
    policy.retry_at = Some(now + wait);
    policy.error = Some(problem);
}

fn sync_policy(
    policy: &mut Policy,
    now: Instant,
    respect_external: bool,
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
    // 外部地板锁存的跟随与释放：外部主动下调地板时锁存值下移；已观察到的
    // 下限不再高于我们的目标时锁存立即解除（含我们自己锁在目标上的状态）。
    if let Some((floor, at)) = policy.external_floor {
        if live.0 <= policy.last_target {
            policy.external_floor = None;
        } else if live.0 < floor {
            policy.external_floor = Some((live.0, at));
        }
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
    // 温度保护线下不再向外部上限让位：本模块的目标上限直接生效，外部改写会
    // 走下面既有的退避重试路径，形成有上限的低频争抢，而不是永久放弃。
    let cap = if respect_external {
        policy.last_cap.min(policy.external_cap.unwrap_or(policy.last_cap))
    } else {
        policy.last_cap
    };
    // 地板让位的时效与自愈：只在最近 2s 内发生过拒绝时生效（瞬时 boost 地板
    // 过期即重新下探）；目标回升超过地板时锁存立即失效。
    if let Some((floor, at)) = policy.external_floor {
        if floor <= policy.last_target || now.saturating_duration_since(at) > Duration::from_secs(2)
        {
            policy.external_floor = None;
        }
    }
    let desired = if matches!(policy.auto, Some(AutoPath::Pin)) {
        // AegisAuto 锁频：目标低于外部锁存的地板（powerkeeper 等原生守护的
        // 下限）时把锁频值抬到地板——该地板无论如何都会生效，锁在地板值上
        // 比放任内核在其区间内自由运行更省；热机仍受外部上限约束。
        let mut x = policy
            .last_target
            .max(policy.external_floor.map_or(0, |(floor, _)| floor));
        if respect_external {
            x = x.min(policy.external_cap.unwrap_or(u32::MAX));
        }
        x = x.clamp(policy.hw_min, policy.hw_max);
        (x, x)
    } else {
        (policy.last_target.min(cap), cap)
    };
    let constraint = (respect_external && policy.external_cap.is_some()).then(|| {
        format!(
            "{}: external frequency cap {}",
            policy.dir.display(),
            policy.external_cap.unwrap_or_default()
        )
    });
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
                // 外部把下限抬到我们的目标之上：锁存其地板值与拒绝时刻，锁频
                // 让位到地板，结束“永远打不赢”的下限争写；2s 时效保证瞬时
                // boost 地板不会把锁频长期钉在高频。
                if policy.auto.is_some() && actual > value {
                    policy.external_floor = Some((actual, now));
                }
            }
            return Err(format!(
                "{}: requested {value}, read back {actual}",
                path.display()
            ));
        }
        if is_floor {
            if let Some((floor, _)) = policy.external_floor {
                if value >= floor {
                    policy.external_floor = None;
                }
            }
        } else {
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

fn read_governor(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("scaling_governor"))
        .ok()
        .map(|text| text.trim().to_owned())
}

/// 写并读回校验 `scaling_governor`；值相同视为成功（不重复写）。
fn governor_write(dir: &Path, name: &str) -> bool {
    if read_governor(dir).as_deref() == Some(name) {
        return true;
    }
    if fs::write(dir.join("scaling_governor"), format!("{name}\n")).is_err() {
        return false;
    }
    read_governor(dir).as_deref() == Some(name)
}

/// 当前 governor 是否已经偏离启动时的备份值（未备份视为未偏离）。
fn governor_changed(policy: &Policy) -> bool {
    if policy.original_governor.is_empty() {
        return false;
    }
    read_governor(&policy.dir).as_deref() != Some(policy.original_governor.as_str())
}

fn write_setspeed(path: &Path, value: u32) -> Result<(), String> {
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

/// AegisAuto 写入路径。目标频率 = 决策目标（已含温控/静态限顶/自愿省电上限）。
///
/// * ±`deadband` 死区内保持不写（双向），内核不再兜底时压制频率来回跳动。
/// * `Pin`：min=max=目标频率，walt/schedutil 彻底无权变频；始终走既有
///   `sync_policy` 机械，外部（温控/云控）改写会按退避被重新锁回。
/// * `SetSpeed`：直写 `scaling_setspeed`，带读回校验与退避；不碰 min/max。
#[allow(clippy::too_many_arguments)]
fn apply_auto(
    policy: &mut Policy,
    decision: &Decision,
    path: AutoPath,
    deadband: f32,
    respect_external: bool,
    applied: &mut u64,
    cap_writes: &mut u64,
    floor_writes: &mut u64,
    rejected_floor: &mut u64,
) {
    let mut target = decision.ceiling_khz.clamp(policy.hw_min, policy.hw_max);
    if target != policy.hw_max {
        target = snap_down(&policy.freqs, target).unwrap_or(target).max(policy.hw_min);
    }
    policy.idle_floor = decision.idle;
    let now = Instant::now();
    let reference = policy.last_target;
    // 死区只压“降向”：AegisAuto 下内核不再兜底，升频方向若也挡在 ±deadband
    // 里会造成欠频卡顿（4.2 流畅度回退的来源之一）；升频立即写入，防抖交给
    // 控制器的降频迟滞（auto_down_hold_ms）与降档步进。
    let within_deadband = reference != 0
        && target < reference
        && f64::from(reference - target) <= f64::from(reference) * f64::from(deadband);

    match path {
        AutoPath::Pin => {
            if !within_deadband {
                if target > policy.last_cap && policy.pending_max.is_none() {
                    policy.retry_at = None;
                }
                policy.last_target = target;
                policy.last_cap = target;
            }
            sync_policy(
                policy,
                now,
                respect_external,
                applied,
                cap_writes,
                floor_writes,
                rejected_floor,
            );
        }
        AutoPath::SetSpeed => {
            if within_deadband || policy.retry_at.is_some_and(|at| now < at) {
                return;
            }
            match write_setspeed(&policy.dir.join("scaling_setspeed"), target) {
                Ok(()) => {
                    policy.touched = true;
                    policy.last_target = target;
                    policy.last_cap = target;
                    *applied += 1;
                    policy.error = None;
                    policy.retry_at = None;
                    policy.retries = 0;
                }
                Err(problem) => backoff(policy, now, problem),
            }
        }
    }
}

/// 恢复单个策略到启动备份：先恢复原始 min/max（解除锁频），再把 governor
/// 写回备份值；governor 未能恢复时把 scaling_setspeed 拉回可用频率兜底。
fn restore_policy(policy: &mut Policy, writes: &mut u64) -> Result<(), String> {
    let result = restore_dir(
        &policy.dir,
        policy.pristine_min,
        policy.pristine_max,
        &policy.original_governor,
    );
    match result {
        Ok(count) => {
            *writes += count;
            policy.touched = false;
            Ok(())
        }
        Err(problem) => Err(problem),
    }
}

/// 把一个策略目录恢复到备份状态。**逐节点独立尝试**（顺序：先开 max、再还原
/// min、最后回写 governor），任何单点失败都不阻塞其余节点——息屏瞬间厂商
/// 温控/省电档案常把 max 压到备份值以下，读回小于备份 max 属于更保守的外部
/// 上限，视为可接受；governor 回写被拒且仍停留在 userspace 时，把
/// `scaling_setspeed` 拉回备份的可用频率兜底。返回实际写入的节点数。
fn restore_dir(dir: &Path, pristine_min: u32, pristine_max: u32, original_governor: &str) -> Result<u64, String> {
    let mut count = 0u64;
    let mut error: Option<String> = None;
    let min_path = dir.join("scaling_min_freq");
    let max_path = dir.join("scaling_max_freq");

    // 1) 先放开上限：只有高于备份值才算失败（低于等于备份值均为安全方向）。
    match read_required(&max_path) {
        Ok(live) if live == pristine_max => {}
        Ok(live) if live > pristine_max => {
            // 外部把 max 抬到开机备份之上：仍按备份写回并严格校验。
            match write_verified(&max_path, pristine_max) {
                Ok(()) => count += 1,
                Err(problem) => {
                    error.get_or_insert(problem);
                }
            }
        }
        Ok(_) => {
            // 外部（多为息屏省电档案）把 max 压得更低：尝试写回备份值，
            // 读回仍低于备份也接受——上限不会高于原始值，方向安全。
            if fs::write(&max_path, format!("{pristine_max}\n")).is_ok() {
                count += 1;
            }
            if let Ok(live) = read_required(&max_path) {
                if live > pristine_max {
                    error.get_or_insert_with(|| {
                        format!("{}: max restore read back {live}", max_path.display())
                    });
                }
            }
        }
        Err(problem) => {
            error.get_or_insert(problem);
        }
    }

    // 2) 释放下限（解除锁频）：必须精确回到备份值，失败交给 shell 看门狗重试。
    match write_verified(&min_path, pristine_min) {
        Ok(()) => count += 1,
        Err(problem) => {
            // 备份值已生效时（值相同不写也会成功）不会走到这里；仅真实失败记录。
            error.get_or_insert(problem);
        }
    }

    // 3) 最后回写 governor（区间已恢复，接管态的 userspace 必须退出）。
    if !original_governor.is_empty() && read_governor(dir).as_deref() != Some(original_governor) {
        if governor_write(dir, original_governor) {
            tracing::info!(dir = %dir.display(), governor = original_governor, "scaling_governor restored");
            count += 1;
        } else {
            if read_governor(dir).as_deref() == Some("userspace") {
                let _ = write_setspeed(&dir.join("scaling_setspeed"), pristine_max);
                count += 1;
            }
            error.get_or_insert_with(|| format!("{}: governor restore rejected", dir.display()));
        }
    }

    match error {
        Some(problem) => Err(problem),
        None => Ok(count),
    }
}

/// 紧急回退（panic 钩子 / 心跳看门狗 / 恢复兜底）：不做任何运行簿记，
/// 直接按备份恢复每个策略。返回实际写入的节点数。
pub fn emergency_restore(policies: &[EmergencyPolicy]) -> Result<u64, String> {
    let mut writes = 0u64;
    let mut error: Option<String> = None;
    for policy in policies {
        match restore_dir(
            &policy.dir,
            policy.pristine_min,
            policy.pristine_max,
            &policy.original_governor,
        ) {
            Ok(count) => writes += count,
            Err(problem) => {
                error.get_or_insert(problem);
            }
        }
    }
    match error {
        Some(problem) => Err(problem),
        None => Ok(writes),
    }
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
            fs::write(dir.join("scaling_governor"), "schedutil\n").unwrap();
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
                    external_floor: None,
                    retries: 0,
                    retry_at: None,
                    error: None,
                    touched: false,
                    auto: None,
                }],
                applied: 0,
                cap_writes: 0,
                floor_writes: 0,
                rejected_floor: 0,
                guard_until: None,
                suspended: false,
                enabled: true,
                respect_external: true,
                auto_mode: false,
                standby: false,
                deadband: 0.10,
                last_error: None,
            }
        }
        fn range(&self) -> (u32, u32) {
            read_range(&self.0).unwrap()
        }
        fn governor(&self) -> String {
            fs::read_to_string(self.0.join("scaling_governor"))
                .unwrap()
                .trim()
                .to_owned()
        }
        fn set_governor(&self, name: &str) {
            fs::write(self.0.join("scaling_governor"), format!("{name}\n")).unwrap();
        }
        /// 模拟内核提供 userspace governor 的设备（含 scaling_setspeed 节点）。
        fn enable_userspace(&self) {
            fs::write(
                self.0.join("scaling_available_governors"),
                "walt conservative powersave performance schedutil userspace\n",
            )
            .unwrap();
            if !self.0.join("scaling_setspeed").exists() {
                write_khz(&self.0.join("scaling_setspeed"), 100).unwrap();
            }
        }
        fn set_speed(&self) -> Option<u32> {
            read_u32(&self.0.join("scaling_setspeed"))
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
            writer.respect_external,
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
    fn cool_device_reasserts_own_cap_over_external_writer() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.apply(&[decision(500, 700, false)]).unwrap();
        // 外部写入者（云控/性能服务）把上限压到 300。
        write_khz(&sysfs.0.join("scaling_max_freq"), 300).unwrap();
        writer.policies[0].observed = Some((500, 300));
        writer.policies[0].external_cap = Some(300);
        // 温度保护线下不再让位：我们按自己的目标上限夺回控制权。
        writer.set_respect_external(false);
        sync_at(&mut writer, Instant::now());
        assert_eq!(sysfs.range(), (500, 700));
        // 外部上限仍被跟踪，温度回升后恢复让位逻辑。
        assert_eq!(writer.policies[0].external_cap, Some(300));
        writer.set_respect_external(true);
        write_khz(&sysfs.0.join("scaling_max_freq"), 300).unwrap();
        writer.policies[0].last_reconcile = None;
        sync_at(&mut writer, Instant::now());
        assert_eq!(sysfs.range().1, 300);
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
        // 用同名目录顶替节点：写入必然失败（EISDIR），模拟恢复被拒。
        fs::remove_file(&min).unwrap();
        fs::create_dir(&min).unwrap();
        assert!(writer.restore().is_err());
        assert!(writer.last_error.is_some());
        fs::remove_dir(&min).unwrap();
        write_khz(&min, 500).unwrap();
        drop(writer);
        assert_eq!(sysfs.range(), (100, 1000));
    }

    #[test]
    fn auto_pin_takes_over_and_releases_on_standby_and_mode_switch() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        // 内核未提供 userspace（沙箱无 available_governors）→ 退化为 Pin 锁频。
        writer.set_auto(true, true, 0.10);
        writer.apply(&[decision(700, 700, false)]).unwrap();
        assert_eq!(writer.auto_path(), Some(AutoPath::Pin));
        assert_eq!(sysfs.range(), (700, 700), "Pin 接管 = min=max=目标频率");

        // AegisAuto 息屏：区间交还内核，随后的 apply 不得写入。
        writer.set_auto(true, false, 0.10);
        assert_eq!(writer.auto_path(), None);
        assert_eq!(sysfs.range(), (100, 1000));
        writer.apply(&[decision(300, 300, false)]).unwrap();
        assert_eq!(sysfs.range(), (100, 1000), "息屏挂起期间不得写入");
        assert!(!writer.reconcile().unwrap());

        // 亮屏重新接管。
        writer.set_auto(true, true, 0.10);
        writer.apply(&[decision(500, 500, false)]).unwrap();
        assert_eq!(writer.auto_path(), Some(AutoPath::Pin));
        assert_eq!(sysfs.range(), (500, 500));

        // 切回 Standard：立即恢复原始区间。
        writer.set_auto(false, true, 0.10);
        assert_eq!(writer.auto_path(), None);
        assert_eq!(sysfs.range(), (100, 1000));
    }

    #[test]
    fn auto_deadband_only_softens_downward_moves() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        // 密集频点：相邻档位相差 <10%。
        writer.policies[0].freqs = vec![100, 300, 500, 540, 580, 620, 700, 1000];
        writer.set_auto(true, true, 0.10);

        writer.apply(&[decision(540, 540, false)]).unwrap();
        assert_eq!(sysfs.range(), (540, 540));
        // 上行 7.4%（540→580）：升频不再被死区挡住，立即写入（否则欠频卡顿）。
        writer.apply(&[decision(580, 580, false)]).unwrap();
        assert_eq!(sysfs.range(), (580, 580), "升频必须立即生效");
        // 上行 14.8%：写入。
        writer.apply(&[decision(620, 620, false)]).unwrap();
        assert_eq!(sysfs.range(), (620, 620));
        // 下行 6.5%（620→580）在 10% 死区内：不写。
        writer.apply(&[decision(580, 580, false)]).unwrap();
        assert_eq!(sysfs.range(), (620, 620), "下行死区内保持不写");
        // 下行 19.4%：写入。
        writer.apply(&[decision(500, 500, false)]).unwrap();
        assert_eq!(sysfs.range(), (500, 500));
    }

    #[test]
    fn auto_setspeed_path_keeps_range_and_repairs_governor() {
        let sysfs = Sysfs::new();
        sysfs.enable_userspace();
        let mut writer = sysfs.writer();
        writer.set_auto(true, true, 0.10);
        writer.apply(&[decision(700, 700, false)]).unwrap();
        assert_eq!(writer.auto_path(), Some(AutoPath::SetSpeed));
        assert_eq!(sysfs.governor(), "userspace");
        assert_eq!(sysfs.set_speed(), Some(700));
        assert_eq!(
            sysfs.range(),
            (100, 1000),
            "setspeed 路径不得改动 min/max"
        );

        // 被外部改回原 governor：reconcile 重新切回 userspace。
        sysfs.set_governor("schedutil");
        writer.policies[0].last_reconcile = None;
        assert!(writer.reconcile().unwrap());
        assert_eq!(sysfs.governor(), "userspace");

        // 紧急回退：governor 写回备份值，区间保持原始状态。
        let policies = writer.emergency_policies();
        crate::writer::emergency_restore(&policies).unwrap();
        assert_eq!(sysfs.governor(), "schedutil");
        assert_eq!(sysfs.range(), (100, 1000));
    }

    #[test]
    fn restore_returns_governor_and_range_after_takeover() {
        let sysfs = Sysfs::new();
        sysfs.enable_userspace();
        let mut writer = sysfs.writer();
        writer.set_auto(true, true, 0.10);
        writer.apply(&[decision(700, 700, false)]).unwrap();
        assert_eq!(sysfs.governor(), "userspace");
        writer.restore().unwrap();
        assert_eq!(sysfs.governor(), "schedutil", "退出时必须写回原 governor");
        assert_eq!(sysfs.range(), (100, 1000));
        assert_eq!(writer.auto_path(), None);
        // 恢复后进入 suspended，任何 apply 都不再写入。
        writer.apply(&[decision(300, 300, false)]).unwrap();
        assert_eq!(sysfs.range(), (100, 1000));
    }

    #[test]
    fn emergency_restore_is_safe_without_prior_takeover() {
        let sysfs = Sysfs::new();
        let writer = sysfs.writer();
        let policies = writer.emergency_policies();
        crate::writer::emergency_restore(&policies).unwrap();
        assert_eq!(sysfs.range(), (100, 1000));
        assert_eq!(sysfs.governor(), "schedutil");
    }

    #[test]
    fn auto_pin_yields_to_latched_external_floor() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.set_auto(true, true, 0.10);
        writer.apply(&[decision(300, 300, false)]).unwrap();
        assert_eq!(sysfs.range(), (300, 300));

        // 外部（powerkeeper 类）把下限抬到 500 并被读回拒绝 → 锁存地板。
        writer.policies[0].external_floor = Some((500, Instant::now()));
        write_khz(&sysfs.0.join("scaling_min_freq"), 500).unwrap();
        writer.policies[0].last_reconcile = None;
        // 外部改写先进入退避（以 Err 形式上报争写），到期后按“让位到地板”
        // 重新锁频：min=max=500。
        assert!(writer.reconcile().unwrap_or(true));
        let retry = writer.policies[0].retry_at.unwrap();
        sync_at(&mut writer, retry);
        assert_eq!(sysfs.range(), (500, 500), "锁频让位到外部地板而不是目标");
        assert!(writer.last_error.is_none(), "让位后不再报告争写错误");

        // 目标回升到地板之上：让位自动解除，锁回目标。
        writer.policies[0].last_reconcile = None;
        writer.apply(&[decision(700, 700, false)]).unwrap();
        assert_eq!(sysfs.range(), (700, 700));
        assert!(writer.policies[0].external_floor.is_none());
    }

    #[test]
    fn auto_pin_stale_external_floor_expires_and_descends() {
        let sysfs = Sysfs::new();
        let mut writer = sysfs.writer();
        writer.set_auto(true, true, 0.10);
        // 曾被瞬时 boost 地板顶到 500：锁频在 500，目标 300，锁存已超 2s 时效。
        write_khz(&sysfs.0.join("scaling_min_freq"), 500).unwrap();
        write_khz(&sysfs.0.join("scaling_max_freq"), 500).unwrap();
        writer.policies[0].last_target = 300;
        writer.policies[0].last_cap = 300;
        writer.policies[0].external_floor = Some((500, Instant::now() - Duration::from_secs(3)));

        writer.policies[0].last_reconcile = None;
        let _ = writer.reconcile().unwrap_or(true);
        assert!(
            writer.policies[0].external_floor.is_none(),
            "过期的 boost 地板必须自动失效"
        );
        let retry = writer.policies[0].retry_at.unwrap_or_else(|| Instant::now());
        sync_at(&mut writer, retry);
        assert_eq!(
            sysfs.range(),
            (300, 300),
            "地板过期后必须下探回目标频率"
        );
    }
}
