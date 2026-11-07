# Tempo 行为规格（实现冻结稿）

- 文档版本：v1.0（实现前冻结）
- 日期：2026-11-06
- 适用范围：`tempod`（调度守护）、`tempowerd`（充电/温控守护）、`tempoctl`（CLI）
- 实现约束：本规格只描述“输入 → 输出 / 时序 / 状态迁移”，实现代码不得引用任何既有第三方调度模块的源码、配置、脚本、资源或构建产物。

---

## 1. 术语与时间语义

| 术语 | 定义 |
|---|---|
| 时间戳 `ts_us` | 调用方提供的单调递增微秒数。控制器内部不做系统时间读取，所有判断只依赖该值，便于离线回放。 |
| 事件 | 由外部源产生的一次输入，含时间戳与负载/帧/触摸/屏幕/前台/温度信息。 |
| 决策 | 每个簇的一个频率上限 `ceiling_khz` 与原因。只有 `changed=true` 才允许写 cpufreq。 |
| 一次性 deadline | 为尚未结束的窗口（触摸抬频、帧 burst、静默确认）挂一个 `timerfd` 到期点；到期执行并撤销。 |
| 固定周期轮询 | 以固定间隔无条件读取 sysfs/proc 或固定 tick 计算。**禁止**。 |

**时间单调性**：调用方必须保证同一控制器实例的 `ts_us` 不减。乱序事件按到达顺序处理，不做插值。

---

## 2. 设备档案（`device.toml`）

### 2.1 结构

```toml
schema = 1
platform = "kalama"
sdk = 36

[[cluster]]
cpus = [0, 1, 2]
role = "efficiency"          # efficiency | balanced | prime
min_khz = 307200
max_khz = 2016000
capacity = 266
freq_khz = [307200, 441600, ...]
capacity_source = "sysfs"    # sysfs | estimated
```

约束：

1. `cluster` 至少 1 个，按首个 CPU 编号升序。
2. `cpus` 非空、全局不重复。
3. `min_khz <= max_khz`，`freq_khz` 严格升序且必须包含 `min_khz` 与 `max_khz`。
4. `capacity > 0`。`capacity_source=estimated` 表示该值由 `max_khz × 核数` 推导，仅用于相对权重，不承诺能效。
5. 角色分配：
   - 最低 capacity 簇 = `efficiency`；
   - 簇数 ≥ 3 且最高 capacity ≥ 1.2 × 次高 capacity 时，最高簇 = `prime`；
   - 其余 = `balanced`；
   - 簇数 = 2 时最高簇标记 `balanced`，不强行标 `prime`；
   - 簇数 = 1 时标记 `efficiency`。

### 2.2 启动校验

启动时读取 live 拓扑（`policy*`、`related_cpus`、`cpuinfo_min_freq`、`cpuinfo_max_freq`），与档案逐项比较簇数、CPU 归属、min/max。任何不一致：

1. 记录 `profile_mismatch`（含差异字段）；
2. 重新运行结构探测并原子替换 `device.toml`；
3. 用新档案继续，不套用旧表。

无法完成探测（无 cpufreq 或拓扑异常）→ 进入 **能力禁用**：不写任何 cpufreq，明确上报 `scheduler_disabled`。

---

## 3. 配置（`config.toml`）

### 3.1 字段与默认值

```toml
schema = 1

[daemon]
log_level = "warn"
profile_path = "/data/adb/tempo/device.toml"

[events]
runtime_quantum_us = 50000
demand_step = 0.10
screen_path = ""
frame_stride = 2
quiet_decay_ms = 60

[control]
quiet_ratio = 0.50
quiet_cap = 0.28
floor_enter_ratio = 0.05
floor_exit_ratio = 0.12
floor_settle_ms = 60
floor_exit_ms = 60
fg_fast_ms = 100
floor_residence_ms = 0
drop_ratio = 0.08
drop_factor = 4.0
touch_lift_ms = 150
touch_lift_level = 0.85
frame_slack_ms = 3.0
frame_burst_ms = 150
frame_burst_level = 0.95
reserve_ratio = 1.05
step_hysteresis = 0.03

[control.role_cap]
efficiency = 1.00
balanced = 1.00
prime = 0.85

[charge]
stop_at_percent = 80
resume_at_percent = 70
temp_stop_c = 45.0
temp_resume_c = 40.0
latch_per_plug = true

[thermal]
guard_c = 45.0
profile = "auto"

[ui]
language = "zh"
```

### 3.2 语义

| 字段 | 语义 |
|---|---|
| `runtime_quantum_us` | 每簇累计运行量达到该值才结算一次需求比率并比较，**不是定时器周期**；观察窗口越长噪声越小、上报越少。 |
| `demand_step` | 需求比率相对**上次已发布值**的移动量达到该阈值才发事件（默认 0.10）。与 `step_hysteresis`（控制侧换档滞回）解耦，禁止复用。 |
| `screen_path` | 屏幕状态来源（背光设备目录）。空表示运行时自动发现当前未熄灭的 backlight 面板。 |
| `frame_stride` | 帧事件合并：每 N 帧最多上报一次；N=1 表示不合并。 |
| `quiet_decay_ms` | 最后一次触摸后，至少经过该时间才允许进入静默路径。 |
| `floor_settle_ms` | 前台负载持续低于 `floor_enter_ratio` 的确认时间。 |
| `floor_residence_ms` | 在最低频时，**非事件驱动**的抬升需满足的最小驻留时间。触摸/帧/前台切换等事件驱动抬升不受限制。默认 0，避免交互回弹延迟。 |
| `quiet_ratio` | 负载→目标频率曲线的压缩系数，见 §4.2。 |
| `quiet_cap` | 前台整体空闲时的额外频率上限（占该簇可调范围的比例）。 |
| `floor_enter_ratio` | 前台阶梯/簇负载低于该比例视为“无有效前台工作”（低频锁存进入阈值）。 |
| `floor_exit_ratio` | 低频锁存退出阈值，必须大于 `floor_enter_ratio`；两者之间为滞回带，静态页面的微动落在带内不会触发抬升。 |
| `floor_exit_ms` | 快速滤波后的前台负载需持续高于 `floor_exit_ratio` 的时间，才确认“真实突发”并退出低频锁存。确认时刻若最新观测已回落到阈值以下，则视为单点噪声并取消退出。 |
| `fg_fast_ms` | 前台负载快速滤波的时间常数；仅用于“微动 vs 突发”的退出确认，不参与进入判定。 |
| `drop_ratio` | 目标比例低于该值时直接吸附到 `min_khz`。 |
| `drop_factor` | 向下换档的滞回阈值为 `step_hysteresis / drop_factor`（快下慢上）。 |
| `touch_lift_ms` / `touch_lift_level` | 触摸/前台切换的一次性抬频窗口与目标比例。 |
| `frame_slack_ms` | 帧渲染时间允许超出预算的余量。 |
| `frame_burst_ms` / `frame_burst_level` | 帧超时后的一次性 burst 窗口与目标比例。 |
| `reserve_ratio` | 高负载段的顶部余量（>1 表示允许超过线性满档，但仍受角色上限约束）。 |
| `step_hysteresis` | 向上换档的最小比例差。 |
| `role_cap` | 按角色（不是 CPU 编号）的硬上限，跨簇数通用。 |
| `thermal` | 热上限由 `tempowerd` 以事件方式下发，调度侧只做频率封顶。 |

### 3.3 校验与错误处理

- `schema != 1` → 拒绝加载。
- 数值越界、`resume_at_percent >= stop_at_percent`、`temp_resume_c >= temp_stop_c`、`floor_enter_ratio > drop_ratio`、`floor_exit_ratio <= floor_enter_ratio` → 拒绝加载并保留上一份有效配置。
- 配置损坏时：`tempod` 回退到上一份内存有效配置，写 `config_rejected` 日志（含字段与原因），不退出。
- 配置热更新：`inotify` 事件触发，整文件解析、校验、原子替换。禁止周期读取文件。

---

## 4. 控制算法（单模式）

### 4.1 输入

```text
DemandSample {
  ts_us,
  cluster_busy[i],       // 0..1，每簇总忙碌比例
  foreground_busy[i],    // 0..1，仅前台树线程
  foreground_active      // 是否存在可识别的前台进程
}

FrameSample {
  ts_us,
  render_ms,             // 本帧从开始渲染到提交的耗时
  budget_ms,             // 当前刷新模式对应的帧预算
  foreground             // 是否前台进程产生
}
```

事件：`ScreenOn/Off`、`TouchContact`（按下与移动）、`TouchRelease`、`ForegroundChanged`、`ThermalCap`、`Deadline`。

`ScreenOn/Off` 由守护进程通过 `inotify` 监听背光设备目录的 `bl_power` 属性变化产生（`0` 为亮屏，非 `0` 为熄灭），不使用任何定时采样。息屏信号同时写入 eBPF 门控开关：息屏期间探针仍累计需求但**不发布事件**，守护进程随之断开所有 deadline 并无限阻塞。

### 4.2 负载 → 目标比例

设 `d = clamp(cluster_busy, 0, 1)`，`q = quiet_ratio`，`r = reserve_ratio`：

```text
g(d) = q * sqrt(d)                              , d <= 0.5
g(d) = g(0.5) + (d - 0.5) * 2 * (r - g(0.5))    , d > 0.5
```

- 连续、单调、凹形：低负载段压缩，高负载段线性留余量。
- `g(0) = 0`；`g(1) = r`。
- 若低频锁存 `floor_low` 已置位，则 `g(d) = min(g(d), quiet_cap)`；其中 `prime` 簇直接取 0（不响应后台/空闲前台负载，只参与前台高负载、暖启动与帧 burst）。锁存置位后仅由“微动 vs 突发”判据解除（见 §4.5），静态页面的窄带抖动不会反复进出。

### 4.3 一次性修正与封顶（按优先级从低到高）

1. 基础目标 `g(d)`；
2. 前台空闲上限 `quiet_cap`；
3. 触摸抬频：`level = touch_lift_level`，窗口 `touch_lift_ms`；
   - 触摸来源只作用于 `efficiency`/`balanced` 簇；
   - 前台切换/亮屏来源作用于全部簇；
4. 帧 burst：`level = frame_burst_level`，窗口 `frame_burst_ms`，作用于全部簇；
5. 角色上限 `role_cap[role]`；
6. 热上限 `thermal_cap`（外部下发的比例，缺省 1.0）。

最终比例 `f` 经频率表吸附：取 `min_khz` 到 `max_khz` 间不大于 `min + f*(max-min)` 的最大可用频率；低于 0 时取 `min_khz`。

### 4.4 换档滞回

- 向上：`f - f_current >= step_hysteresis` 才写；若当前在 `min_khz` 且 `floor_residence_ms > 0` 且不是事件驱动原因（触摸/帧/前台/亮屏），需满足驻留时间。
- 向下：`f_current - f >= step_hysteresis / drop_factor` 才写；`f < drop_ratio` 时直接写 `min_khz`。
- 目标频率与当前频率相同（吸附后）→ `changed=false`，不写 sysfs。
- 静默路径与息屏直接强制 `min_khz`，不受滞回限制。

### 4.5 静默路径（Quiet Path）

**进入条件（全部满足）**：

1. 屏幕亮；
2. 无活动的触摸抬频、无活动的帧 burst；
3. 距最后一次触摸接触 ≥ `quiet_decay_ms`；
4. 全部簇的 `foreground_busy <= floor_enter_ratio` 已持续 ≥ `floor_settle_ms`（进入前必须至少收到一次 `DemandSample`）；满足后置位低频锁存 `floor_low`；
5. 屏幕亮起或前台切换后，至少收到一次新的 `DemandSample`；亮屏/前台切换会清空锁存与滤波器。

**进入时刻** = `max(最后触摸 + quiet_decay_ms, 低负载起点 + floor_settle_ms)`；若有抬频/burst，则再顺延到其窗口结束。

**动作**：每个簇写 `min_khz`。

**微动 vs 突发（退出判据）**：
- 进入使用**瞬时**前台比率：静态页面上的窄带抖动（本机实测 3% 左右）能稳定落锁，不要求等滤波器收敛。
- 退出使用**快速滤波**（`fg_fast_ms` 时间常数，按真实采样间隔计算）加确认窗口：滤波值需持续 ≥ `floor_exit_ratio` 达 `floor_exit_ms` 才退出；确认时刻若最新观测已 < `floor_exit_ratio`，视为单点噪声并取消。
- 触摸、帧超时、前台切换、亮屏属于事件驱动中断，直接解除锁存或抬频，不等确认窗口。

**退出条件（任一）**：

- 触摸接触（按下或移动）→ 立即抬频（锁存保留，避免单次点按把 prime 拉起）；
- 前台切换；
- 快速滤波后的任意簇 `foreground_busy >= floor_exit_ratio` 持续 ≥ `floor_exit_ms` 且最新观测仍未回落；
- 前台帧 `render_ms > budget_ms + frame_slack_ms`；
- 从息屏恢复亮屏。

**退出动作**：

- 触摸 → 一次性抬到 `touch_lift_level`（仅 efficiency/balanced）；
- 帧超时 → 一次性 `frame_burst_level`（全部簇）；
- 前台切换/亮屏 → 一次性 `touch_lift_level`（全部簇）；
- 负载上升 → 直接按 §4.2 计算，不做逐档爬升。

**防回弹**：

- 抬频窗口内不进入静默路径；
- 帧超时会清空“低负载起点”，burst 结束后需重新满足 `floor_settle_ms`；
- 换档滞回避免相邻档反复写；
- 默认 `floor_residence_ms = 0`，事件驱动抬升不受最低频驻留阻塞。

**零轮询边界**：进入静默后，若没有抬频/burst 窗口，`next_deadline = None`，事件循环 `epoll_wait(-1)` 无限阻塞；息屏同样不挂 deadline。

### 4.6 帧截止与 burst

- 仅处理 `foreground=true` 的帧。
- `render_ms > budget_ms + frame_slack_ms` → 进入 burst，静默状态解除，清空低负载起点。
- burst 窗口结束后按当前负载重新决策；不保留 hold 状态，不做逐档爬升。
- `frame_stride` 只影响事件上报频率，不改变判定逻辑。

### 4.7 息屏

- 收到 `ScreenOff`：立即对全部簇写 `min_khz`，清空抬频/burst，静默状态置真。
- 息屏期间只响应充电/热/屏幕事件；不挂任何固定 timer。
- `ScreenOn`：解除静默，挂一次 `touch_lift_ms` 的亮屏抬频（全部簇），清空低负载起点，等待新的 `DemandSample`。

---

## 5. 充电状态机（`tempowerd`）

### 5.1 输入

`ChargeSample { ts_us, percent, temp_c, plugged }`，由 `power_supply` uevent 驱动。

### 5.2 状态

| 状态 | 含义 |
|---|---|
| `charging` | 正常充电 |
| `stopped_percent` | 达到停充阈值而停充 |
| `stopped_temp` | 达到温度阈值而停充 |
| `latched` | 本次插电已因电量阈值停充，禁止同次插电恢复 |

### 5.3 迁移

1. `plugged=false`：清除 `latched` 与 `stopped_temp`，停止态复位；不发恢复命令。
2. `temp_c >= temp_stop_c`：进入 `stopped_temp`，发 `Stop`。
3. `temp_c <= temp_resume_c` 且处于 `stopped_temp`：退出温度停充；若未 latch 且 `percent <= resume_at_percent`，发 `Resume`。
4. `percent >= stop_at_percent`：发 `Stop`；`latch_per_plug=true` 时置 latch。
5. 已停充且 `!latched` 且 `!stopped_temp` 且 `percent <= resume_at_percent`：发 `Resume`。
6. 任何温度停充都不置 latch；温度恢复后仍需满足电量恢复条件。
7. 每次迁移只发一个动作；无变化时不写节点、不写日志。

### 5.4 节点与写入口径

- 只写设备实际存在的充电控制节点；启动时能力探测，缺失则记录 `charge_control_unavailable` 并只读监测。
- 写入失败：回滚到上一动作，记录节点与错误码，30s 内不重试同一动作（由一次性 deadline 驱动，非轮询）。

---

## 6. 温控

- `tempowerd` 订阅 thermal uevent，按 `guard_c` 计算调度侧上限比例：
  - `temp < guard_c - 8` → 1.00；
  - `temp >= guard_c + 4` → 0.45；
  - 之间线性下降。
- 通过 UDS 或内部事件把 `thermal_cap` 下发给 `tempod`；`tempod` 只做封顶，不接管厂商热管理。
- 温度回落按同一曲线恢复；恢复是事件驱动，不周期轮询。

---

## 7. 进程、IPC 与文件

### 7.1 目录

```text
/data/adb/tempo/
├─ config.toml         # 用户配置（可热更新）
├─ device.toml         # 探测生成，只读
├─ run/control.sock    # UDS，0700 目录 / 0600 socket
├─ state/              # 充电锁存等持久状态
└─ log/                # 环形日志
```

### 7.2 UDS 协议

- JSON Lines，每行一个请求对象 `{"cmd":"status"}`，响应一行 `{"ok":true,...}`。
- 命令：`status`、`profile.get`、`config.get`、`config.validate`、`ping`。
- 仅 root 可访问；连接不写入任何调度状态。
- 未知命令返回 `{"ok":false,"error":"unknown_command"}`，不断连接。

### 7.3 日志

- 默认 `warn`；只记录状态变化、配置拒绝、能力缺失、节点写失败。
- 不逐事件、不逐帧格式化；日志落盘走环形缓冲。

---

## 8. 跨 SoC 能力边界

| 能力 | 自动推导 | 需实机标定 |
|---|---|---|
| 簇数、簇归属、频率表、min/max | 是（`policy*`、`related_cpus`、`scaling_available_frequencies`，缺失回退 `time_in_state`） | — |
| capacity 与角色 | 多数（读 `cpu_capacity`；缺失则估算并标记） | 估算值下的能效/手感 |
| 角色上限向量长度 | 是（按角色表，不按簇数写死） | — |
| 触摸窗口、帧余量、静默阈值 | — | 是（屏幕刷新率、渲染管线、调度延迟差异） |
| 热区/充电节点语义 | 部分（能力探测） | 是（厂商节点命名与量纲） |
| eBPF 可用性 | 部分（tracepoint/uprobe 探测） | 是（内核符号/权限差异） |
| 功耗/帧稳定性承诺 | — | 是（仅同机同 ROM 可验证） |

**不可承诺**：

- 无法仅凭静态探测得知某频率的真实能效；
- 无该机型时无法验证触摸延迟、帧截止、热行为；
- 无法适配禁止写 `scaling_max_freq` 或 governor 不响应的内核。

**能力不足处理**：eBPF tracepoint/uprobe 不可用 → 不启用调度，明确上报 `scheduler_disabled`；**不做降级采样**，不引入固定轮询。

---

## 9. 验收口径（实现自检）

1. **最低频可达**：亮屏静态 10min，每簇 `time_in_state` 最低频占比 ≥ 95%（首 60s 稳定期除外）。
2. **无负载抬频**：连续 10min 无触摸、前台无有效负载，高于最低频的连续区间 0 个 >100ms，抬频 ≤1 次/10min 且可解释。
3. **掉频响应**：最后一次触摸/帧超时事件 → 回到最低频 ≤150ms（P95），无 >300ms 尾部。
4. **抖动**：静态 1s 内换档 ≤1 次；滑动 ≤4 次；无相邻档来回 >2 次。
5. **静默空闲**：进入静默后 `next_deadline=None`，10min 内守护唤醒 <0.2 次/s，CPU <0.2% 单核。
6. **功耗**：同机 A/B，静态阅读 ≤0.90× 基线；日常/视频 ≤1.00×；游戏 jank 不高于基线。
7. **零轮询**：代码中不存在固定间隔 `sleep`/定时器循环；所有 deadline 均为一次性窗口。

---

## 10. 实现自检映射

| 规格条目 | 代码位置 | 测试 |
|---|---|---|
| 配置默认/校验 | `crates/tempo-core/src/config.rs` | `config_defaults_roundtrip`、`config_rejects_bad_ranges` |
| 设备档案校验/角色 | `crates/tempo-core/src/profile.rs` | `profile_parses_three_clusters`、`profile_rejects_unsorted_freqs` |
| 负载曲线/角色上限/热上限 | `crates/tempo-core/src/controller.rs` | `high_load_respects_prime_cap`、`thermal_cap_lowers_ceiling` |
| Quiet Path | `controller.rs::quiet_ready` | `quiet_path_reaches_min_freq`、`quiet_path_no_raise_for_ten_minutes` |
| 触摸抬频/回落 | `controller.rs::on_touch` | `touch_lift_then_floor_within_150ms` |
| 帧 burst | `controller.rs::on_frame` | `frame_overrun_triggers_burst` |
| 零 deadline | `controller.rs::next_deadline` | `quiet_state_has_no_deadline` |
| 充电状态机 | `crates/tempo-core/src/charge.rs` | `charge_stop_latch_and_temp_resume` |
| 跨簇数 | `controller.rs` | `two_and_four_cluster_profiles_work` |

---

## 11. 变更纪律

- 本文件冻结后，任何控制语义变更必须先改本文件、补测试，再改实现。
- 参数默认值来自本机标定；不得从任何既有模块的配置文件复制字段、结构或成组默认值。
