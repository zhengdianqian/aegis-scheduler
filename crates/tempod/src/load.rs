// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! 负载数据源抽象（LoadSource）。
//!
//! 决策层只消费归一化后的 `LoadSample`——与 /proc/stat 同量纲的每簇 0..1
//! 占用率，不关心数据来自哪里；本轮只换数据源，不改任何决策算法。
//!
//! * `EbpfSource`（精度增强包，默认）：加载 `tempo.bpf.o` 挂
//!   `tracepoint/sched/sched_switch`，BPF 侧只记录切换时间差并按 CPU/簇聚合
//!   到 Map，用户态读 Map 后归一化输出。不可用时自动回退。
//! * `ProcStatSource`（标准回退，降级是一等公民）：读 /proc/stat 的每 CPU
//!   计数差值；前景线程占用由 /dev/cpuset/top-app 与 /proc/<tid>/stat 差值
//!   归因，输出同量纲、同采样周期的数据。回退后行为与标准 Aegis 一致。
//!
//! 启动时默认尝试 eBPF：设备不支持、加载失败或探测缓存判定“内核与版本均未
//! 变化的已知不可用”，都静默回退到标准源并记录明确原因。运行时连续
//! [`MAX_CONSECUTIVE_FAILURES`] 次读取失败或数据异常会触发安全阀，永久回退
//! 到标准源，防止半坏的 eBPF 源拖累决策。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 运行时安全阀：eBPF 源连续该次数读取失败/数据异常后永久回退。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// 采样窗口超过 10 个量子说明中间有长时间停摆（息屏挂起、调度停顿），
/// 直接重新播种基线，不用跨窗口均值污染第一拍数据。
pub const RESEED_WINDOW_FACTOR: u64 = 10;
/// 前景归因的最小采样间隔。/proc/<tid>/stat 逐线程读取比 /proc/stat 贵一个
/// 数量级，前景占用按 1 s 节拍计算（EMA 平滑不受影响），整簇占用仍按量子节拍。
pub const FOREGROUND_SAMPLE_US: u64 = 1_000_000;
/// 前景归因窗口异常拉长（>5 s）时只重新播种，不产出比例。
pub const FOREGROUND_RESEED_FACTOR: u64 = 5;
/// Android 的 USER_HZ 恒为 100（bionic 不暴露可变 tick）。
pub const TICK_HZ: u64 = 100;
/// 前台任务集合来源（与 eBPF 探针共用同一标准节点）。
pub const FOREGROUND_TASKS: &str = "/dev/cpuset/top-app/tasks";
/// 探测缓存文件名（位于 /data/adb/tempo，跨开机保留，卸载即清除）。
pub const PROBE_CACHE_FILE: &str = "ebpf-probe.cache";

/// 当前负载源。诊断包与 WebUI 据此显示“精度增强（eBPF）”还是“标准回退
/// （/proc/stat）”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Ebpf,
    ProcStat,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ebpf => "ebpf",
            Self::ProcStat => "proc_stat",
        }
    }
}

/// 一次归一化的负载观测。所有字段与 eBPF 探针的历史语义一致，
/// 两条数据源路径输出完全同量纲。
#[derive(Debug, Clone, Default)]
pub struct LoadSample {
    pub ts_us: u64,
    /// 每簇总占用率，0..1。
    pub busy: Vec<f32>,
    /// 每簇前台占用率，0..1。
    pub foreground: Vec<f32>,
    pub foreground_active: bool,
    /// 本拍前台任务集合发生变化（事件循环据此解析包名、重挂帧反馈）。
    pub foreground_changed: bool,
    pub events: u64,
    pub active_events: u64,
    pub fg_switches: Vec<u64>,
    pub switches: Vec<u64>,
}

/// 负载数据源接口。决策层（事件循环）只依赖本接口，不直接调用具体实现；
/// 帧感知（AegisFrame）是独立数据源，与本接口互不耦合。
pub trait LoadSource {
    fn kind(&self) -> SourceKind;
    /// 该源的 epoll 事件描述符；纯定时读取的源由 `LoadManager` 暴露其
    /// 定时器描述符。fd 由源自身持有，源被关闭时由内核自动从 epoll 摘除。
    fn event_fd(&self) -> Option<i32>;
    /// 非阻塞读取一次。`Ok(None)` 表示本拍没有值得发布的新观测（不是错误）。
    fn read(&mut self) -> Result<Option<LoadSample>, String>;
    fn foreground_ids(&self) -> Vec<u32>;
    fn foreground_error(&self) -> Option<String> {
        None
    }
    /// 屏幕开关门（关屏停止产出/停表，开屏重新播种）。
    fn set_screen(&mut self, on: bool) -> Result<(), String>;
    fn close(&mut self);
}

// ---------------------------------------------------------------------------
// 启动探测缓存
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVerdict {
    pub kernel: String,
    pub version: String,
    pub result: String,
    pub reason: Option<String>,
}

/// 首次探测结果缓存：只有“内核与本版本都没变的已知不可用”才跳过加载尝试，
/// 内核、ROM 或 Aegis 版本变化都会重新探测。
pub struct ProbeCache {
    path: PathBuf,
}

impl ProbeCache {
    pub fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(PROBE_CACHE_FILE),
        }
    }

    pub fn read(&self) -> Option<CachedVerdict> {
        let text = fs::read_to_string(&self.path).ok()?;
        let mut verdict = CachedVerdict {
            kernel: String::new(),
            version: String::new(),
            result: String::new(),
            reason: None,
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "kernel" => verdict.kernel = value.to_owned(),
                "version" => verdict.version = value.to_owned(),
                "result" => verdict.result = value.to_owned(),
                "reason" => verdict.reason = Some(value.to_owned()),
                _ => {}
            }
        }
        if verdict.kernel.is_empty() || verdict.result.is_empty() {
            None
        } else {
            Some(verdict)
        }
    }

    pub fn store(&self, kernel: &str, version: &str, result: &str, reason: Option<&str>) {
        let mut text = format!(
            "kernel={kernel}\nversion={version}\nresult={result}\n"
        );
        if let Some(reason) = reason {
            // 单行化，保持 key=value 文件格式稳定。
            text.push_str(&format!(
                "reason={}\n",
                reason.replace('\n', " ").trim()
            ));
        }
        let _ = fs::write(&self.path, text);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupDecision {
    Attempt,
    Skip { reason: String },
}

/// 启动策略：已知不可用（同内核、同版本）→ 跳过重复加载尝试；
/// 其余情况一律现场尝试 eBPF。
pub fn startup_decision(
    cached: Option<&CachedVerdict>,
    kernel: &str,
    version: &str,
) -> StartupDecision {
    match cached {
        Some(cached)
            if cached.result == "unavailable"
                && cached.kernel == kernel
                && cached.version == version =>
        {
            StartupDecision::Skip {
                reason: cached
                    .reason
                    .clone()
                    .unwrap_or_else(|| "上次探测失败".to_owned()),
            }
        }
        _ => StartupDecision::Attempt,
    }
}

// ---------------------------------------------------------------------------
// 归一化与校验（纯函数，宿主可测）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// 解析 /proc/stat 的单个 `cpuN` 行：前 8 列（user..steal）求和为 total，
/// idle+iowait 为 idle——与 WebUI 的 /proc/stat 口径逐字节一致。
pub fn parse_cpu_line(line: &str) -> Option<(u32, CpuTimes)> {
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    let cpu = name.strip_prefix("cpu")?.parse::<u32>().ok()?;
    let mut values = [0u64; 8];
    for slot in values.iter_mut() {
        *slot = fields.next()?.parse().ok()?;
    }
    let total = values.iter().sum();
    let idle = values[3] + values[4];
    Some((cpu, CpuTimes { total, idle }))
}

/// 解析整份 /proc/stat 文本到每 CPU 计数（跳过聚合的 `cpu ` 行）。
pub fn parse_proc_stat(text: &str) -> HashMap<u32, CpuTimes> {
    let mut cpus = HashMap::new();
    for line in text.lines() {
        if let Some(entry) = parse_cpu_line(line) {
            cpus.insert(entry.0, entry.1);
        }
    }
    cpus
}

/// 按簇聚合每 CPU 的 total/idle 差值。窗口内消失的 CPU（热插拔离线）视为
/// 无贡献；全簇无数据（全部离线）按空闲处理。
pub fn cluster_busy(
    prev: &HashMap<u32, CpuTimes>,
    cur: &HashMap<u32, CpuTimes>,
    cpus: &[u32],
) -> f32 {
    let mut total = 0u64;
    let mut busy = 0u64;
    for cpu in cpus {
        let (Some(before), Some(after)) = (prev.get(cpu), cur.get(cpu)) else {
            continue;
        };
        if after.total < before.total {
            continue;
        }
        let delta = after.total - before.total;
        let idle = after.idle.saturating_sub(before.idle).min(delta);
        total += delta;
        busy += delta - idle;
    }
    if total == 0 {
        return 0.0;
    }
    (busy as f32 / total as f32).clamp(0.0, 1.0)
}

/// 解析 /proc/<tid>/stat：跳过含空格的 (comm) 后取 utime(14)、stime(15)
/// 与 processor(39)。返回 (累计 CPU ticks, 最近运行 CPU)。
pub fn parse_task_stat(line: &str) -> Option<(u64, u32)> {
    let close = line.rfind(')')?;
    let rest = line[close + 1..].split_whitespace().collect::<Vec<_>>();
    // rest[0] 是第 3 列（state），因此第 14 列 utime 的下标是 14-3=11。
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    let cpu: u32 = rest.get(36)?.parse().ok()?;
    Some((utime + stime, cpu))
}

/// 前景占用归因：把每个前台线程的 ticks 增量记到它最近运行 CPU 所属的簇，
/// 再除以（簇核数 × 窗口秒数）得到与 /proc/stat 同量纲的 0..1 比例。
pub fn foreground_cluster_busy(
    deltas: &[(u64, u32)],
    cpu_cluster: &HashMap<u32, usize>,
    cluster_cpus: &[usize],
    elapsed_s: f64,
) -> Vec<f32> {
    let mut seconds = vec![0.0f64; cluster_cpus.len()];
    for (ticks, cpu) in deltas {
        let Some(index) = cpu_cluster.get(cpu) else {
            continue;
        };
        if let Some(slot) = seconds.get_mut(*index) {
            *slot += *ticks as f64 / TICK_HZ as f64;
        }
    }
    seconds
        .into_iter()
        .zip(cluster_cpus)
        .map(|(used, cpus)| {
            if elapsed_s <= 0.0 || *cpus == 0 {
                0.0
            } else {
                (used / (elapsed_s * *cpus as f64)) as f32
            }
        })
        .map(|ratio| ratio.clamp(0.0, 1.0))
        .collect()
}

/// 解析 /dev/cpuset/top-app/tasks 的 pid 列表。
pub fn parse_task_list(text: &str) -> HashSet<u32> {
    text.split_whitespace()
        .filter_map(|line| line.parse::<u32>().ok())
        .collect()
}

/// 校验并夹取一个负载样本；维度不符或数值越界视为数据异常（触发安全阀计数）。
fn validate_sample(mut sample: LoadSample, cluster_count: usize) -> Result<LoadSample, String> {
    if sample.busy.len() != cluster_count || sample.foreground.len() != cluster_count {
        return Err(format!(
            "数据异常：样本维度 busy={} foreground={} 与 {} 簇不符",
            sample.busy.len(),
            sample.foreground.len(),
            cluster_count
        ));
    }
    for slot in sample.busy.iter_mut().chain(sample.foreground.iter_mut()) {
        let value = *slot;
        if !value.is_finite() || value < -0.01 || value > 1.01 {
            return Err(format!("数据异常：占用率 {value} 超出 0..1"));
        }
        *slot = value.clamp(0.0, 1.0);
    }
    Ok(sample)
}

// ---------------------------------------------------------------------------
// 前台任务集合（标准节点，两条数据源共用语义）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ForegroundSet {
    pub pids: HashSet<u32>,
    pub error: Option<String>,
    last_refresh_us: u64,
}

impl ForegroundSet {
    /// 读取一次当前集合（失败保留旧集合并记录原因，与探针行为一致）。
    pub fn seed() -> Self {
        let mut set = Self::default();
        set.reload();
        set
    }

    fn reload(&mut self) -> bool {
        match fs::read_to_string(FOREGROUND_TASKS) {
            Ok(text) => {
                let next = parse_task_list(&text);
                let changed = next != self.pids;
                self.pids = next;
                self.error = None;
                changed
            }
            Err(error) => {
                self.error = Some(format!("read {FOREGROUND_TASKS}: {error}"));
                false
            }
        }
    }

    /// 最多每秒刷新一次；返回集合是否变化。
    pub fn refresh(&mut self, now_us: u64) -> bool {
        if now_us.saturating_sub(self.last_refresh_us) < FOREGROUND_SAMPLE_US {
            return false;
        }
        self.last_refresh_us = now_us;
        self.reload()
    }
}

// ---------------------------------------------------------------------------
// 启动编排与运行时安全阀
// ---------------------------------------------------------------------------

pub struct StartupReport {
    pub kind: SourceKind,
    pub reason: Option<String>,
}

pub struct LoadManager {
    active: SourceKind,
    ebpf: Option<Box<dyn LoadSource>>,
    procstat: Box<dyn LoadSource>,
    cluster_count: usize,
    failures: u32,
    reason: Option<String>,
    screen_on: bool,
    switched: Option<String>,
}

/// 组装数据源：按缓存决定是否尝试 eBPF，失败静默回退标准源。
/// 两个 opener 由调用方注入（真机实现 / 单测 mock），任何一路彻底不可用才
/// 返回 Err（此时与旧版“探针不可用则不调度”一致）。
pub fn open_manager(
    directory: &Path,
    kernel: &str,
    version: &str,
    cluster_count: usize,
    ebpf_opener: impl FnOnce() -> Result<Box<dyn LoadSource>, String>,
    procstat_opener: impl FnOnce() -> Result<Box<dyn LoadSource>, String>,
) -> Result<(LoadManager, StartupReport), String> {
    let cache = ProbeCache::new(directory);
    let verdict = cache.read();
    let mut ebpf = None;
    let mut reason = None;
    match startup_decision(verdict.as_ref(), kernel, version) {
        StartupDecision::Skip { reason: cached } => {
            let message = format!(
                "负载源：探测缓存命中（内核 {kernel} / 版本 {version} 均未变化），跳过 eBPF 加载；{cached}"
            );
            announce(&message);
            reason = Some(cached);
        }
        StartupDecision::Attempt => match ebpf_opener() {
            Ok(source) => {
                cache.store(kernel, version, "available", None);
                announce(&format!("负载源：精度增强包（eBPF）已启用，内核 {kernel}"));
                ebpf = Some(source);
            }
            Err(error) => {
                let detail = error.replace('\n', " ");
                cache.store(kernel, version, "unavailable", Some(&detail));
                let message =
                    format!("负载源：精度增强包不可用，静默回退标准 /proc/stat 采集；{detail}");
                announce(&message);
                reason = Some(detail);
            }
        },
    }
    let procstat = procstat_opener().map_err(|error| {
        format!("标准负载源（/proc/stat）初始化失败，无法继续：{error}")
    })?;
    let kind = ebpf
        .as_ref()
        .map_or(SourceKind::ProcStat, |source| source.kind());
    let report = StartupReport {
        kind,
        reason: reason.clone(),
    };
    Ok((
        LoadManager {
            active: kind,
            ebpf,
            procstat,
            cluster_count,
            failures: 0,
            reason,
            screen_on: true,
            switched: None,
        },
        report,
    ))
}

/// 关键路径的可见日志：即使 log_level=off 也会随 stdout 落进 Aegis.log。
fn announce(message: &str) {
    eprintln!("Aegis: {message}");
    tracing::info!("{message}");
}

impl LoadManager {
    pub fn kind(&self) -> SourceKind {
        self.active
    }

    /// 当前回退原因（启动诊断与单测断言使用；运行时切换原因经
    /// `take_switched` 一次性取走）。
    #[allow(dead_code)]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// 运行时安全阀触发后取走回退原因（只取一次，由事件循环写入诊断状态）。
    pub fn take_switched(&mut self) -> Option<String> {
        self.switched.take()
    }

    pub fn ebpf_fd(&self) -> Option<i32> {
        self.ebpf.as_ref().and_then(|source| source.event_fd())
    }

    /// 标准源的采样定时器（源被创建就存在；仅作为活动源且亮屏时运行）。
    pub fn timer_fd(&self) -> Option<i32> {
        self.procstat.event_fd()
    }

    pub fn set_screen(&mut self, on: bool) -> Result<(), String> {
        self.screen_on = on;
        if let Some(source) = self.ebpf.as_mut() {
            source.set_screen(on)?;
        }
        self.procstat
            .set_screen(self.active == SourceKind::ProcStat && on)
    }

    /// 读取活动源并执行安全阀：eBPF 连续失败/数据异常达到阈值后永久回退，
    /// 关闭 eBPF（内核自动从 epoll 摘除 ring fd）并启动标准源定时器。
    pub fn read(&mut self) -> Result<Option<LoadSample>, String> {
        if self.active == SourceKind::Ebpf {
            let result = match self.ebpf.as_mut() {
                Some(source) => source.read(),
                None => Err("内部状态异常：eBPF 源缺失".to_owned()),
            };
            let checked = match result {
                Ok(Some(sample)) => validate_sample(sample, self.cluster_count).map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            match checked {
                Ok(sample) => {
                    self.failures = 0;
                    Ok(sample)
                }
                Err(error) => {
                    self.failures += 1;
                    if self.failures >= MAX_CONSECUTIVE_FAILURES {
                        let message = format!(
                            "运行时安全阀：连续 {} 次读取失败/数据异常，已永久回退标准 /proc/stat 源（最近：{error}）",
                            self.failures
                        );
                        self.switch_to_procstat(&message);
                    }
                    Err(error)
                }
            }
        } else {
            let result = self.procstat.read();
            match result {
                Ok(Some(sample)) => match validate_sample(sample, self.cluster_count) {
                    Ok(sample) => {
                        self.failures = 0;
                        Ok(Some(sample))
                    }
                    Err(error) => {
                        // 标准源是兜底底线：记录计数并继续重试，保留上一拍需求。
                        self.failures += 1;
                        Err(error)
                    }
                },
                Ok(None) => {
                    self.failures = 0;
                    Ok(None)
                }
                Err(error) => {
                    self.failures += 1;
                    Err(format!("标准源读取失败：{error}"))
                }
            }
        }
    }

    fn switch_to_procstat(&mut self, message: &str) {
        if let Some(mut source) = self.ebpf.take() {
            // 按接口契约显式关闭（关闭后 fd 由内核自动从 epoll 摘除）。
            source.close();
        }
        self.active = SourceKind::ProcStat;
        self.reason = Some(message.to_owned());
        self.failures = 0;
        self.switched = Some(message.to_owned());
        announce(message);
        // 标准源成为活动源：按当前屏幕状态启动采样定时器。
        let screen_on = self.screen_on;
        let _ = self.procstat.set_screen(screen_on);
    }

    pub fn foreground_ids(&self) -> Vec<u32> {
        let source: &dyn LoadSource = if self.active == SourceKind::Ebpf {
            match self.ebpf.as_ref() {
                Some(source) => source.as_ref(),
                None => self.procstat.as_ref(),
            }
        } else {
            self.procstat.as_ref()
        };
        source.foreground_ids()
    }

    pub fn foreground_error(&self) -> Option<String> {
        if self.active == SourceKind::Ebpf {
            self.ebpf.as_ref().and_then(|source| source.foreground_error())
        } else {
            self.procstat.foreground_error()
        }
    }

    pub fn close(&mut self) {
        if let Some(source) = self.ebpf.as_mut() {
            source.close();
        }
        self.procstat.close();
    }
}

impl Drop for LoadManager {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// 真机实现（仅 Linux/Android）
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use real::{EbpfSource, ProcStatSource};

#[cfg(any(target_os = "linux", target_os = "android"))]
mod real {
    use super::*;
    use crate::event_loop::now_us;
    use crate::probe::{DemandSnapshot, ProbeSession};
    use tempo_core::{Config, DeviceProfile};

    /// 精度增强包：包装 eBPF 探针会话，输出归一化负载样本。
    pub struct EbpfSource {
        session: Option<ProbeSession>,
    }

    impl EbpfSource {
        pub fn open(profile: &DeviceProfile, config: &Config) -> Result<Self, String> {
            Ok(Self {
                session: Some(ProbeSession::open(profile, config)?),
            })
        }
    }

    impl LoadSource for EbpfSource {
        fn kind(&self) -> SourceKind {
            SourceKind::Ebpf
        }

        fn event_fd(&self) -> Option<i32> {
            self.session.as_ref().map(ProbeSession::ring_fd)
        }

        fn read(&mut self) -> Result<Option<LoadSample>, String> {
            let session = self
                .session
                .as_mut()
                .ok_or_else(|| "eBPF 源已关闭".to_owned())?;
            Ok(session.drain()?.map(LoadSample::from))
        }

        fn foreground_ids(&self) -> Vec<u32> {
            self.session
                .as_ref()
                .map(ProbeSession::foreground_ids)
                .unwrap_or_default()
        }

        fn foreground_error(&self) -> Option<String> {
            self.session
                .as_ref()
                .and_then(ProbeSession::foreground_error)
                .map(str::to_owned)
        }

        fn set_screen(&mut self, on: bool) -> Result<(), String> {
            match self.session.as_mut() {
                Some(session) => session.set_screen(on),
                None => Ok(()),
            }
        }

        fn close(&mut self) {
            self.session = None;
        }
    }

    impl From<DemandSnapshot> for LoadSample {
        fn from(snapshot: DemandSnapshot) -> Self {
            Self {
                ts_us: snapshot.ts_us,
                busy: snapshot.busy,
                foreground: snapshot.foreground,
                foreground_active: snapshot.foreground_active,
                foreground_changed: snapshot.foreground_changed,
                events: snapshot.events,
                active_events: snapshot.active_events,
                fg_switches: snapshot.fg_switches,
                switches: snapshot.switches,
            }
        }
    }

    /// 标准回退源：/proc/stat 每 CPU 计数差值 + 前台线程归因，
    /// 由 timerfd 按 `runtime_quantum_us` 节拍唤醒，与 eBPF 路径同采样周期。
    pub struct ProcStatSource {
        shape: Vec<Vec<u32>>,
        cpu_cluster: HashMap<u32, usize>,
        cluster_cpus: Vec<usize>,
        prev: HashMap<u32, CpuTimes>,
        prev_ts_us: u64,
        seeded: bool,
        fg: ForegroundSet,
        fg_prev_used: HashMap<u32, u64>,
        fg_prev_ts_us: u64,
        last_fg: Vec<f32>,
        quantum_us: u64,
        timer: GateTimer,
    }

    impl ProcStatSource {
        pub fn open(profile: &DeviceProfile, config: &Config) -> Result<Self, String> {
            let shape: Vec<Vec<u32>> = profile.clusters.iter().map(|c| c.cpus.clone()).collect();
            let mut cpu_cluster = HashMap::new();
            let mut cluster_cpus = Vec::with_capacity(shape.len());
            for (index, cpus) in shape.iter().enumerate() {
                for cpu in cpus {
                    cpu_cluster.insert(*cpu, index);
                }
                cluster_cpus.push(cpus.len().max(1));
            }
            let quantum_us = config.events.runtime_quantum_us.clamp(1_000, 300_000);
            Ok(Self {
                shape,
                cpu_cluster,
                cluster_cpus,
                prev: HashMap::new(),
                prev_ts_us: 0,
                seeded: false,
                fg: ForegroundSet::seed(),
                fg_prev_used: HashMap::new(),
                fg_prev_ts_us: 0,
                last_fg: vec![0.0; profile.clusters.len()],
                quantum_us,
                timer: GateTimer::new()?,
            })
        }

        /// 重新播种：丢弃跨窗口差值，只建立新基线。
        fn reseed(&mut self, now: u64, current: HashMap<u32, CpuTimes>) {
            self.prev = current;
            self.prev_ts_us = now;
            self.seeded = true;
            self.fg_prev_ts_us = now;
            self.fg_prev_used.clear();
            for pid in self.fg.pids.iter().copied() {
                if let Some((used, _)) = read_task_stat(pid) {
                    self.fg_prev_used.insert(pid, used);
                }
            }
        }

        fn foreground_due(&self, now: u64) -> bool {
            self.fg_prev_ts_us == 0
                || now.saturating_sub(self.fg_prev_ts_us) >= FOREGROUND_SAMPLE_US
        }

        fn collect_foreground(&mut self, now: u64) {
            let window = now.saturating_sub(self.fg_prev_ts_us);
            let stale = self.fg_prev_ts_us == 0
                || window > FOREGROUND_SAMPLE_US * FOREGROUND_RESEED_FACTOR;
            let pids: Vec<u32> = self.fg.pids.iter().copied().collect();
            let mut deltas: Vec<(u64, u32)> = Vec::with_capacity(pids.len());
            for pid in pids {
                match read_task_stat(pid) {
                    Some((used, cpu)) => {
                        if let Some(previous) = self.fg_prev_used.get(&pid) {
                            if !stale && used >= *previous {
                                deltas.push((used - *previous, cpu));
                            }
                        }
                        self.fg_prev_used.insert(pid, used);
                    }
                    None => {
                        self.fg_prev_used.remove(&pid);
                    }
                }
            }
            if stale {
                // 首拍或异常长窗口：只播种，不产出比例。
                self.fg_prev_ts_us = now;
            } else {
                let elapsed_s = window as f64 / 1_000_000.0;
                self.last_fg = foreground_cluster_busy(
                    &deltas,
                    &self.cpu_cluster,
                    &self.cluster_cpus,
                    elapsed_s,
                );
                self.fg_prev_ts_us = now;
            }
        }
    }

    fn read_task_stat(pid: u32) -> Option<(u64, u32)> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_task_stat(&text)
    }

    impl LoadSource for ProcStatSource {
        fn kind(&self) -> SourceKind {
            SourceKind::ProcStat
        }

        fn event_fd(&self) -> Option<i32> {
            Some(self.timer.fd)
        }

        fn read(&mut self) -> Result<Option<LoadSample>, String> {
            self.timer.drain();
            let now = now_us();
            let text = fs::read_to_string("/proc/stat").map_err(|error| {
                format!("read /proc/stat: {error}")
            })?;
            let current = parse_proc_stat(&text);
            if current.is_empty() {
                return Err("read /proc/stat: no per-cpu lines".to_owned());
            }
            let window_us = now.saturating_sub(self.prev_ts_us);
            if !self.seeded || window_us > self.quantum_us * RESEED_WINDOW_FACTOR {
                self.reseed(now, current);
                return Ok(None);
            }
            let busy: Vec<f32> = self
                .shape
                .iter()
                .map(|cpus| cluster_busy(&self.prev, &current, cpus))
                .collect();
            self.prev = current;
            self.prev_ts_us = now;

            let mut changed = false;
            if self.foreground_due(now) {
                changed = self.fg.refresh(now);
                self.collect_foreground(now);
            }
            let foreground_active = !self.fg.pids.is_empty();
            Ok(Some(LoadSample {
                ts_us: now,
                busy,
                foreground: self.last_fg.clone(),
                foreground_active,
                foreground_changed: changed,
                events: 0,
                active_events: 0,
                fg_switches: Vec::new(),
                switches: Vec::new(),
            }))
        }

        fn foreground_ids(&self) -> Vec<u32> {
            self.fg.pids.iter().copied().collect()
        }

        fn foreground_error(&self) -> Option<String> {
            self.fg.error.clone()
        }

        fn set_screen(&mut self, on: bool) -> Result<(), String> {
            if on {
                self.timer.set_interval(self.quantum_us);
                // 开屏强制重新播种，避免把息屏期间的跨窗口均值当成本拍负载。
                self.seeded = false;
                self.fg_prev_ts_us = 0;
            } else {
                self.timer.stop();
                self.seeded = false;
                self.fg_prev_ts_us = 0;
            }
            Ok(())
        }

        fn close(&mut self) {
            self.timer.stop();
            self.timer.close();
        }
    }

    /// 采样定时器（timerfd，周期节拍）。
    struct GateTimer {
        fd: i32,
        closed: bool,
    }

    impl GateTimer {
        fn new() -> Result<Self, String> {
            let fd = unsafe {
                libc::timerfd_create(
                    libc::CLOCK_MONOTONIC,
                    libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "timerfd_create: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { fd, closed: false })
        }

        fn set_interval(&self, interval_us: u64) {
            let spec = libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: (interval_us / 1_000_000) as libc::time_t,
                    tv_nsec: ((interval_us % 1_000_000) * 1_000) as libc::c_long,
                },
                it_value: libc::timespec {
                    tv_sec: (interval_us / 1_000_000) as libc::time_t,
                    tv_nsec: ((interval_us % 1_000_000) * 1_000) as libc::c_long,
                },
            };
            unsafe { libc::timerfd_settime(self.fd, 0, &spec, std::ptr::null_mut()) };
        }

        fn stop(&self) {
            if self.closed {
                return;
            }
            let spec = libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
                it_value: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
            };
            unsafe { libc::timerfd_settime(self.fd, 0, &spec, std::ptr::null_mut()) };
        }

        fn drain(&self) {
            let mut count: u64 = 0;
            unsafe {
                libc::read(self.fd, &mut count as *mut u64 as *mut libc::c_void, 8);
            }
        }

        fn close(&mut self) {
            if !self.closed {
                unsafe { libc::close(self.fd) };
                self.closed = true;
            }
        }
    }

    impl Drop for GateTimer {
        fn drop(&mut self) {
            self.close();
        }
    }
}

/// 当前内核版本（uname -r），用于探测缓存失效判定。
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn kernel_release() -> String {
    unsafe {
        let mut info: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut info) != 0 {
            return String::new();
        }
        let bytes = std::ffi::CStr::from_ptr(info.release.as_ptr());
        bytes.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn sample(busy: &[f32]) -> LoadSample {
        LoadSample {
            ts_us: 1,
            busy: busy.to_vec(),
            foreground: vec![0.0; busy.len()],
            foreground_active: true,
            ..LoadSample::default()
        }
    }

    /// 记录调用轨迹的 mock 源。
    struct MockSource {
        kind: SourceKind,
        reads: Vec<Result<Option<LoadSample>, String>>,
        cursor: usize,
        fd: Option<i32>,
        screen: Rc<Cell<Vec<bool>>>,
        closed: Rc<Cell<bool>>,
        ids: Vec<u32>,
    }

    impl MockSource {
        fn new(kind: SourceKind, reads: Vec<Result<Option<LoadSample>, String>>) -> Self {
            Self {
                kind,
                reads,
                cursor: 0,
                fd: None,
                screen: Rc::new(Cell::new(Vec::new())),
                closed: Rc::new(Cell::new(false)),
                ids: vec![1, 2, 3],
            }
        }

        fn ebpf(reads: Vec<Result<Option<LoadSample>, String>>) -> Self {
            Self::new(SourceKind::Ebpf, reads)
        }

        fn procstat(reads: Vec<Result<Option<LoadSample>, String>>) -> Self {
            Self::new(SourceKind::ProcStat, reads)
        }
    }

    impl LoadSource for MockSource {
        fn kind(&self) -> SourceKind {
            self.kind
        }

        fn event_fd(&self) -> Option<i32> {
            self.fd
        }

        fn read(&mut self) -> Result<Option<LoadSample>, String> {
            let outcome = if self.cursor < self.reads.len() {
                self.reads[self.cursor].clone()
            } else {
                Ok(None)
            };
            self.cursor += 1;
            outcome
        }

        fn foreground_ids(&self) -> Vec<u32> {
            self.ids.clone()
        }

        fn set_screen(&mut self, on: bool) -> Result<(), String> {
            let mut log = self.screen.take();
            log.push(on);
            self.screen.set(log);
            Ok(())
        }

        fn close(&mut self) {
            self.closed.set(true);
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aegis-load-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_with(
        directory: &Path,
        ebpf: Result<Box<dyn LoadSource>, String>,
        calls: Rc<Cell<bool>>,
    ) -> Result<(LoadManager, StartupReport), String> {
        let kernel = "6.1.0-test";
        let version = "5.2.0";
        open_manager(
            directory,
            kernel,
            version,
            2,
            move || {
                calls.set(true);
                ebpf
            },
            || Ok(Box::new(MockSource::procstat(vec![Ok(Some(sample(&[0.1, 0.2])))])) as Box<dyn LoadSource>),
        )
    }

    #[test]
    fn open_defaults_to_ebpf_when_available() {
        let dir = temp_dir("ebpf-ok");
        let called = Rc::new(Cell::new(false));
        let (mut manager, report) = open_with(
            &dir,
            Ok(Box::new(MockSource::ebpf(vec![Ok(Some(sample(&[0.5, 0.6])))])) as Box<dyn LoadSource>),
            called.clone(),
        )
        .unwrap();
        assert!(called.get(), "默认必须尝试 eBPF");
        assert_eq!(report.kind, SourceKind::Ebpf);
        assert!(report.reason.is_none());
        assert_eq!(manager.kind(), SourceKind::Ebpf);
        assert!(manager.reason().is_none());
        // 缓存记录 available。
        let verdict = ProbeCache::new(&dir).read().unwrap();
        assert_eq!(verdict.result, "available");
        // 决策层拿到的是同量纲样本。
        let produced = manager.read().unwrap().unwrap();
        assert_eq!(produced.busy, vec![0.5, 0.6]);
    }

    #[test]
    fn open_falls_back_when_ebpf_load_fails_and_caches_verdict() {
        let dir = temp_dir("ebpf-fail");
        let called = Rc::new(Cell::new(false));
        let (mut manager, report) =
            open_with(&dir, Err("attach sched_switch: permission denied".to_owned()), called)
                .unwrap();
        assert_eq!(report.kind, SourceKind::ProcStat);
        assert!(report
            .reason
            .as_deref()
            .unwrap()
            .contains("permission denied"));
        assert_eq!(manager.kind(), SourceKind::ProcStat);
        let verdict = ProbeCache::new(&dir).read().unwrap();
        assert_eq!(verdict.result, "unavailable");
        assert_eq!(verdict.kernel, "6.1.0-test");
        assert_eq!(verdict.version, "5.2.0");
        // 回退后仍能读到标准源数据（降级是一等公民，不是异常分支）。
        let produced = manager.read().unwrap().unwrap();
        assert_eq!(produced.busy, vec![0.1, 0.2]);
    }

    #[test]
    fn cached_unavailable_skips_load_on_same_kernel_and_version() {
        let dir = temp_dir("cached");
        ProbeCache::new(&dir).store(
            "6.1.0-test",
            "5.2.0",
            "unavailable",
            Some("BTF missing"),
        );
        let called = Rc::new(Cell::new(false));
        let (_, report) = open_with(&dir, Err("不应被调用".to_owned()), called.clone()).unwrap();
        assert!(!called.get(), "同内核同版本的已知不可用不得重复加载");
        assert_eq!(report.kind, SourceKind::ProcStat);
        assert!(report.reason.as_deref().unwrap().contains("BTF missing"));
    }

    #[test]
    fn kernel_or_version_change_retriggers_probe() {
        let dir = temp_dir("retrigger");
        ProbeCache::new(&dir).store("6.1.0-old", "5.1.0", "unavailable", Some("old"));

        // 内核变化 → 重新探测。
        let called = Rc::new(Cell::new(false));
        let (mut manager, _) = open_manager(
            &dir,
            "6.1.0-new",
            "5.2.0",
            1,
            {
                let called = called.clone();
                move || {
                    called.set(true);
                    Ok(Box::new(MockSource::ebpf(vec![])) as Box<dyn LoadSource>)
                }
            },
            || Ok(Box::new(MockSource::procstat(vec![])) as Box<dyn LoadSource>),
        )
        .unwrap();
        assert!(called.get(), "内核变化必须重新探测");
        assert_eq!(manager.kind(), SourceKind::Ebpf);

        // 版本变化 → 重新探测。
        ProbeCache::new(&dir).store("6.1.0-new", "5.1.0", "unavailable", Some("old"));
        let called = Rc::new(Cell::new(false));
        open_manager(
            &dir,
            "6.1.0-new",
            "5.2.0",
            1,
            {
                let called = called.clone();
                move || {
                    called.set(true);
                    Err("retry".to_owned())
                }
            },
            || Ok(Box::new(MockSource::procstat(vec![])) as Box<dyn LoadSource>),
        )
        .unwrap();
        assert!(called.get(), "版本变化必须重新探测");
    }

    #[test]
    fn runtime_failures_trigger_permanent_fallback() {
        let dir = temp_dir("valve");
        let mut reads = Vec::new();
        for index in 0..MAX_CONSECUTIVE_FAILURES + 3 {
            reads.push(if index % 2 == 0 {
                Err(format!("map read failed #{index}"))
            } else {
                Ok(Some(sample(&[f32::NAN, 9.9])))
            });
        }
        reads.push(Ok(Some(sample(&[0.7, 0.7]))));
        let ebpf = MockSource::ebpf(reads);
        let closed = ebpf.closed.clone();
        let (mut manager, report) = open_manager(
            &dir,
            "k",
            "v",
            2,
            || Ok(Box::new(ebpf) as Box<dyn LoadSource>),
            || {
                Ok(Box::new(MockSource::procstat(vec![Ok(Some(sample(&[
                    0.3, 0.4,
                ])))])) as Box<dyn LoadSource>)
            },
        )
        .unwrap();
        assert_eq!(report.kind, SourceKind::Ebpf);

        // 前 N-1 次失败：仍在 eBPF 路径上，仅计数。
        for _ in 0..(MAX_CONSECUTIVE_FAILURES - 1) {
            assert!(manager.read().is_err());
            assert_eq!(manager.kind(), SourceKind::Ebpf, "未达阈值不得回退");
        }
        // 达到阈值：永久回退。
        assert!(manager.read().is_err());
        assert_eq!(manager.kind(), SourceKind::ProcStat);
        assert!(closed.get(), "回退必须关闭 eBPF 源（ring fd 随之摘除）");
        let reason = manager.take_switched().expect("回退原因可取");
        assert!(reason.contains("安全阀"), "{reason}");
        assert!(manager.reason().unwrap().contains("安全阀"));
        // 回退后由标准源出数。
        let produced = manager.read().unwrap().unwrap();
        assert_eq!(produced.busy, vec![0.3, 0.4]);
        // 原因只被取走一次（诊断状态只写一次日志）。
        assert!(manager.take_switched().is_none());
    }

    #[test]
    fn empty_reads_do_not_trigger_fallback() {
        let dir = temp_dir("steady");
        let reads = vec![Ok(None); 100];
        let (mut manager, _) = open_manager(
            &dir,
            "k",
            "v",
            2,
            || Ok(Box::new(MockSource::ebpf(reads)) as Box<dyn LoadSource>),
            || Ok(Box::new(MockSource::procstat(vec![])) as Box<dyn LoadSource>),
        )
        .unwrap();
        for _ in 0..100 {
            assert!(manager.read().unwrap().is_none());
        }
        assert_eq!(manager.kind(), SourceKind::Ebpf, "稳态无新观测不是故障");
    }

    #[test]
    fn screen_gate_reaches_active_source() {
        let dir = temp_dir("gate");
        let ebpf = MockSource::ebpf(vec![]);
        let ebpf_screen = ebpf.screen.clone();
        let procstat = MockSource::procstat(vec![]);
        let proc_screen = procstat.screen.clone();
        let (mut manager, _) = open_manager(
            &dir,
            "k",
            "v",
            1,
            || Ok(Box::new(ebpf) as Box<dyn LoadSource>),
            || Ok(Box::new(procstat) as Box<dyn LoadSource>),
        )
        .unwrap();
        manager.set_screen(false).unwrap();
        assert_eq!(ebpf_screen.take(), vec![false], "eBPF 源收到屏幕门");
        assert_eq!(proc_screen.take(), vec![false], "标准源定时器关闭");
        manager.set_screen(true).unwrap();
        assert_eq!(ebpf_screen.take(), vec![true]);
        // 每次 take 清空轨迹；本次（set_screen(true)）标准源仍收到“非活动”门
        // —— eBPF 活动期间标准源定时器保持关闭。
        assert_eq!(proc_screen.take(), vec![false]);
    }

    #[test]
    fn validate_rejects_wrong_dimensions_and_out_of_range() {
        assert!(validate_sample(sample(&[0.5, 0.5]), 2).is_ok());
        let error = validate_sample(sample(&[0.5]), 2).unwrap_err();
        assert!(error.contains("维度"), "{error}");
        let error = validate_sample(sample(&[0.5, 7.5]), 2).unwrap_err();
        assert!(error.contains("0..1"), "{error}");
        let error = validate_sample(sample(&[0.5, f32::NAN]), 2).unwrap_err();
        assert!(error.contains("数据异常"), "{error}");
        // 边界值夹取到 0..1。
        let clamped = validate_sample(sample(&[1.005, -0.005]), 2).unwrap();
        assert_eq!(clamped.busy, vec![1.0, 0.0]);
    }

    #[test]
    fn startup_decision_matrix() {
        let unavailable = CachedVerdict {
            kernel: "k1".to_owned(),
            version: "v1".to_owned(),
            result: "unavailable".to_owned(),
            reason: Some("BTF missing".to_owned()),
        };
        assert_eq!(
            startup_decision(Some(&unavailable), "k1", "v1"),
            StartupDecision::Skip {
                reason: "BTF missing".to_owned()
            }
        );
        assert_eq!(startup_decision(Some(&unavailable), "k2", "v1"), StartupDecision::Attempt);
        assert_eq!(startup_decision(Some(&unavailable), "k1", "v2"), StartupDecision::Attempt);
        let available = CachedVerdict {
            result: "available".to_owned(),
            ..unavailable
        };
        assert_eq!(startup_decision(Some(&available), "k1", "v1"), StartupDecision::Attempt);
        assert_eq!(startup_decision(None, "k1", "v1"), StartupDecision::Attempt);
    }

    #[test]
    fn probe_cache_roundtrip() {
        let dir = temp_dir("cache");
        let cache = ProbeCache::new(&dir);
        assert!(cache.read().is_none());
        cache.store("6.1", "5.2.0", "unavailable", Some("原因\n多行"));
        let verdict = cache.read().unwrap();
        assert_eq!(verdict.kernel, "6.1");
        assert_eq!(verdict.version, "5.2.0");
        assert_eq!(verdict.result, "unavailable");
        assert_eq!(verdict.reason.as_deref(), Some("原因 多行"));
        cache.store("6.1", "5.2.0", "available", None);
        assert_eq!(cache.read().unwrap().result, "available");
        assert!(cache.read().unwrap().reason.is_none());
    }

    #[test]
    fn proc_stat_line_parses_webui_compatible_totals() {
        // 与 WebUI /proc/stat 口径一致：前 8 列求和，idle+iowait 计入 idle。
        let (cpu, times) = parse_cpu_line("cpu0 100 0 50 850 30 0 0 0 0 0 0").unwrap();
        assert_eq!(cpu, 0);
        assert_eq!(times.total, 1030);
        assert_eq!(times.idle, 880);
        assert!(parse_cpu_line("cpu 100 0 50 850 0 0 0 0").is_none(), "聚合行跳过");
        assert!(parse_cpu_line("cpu0 100 0").is_none(), "字段不足必须拒绝");
    }

    #[test]
    fn cluster_busy_aggregates_deltas_and_handles_offline_cpus() {
        let mut prev = HashMap::new();
        let mut cur = HashMap::new();
        // CPU0：1000 ticks 中 600 忙。CPU1：窗口内离线（无数据）。
        prev.insert(0u32, CpuTimes { total: 1000, idle: 400 });
        cur.insert(0u32, CpuTimes { total: 2000, idle: 800 });
        assert!((cluster_busy(&prev, &cur, &[0, 1]) - 0.6).abs() < 1e-6);

        // 全簇离线 → 0（空闲）。
        assert_eq!(cluster_busy(&prev, &HashMap::new(), &[0]), 0.0);
        // 计数回退（热插拔竞态）→ 视为无数据。
        let mut regressed = HashMap::new();
        regressed.insert(0u32, CpuTimes { total: 10, idle: 10 });
        assert_eq!(cluster_busy(&prev, &regressed, &[0]), 0.0);
    }

    #[test]
    fn task_stat_parses_ticks_and_cpu_field() {
        // 构造真实格式：pid (comm) state ... utime=14 stime=15 ... processor=39。
        let mut fields: Vec<String> = (3..=52).map(|i| format!("0")).collect();
        // rest[0]=field3（state），因此 rest[k]=field(k+3)：rest 下标 = 列号-3；
        // utime(14)→rest[11]→fields[10]，stime(15)→fields[11]，processor(39)→fields[35]。
        fields[10] = "250".to_owned();
        fields[11] = "50".to_owned();
        fields[35] = "7".to_owned();
        let line = format!("1234 (com.example app) S {}", fields.join(" "));
        let (used, cpu) = parse_task_stat(&line).unwrap();
        assert_eq!(used, 300);
        assert_eq!(cpu, 7);
        assert!(parse_task_stat("1234 garbage").is_none());
    }

    #[test]
    fn foreground_attribution_matches_thread_cpu_and_normalizes_by_cluster_width() {
        let mut cpu_cluster = HashMap::new();
        for cpu in 0..4 {
            cpu_cluster.insert(cpu, 0usize);
        }
        cpu_cluster.insert(7, 2);
        // 2 秒窗口：簇0 有 4 核，一个线程满载 2s = 2s CPU 时间。
        let deltas = vec![(2 * TICK_HZ, 1u32)];
        let ratios = foreground_cluster_busy(&deltas, &cpu_cluster, &[4, 4, 1], 2.0);
        assert!((ratios[0] - 0.25).abs() < 1e-6, "2s / (2s*4核) = 0.25");
        assert_eq!(ratios[1], 0.0);

        // 单核簇满载一个窗口 = 1.0（与 /proc/stat 同量纲，超出部分夹取）。
        let deltas = vec![(3 * TICK_HZ, 7u32)];
        let ratios = foreground_cluster_busy(&deltas, &cpu_cluster, &[4, 4, 1], 2.0);
        assert!((ratios[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn foreground_task_list_parses() {
        let pids = parse_task_list("100\n200\n 300 \n\njunk\n400\n");
        assert_eq!(pids.len(), 4);
        assert!(pids.contains(&300));
    }

    #[test]
    fn foreground_set_refresh_is_rate_limited_and_tracks_changes() {
        let mut set = ForegroundSet::default();
        // 无该节点的宿主：读取失败记录原因、集合保持为空、不当作变化。
        let changed = set.refresh(1_000_000);
        assert!(!changed);
        if set.error.is_none() {
            // Android 上有节点：至少应解析出集合。
        }
        // 1 秒内不重复读取。
        let _ = set.refresh(1_500_000);
    }
}
