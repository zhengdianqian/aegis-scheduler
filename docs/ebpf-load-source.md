# eBPF 负载采集（精度增强包）：依赖与启用条件

适用版本：Aegis 5.2.0 及以上。本文档描述 `LoadSource` 抽象、eBPF 精度增强包
（`EbpfSource`）的启用条件、标准回退（`ProcStatSource`）的兜底语义，以及诊断方法。
本轮只换数据源：决策层（EMA、事件优先级、帧救援、仲裁锁定期）一行未改。

## 1. 架构总览

```
                 ┌────────────────────────── LoadSource 接口 ──────────────────────────┐
                 │  init / read / availability / close / set_screen / foreground_ids  │
                 └───────────────┬──────────────────────────────────┬─────────────────┘
                                 │ 默认启用                          │ 任何一步失败即回退
                 ┌───────────────▼───────────────┐      ┌───────────▼──────────────────┐
                 │ EbpfSource（精度增强包）        │      │ ProcStatSource（标准回退）    │
                 │ tempo.bpf.o                    │      │ /proc/stat 每 CPU 计数差值    │
                 │ tracepoint/sched/sched_switch  │      │ + top-app 线程 /proc/tid/stat│
                 │ BPF 侧：只记录切换时间差、按 CPU │      │ timerfd 按 quantum 定时采样   │
                 │ 聚合到 Map（无复杂计算）         │      │                              │
                 │ 用户态：读 Map → 归一化 0..1     │      │ 归一化 0..1（同 WebUI 口径）  │
                 └───────────────┬───────────────┘      └───────────┬──────────────────┘
                                 └──────────┬───────────────────────┘
                                            ▼
                        LoadSample{busy, foreground, ...}（同量纲、同采样周期）
                                            ▼
                          决策层 controller.on_demand（零改动）
```

* 两条路径输出**同量纲**：每簇 0..1 占用率（/proc/stat 口径 = 前 8 列求和为
  total、idle+iowait 计入 idle），与 WebUI 的占用采样逐字节一致。
* 同采样周期：都按 `events.runtime_quantum_us`（默认 250 ms）节拍产出。
* 帧感知数据源（Aegis Frame / aegis_frame.rs）与负载源完全独立，帧感知默认
  关闭的状态不受本任务影响；本轮不引入 eBPF 帧采集。

## 2. eBPF 精度增强包启用条件（全满足才启用）

| 条件 | 检查点 | 不满足时 |
|---|---|---|
| 内核编译了 BPF | `CONFIG_BPF=y`、`/sys/kernel/btf/vmlinux` 存在（BTF） | 加载失败 → 回退 |
| tracepoint 可用 | `tracepoint/sched/sched_switch`（标准内核均提供） | attach 失败 → 回退 |
| 加载权限 | Aegis 以 root 运行（KernelSU 模块 service.sh）；需 `CAP_BPF`/`CAP_PERFMON` 或等价特权；SELinux 未被本模块修改，`untrusted_app` 限制不适用于 root 守护 | EPERM → 回退 |
| 探针对象已构建 | `tempo.bpf.o` 编入二进制（`scripts/build-probe.ps1`，发布包恒已构建） | "probe object was not built" → 回退 |
| 未被探测缓存判定不可用 | 见 §3 | 跳过加载 → 直接标准源 |

**内核版本要求**：5.10+（Android 12+ GKI 均满足）。eBPF 帧采集不在本任务范围。

## 3. 启动流程与探测缓存

启动时（`event_loop::run`）：

1. 读取 `/data/adb/tempo/ebpf-probe.cache`（key=value：`kernel` / `version` /
   `result` / `reason`）。
2. `result=unavailable` **且** `kernel=uname -r` **且** `version=当前版本` →
   跳过加载尝试（避免每次开机重复踩同一个失败），直接标准回退，原因进日志与诊断。
3. 否则现场尝试 `EbpfSource::open`（加载 tempo.bpf.o → 挂 sched_switch →
   打开事件环与 Map）：
   * 成功 → 写缓存 `result=available`，eBPF 精度增强包生效；
   * 失败 → 写缓存 `result=unavailable` + 失败原因，**静默回退**标准源并记录日志。
4. 内核变化（OTA/刷机）、Aegis 版本变化都会使缓存失效 → 重新探测。
5. 标准源 `/proc/stat` 打开也失败（极罕见）→ 才走旧版“无负载源则不调度”。

降级是一等公民：回退后守护照常接管/调度，行为与标准 Aegis 一致，用户无感。

## 4. 运行时安全阀（半坏源自动永久回退）

* eBPF 路径连续 **5 次**（`MAX_CONSECUTIVE_FAILURES`）`read()` 返回错误或
  样本数据异常（维度与簇数不符、占用率 NaN/越界）→ **永久回退**标准源：
  * 关闭 eBPF 会话（ring fd 由内核自动从 epoll 摘除）；
  * 启动标准源 timerfd 采样节拍（按当前屏幕状态）；
  * 日志（WARN）+ 诊断字段写明原因；切换后不再回到 eBPF（本运行期内）。
* `Ok(None)`（稳态无新观测，探针按 2% 步进合并发布）**不是**故障，不计数。
* 标准源自身读取失败：计数并继续重试（标准源是兜底底线，不设第三级回退），
  保留上一拍需求，避免数据断流导致频率决策跳变。

## 5. 标准回退源的实现要点

* **整簇占用**：读 `/proc/stat`，逐 CPU 计算 (total-idle)/total 差值并按簇求
  和聚合；窗口超过 10 个量子（`RESEED_WINDOW_FACTOR`）视为停摆后重新播种，
  息屏挂起/开屏第一拍不会产出跨窗口均值。
* **前台占用**（静态限顶的判据）：读 `/dev/cpuset/top-app/tasks`（至少每秒一次）
  + 各前台线程 `/proc/<tid>/stat` 的 utime+stime 差值，按线程最近运行 CPU
  （processor 字段）归因到簇，除以（簇核数 × 窗口秒数）→ 与整簇同量纲。
  Android `USER_HZ=100`，按 tick 常量换算。逐线程读取按 1 s 节拍执行
  （`FOREGROUND_SAMPLE_US`），整簇读取仍按量子节拍。
* 热插拔离线的 CPU 在窗口内无贡献；全簇离线按 0（空闲）处理。
* 息屏：标准源 timerfd 停摆（不制造唤醒），开屏重新播种。

## 6. 可观测性（诊断包/日志字段）

`tempoctl status`（或 WebUI 总览页“负载源”卡片，悬浮显示原因）：

| 字段 | 含义 |
|---|---|
| `load_source` | `"ebpf"`（精度增强包）/ `"proc_stat"`（标准回退）/ `"unavailable"` |
| `load_source_reason` | 回退原因：启动不可用（含 attach/BTF 错误）、缓存命中（含原始原因）、运行时安全阀触发说明；eBPF 正常时为 `null` |
| `probe.state` | `"ready"`（eBPF 活动）/ `"disabled"`（回退） |
| `probe.error` | eBPF 不可用或运行时切换原因 |
| `screen.source` | 参与判定的面板清单（如 `panel0-backlight+panel1-backlight`） |

日志（`/data/adb/tempo/Aegis.log`）关键行——即使 `log_level=off`，下列启动/切换
摘要也会随 stdout 落盘：

```
Aegis: 负载源：精度增强包（eBPF）已启用，内核 <release>
Aegis: 负载源：精度增强包不可用，静默回退标准 /proc/stat 采集；<原因>
Aegis: 负载源：探测缓存命中（内核 <release> / 版本 5.2.0 均未变化），跳过 eBPF 加载；<原因>
Aegis: 运行时安全阀：连续 5 次读取失败/数据异常，已永久回退标准 /proc/stat 源（最近：<错误>）
```

## 7. 明确不做（本任务边界）

* 不修改 SELinux 策略，不承诺真机一定能加载 eBPF——回退路径就是为此准备的。
* 不做真机性能调优，BPF 程序开销需人工实测（见验证清单）。
* 不重写决策逻辑，不引入 PID 控制器；不改变帧感知默认关闭的现状；
  不在本轮引入 eBPF 帧采集。
