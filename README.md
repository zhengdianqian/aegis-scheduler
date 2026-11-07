# Aegis

**自适应 CPU 调度 + FAS 帧感知 + 充电控制**，面向骁龙 8Gen2 深度标定，并对其他 SoC 做尽力适配。

Aegis 是一个完全独立实现的 Android 调频模块（KernelSU / Magisk）。一套参数覆盖冷启动、应用内热切换、最近任务回切、突然触摸、通知弹窗等全部场景；后台常驻 100% 事件驱动，零周期轮询，息屏不阻止 Doze。

- 许可证：**GPL-3.0-or-later**
- 版权：`Copyright (c) 2026 默默的阿文`
- 模块 id：`aegis_scheduler`｜对外显示名：**Aegis**
- 版本：`v2.1.1`（`versionCode=5`）
- 模块描述：`自适应 CPU 调度+FAS + 充电控制`
- 首发承诺机型：骁龙 8Gen2（`kalama` / `babylon`）

> 本项目自 2026 年起由 **默默的阿文** 独立编写，不是任何现有项目的 fork，不包含任何第三方项目的源码、注释、命名、配置模板、脚本模板或构建产物。

---

## 特性

| 能力 | 说明 |
| --- | --- |
| **自适应单档调度** | 统一的自适应模型，不区分游戏/日常/省电档位，按实时负载自动升降频 |
| **只写下限，永不封顶** | 只写 `scaling_min_freq`；`scaling_max_freq` 始终保持硬件最大值，冷启动与重负载永远不会撞顶 |
| **档位渐进** | 抬频三级递进（约 20 ms/级，60 ms 到达目标档），回落按滞回逐级下降（单步最多减半），不会从最低频直跳高频 |
| **启动边沿** | 检测到应用启动（静默→前台、前台负载≥0.30）时，三簇地板一次性抬到 100% 并保持 800 ms，随后按 45 ms/级回落，覆盖首帧、dex/IO 与启动动画 |
| **FAS 帧感知**（默认关闭） | 仅对用户在 WebUI 手动勾选的应用生效，前台切换时解析一次并缓存，事件触发而非轮询 |
| **全事件驱动** | BPF ring、屏幕 inotify、evdev 触摸、FAS 结果、一次性 `timerfd`；静默时阻塞在 `epoll_wait`，无任何固定周期定时器 |
| **开机安全** | `sys.boot_completed` 未置 1 前绝不写任何 cpufreq 节点（最多等 180 s，超时退出保留厂商默认）；开机后再留 10 s + 守护内部稳定期 |
| **看门狗兜底** | 守护异常退出（OOM / 崩溃 / 手动 kill）后 60 s 内自动还原原始 governor/min/max |
| **充电控制** | 上限 / 回差复充 / 100% 插电锁存 / 预约时段（支持跨零点）/ 温度停充，优先级「温度 > 上限/锁存 > 预约」 |
| **可逆** | 原始值备份在 `/data/adb/tempo/orig/`，`restore.sh` 一键还原；卸载自动执行恢复 |
| **不碰厂商 HAL** | `perfservice`、`perf-hal`、`miuibooster`、`mi_thermald`、`android.thermal-hal` 全部保持运行 |

## 组成

| 组件 | 说明 |
| --- | --- |
| `bin/Aegis` | 常驻守护，单一 epoll 事件循环，零固定周期定时器，空闲 CPU 接近 0 |
| `bin/tempoctl` | 命令行控制与离线回放（`status` / `config.apply` / `restore` / `power.get` / `log.tail`） |
| `webroot/` | 内置 WebUI（KernelSU / Magisk 模块页内嵌，也可浏览器打开），与 APP 内置资源为同一份 8 文件构建 |
| `Aegis-v2.1.1.apk` | 独立控制端（包名 `com.aegis.scheduler`，WebView 壳，读写同一 socket） |

> 同时只保留一个控制端（`com.aegis.scheduler`）。若设备上存在旧包名的控制端请先卸载，避免两个进程同时写配置。

## 工作原理

1. **探针**：BPF 统计各簇运行时间与需求，按 50 ms 观察窗口累积；只有需求变化超过独立阈值才发布事件，避免事件率虚高。
2. **静默判据**：进入静默用瞬时比率（`floor_enter_ratio` 持续 `floor_settle_ms`），退出用快速滤波并要求持续窗口（`floor_exit_ratio` / `floor_exit_ms`），单点尖峰被过滤；触摸、帧超时、前台切换、亮屏会立即打断静默并抬频。
3. **启动边沿**：满足 `was_quiet && foreground_active && max(foreground_busy) ≥ 0.30 && 距上次启动 ≥ 800 ms` 时，控制器清空静默/回落状态，三簇地板一次性抬到 `launch_lift_level=1.00`，保持 `launch_lift_ms=800`，之后按 `touch_decay_ms=45` 逐级回落。
4. **写地板**：静默时把 `scaling_min_freq` 下探到厂商 min；触摸/启动/帧突发时短窗口抬高地板，再按档位逐级回落。floor 是“允许下探的目标”，governor 仍可在 `[floor, 硬件 max]` 内自由升频。
5. **硬约束**：每次写入前校验 `live_max >= 生效硬件上限`，不满足即拒绝并记录 `ERROR`；quiet 期每 3 s 回读一次地板，纠正厂商 perfservice 私自抬高的值，被 QoS 拒绝时退避 30 s，不与厂商互相拉扯。

## 安装

1. **卸载旧模块**：模块管理器里删除或禁用历史调度模块（本模块会自动给旧目录打 `disable`，但推荐手动卸载以免残留两份 WebUI）。旧目录请保留而非删除，便于回滚。
2. 通过 **KernelSU / Magisk 刷入** `Aegis-v2.1.1.zip`。
3. 重启，等待 `sys.boot_completed=1`。开机阶段不写任何 cpufreq；开机完成并稳定后才接管。
4. 打开 **WebUI**（模块页面内嵌），或安装 `Aegis-v2.1.1.apk`。
5. 首次运行 APP 需在 KernelSU「超级用户」页为 **Aegis** 打开开关。未授权时界面提示「控制服务未就绪」，仅控制命令不可用。

## 使用

### WebUI

- 首页：实时频率、电量、温度、充电状态、当前前台包名
- 调度：静默阈值、抬频档位、回落速度（修改后 `config.apply`，必要时重启守护）
- FAS：总开关 + 应用勾选列表（写入 `/data/adb/tempo/fas.json`，列表为空即完全关闭）
- 充电：上限、恢复阈值、停充/恢复温度、预约时段
- 日志：抽屉式查看，可关闭，行数有上限，切换参数后自动刷新

### 命令行

```sh
# 状态
/data/adb/modules/aegis_scheduler/bin/tempoctl status

# 还原原始调频
/data/adb/modules/aegis_scheduler/bin/tempoctl restore \
  --config /data/adb/tempo/config.toml \
  --profile /data/adb/tempo/profile/babylon.toml \
  --socket /data/adb/tempo/run/control.sock
```

Unix socket：`/data/adb/tempo/run/control.sock`，一行 JSON 请求 / 一行 JSON 响应。
命令：`status`、`config.get`、`config.apply`、`power.get`、`log.tail`、`scheduler.restore`。

## 配置

- 主配置：`/data/adb/tempo/config.toml`
- 设备档案：`/data/adb/tempo/profile/babylon.toml`
- FAS 侧车：`/data/adb/tempo/fas.json`
- 预约充电：`/data/adb/tempo/charge_plan.json`

### `[control]` 关键字段

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `floor_enter_ratio` | 0.05 | 进入静默的瞬时比率上限 |
| `floor_exit_ratio` | 0.12 | 退出静默的滤波比率（必须大于进入值） |
| `floor_settle_ms` | 60 | 进入静默需保持的时间 |
| `floor_exit_ms` | 60 | 退出静默需保持的时间 |
| `fg_fast_ms` | 100 | 退出判据的快速滤波时间常数 |
| `touch_lift_ms` | 150 | 手指抬起后保持抬频的窗口 |
| `touch_lift_level` | 0.45 | 抬频目标（簇跨度的比例，中档不到顶） |
| `touch_ramp_ms` / `touch_ramp_stages` | 60 / 3 | 三级抬频，约 20 ms 一级 |
| `touch_decay_ms` | 45 | 回落每一级的最小间隔 |
| `drop_factor` | 2.0 | 单次回落最多降到当前的 1/2 |
| `launch_lift_ms` / `launch_lift_level` | 800 / 1.00 | 应用启动边沿的短窗口高地板 |
| `frame_burst_ms` / `frame_burst_level` | 150 / 0.70 | 帧超时突发的抬频窗口与目标 |
| `parked_headroom` | 0.08 | 静默余量，保证上限始终高于地板，绝不锁最低频 |

### `[fas]`

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `enabled` | `false` | FAS 总开关 |
| `apps` | `[]` | 启用 FAS 的包名集合，为空即完全关闭 |
| `frame_budget_ms` | 16.0 | 帧预算，95th percentile 超过它时抬频 |

### 充电与温控

| 段 | 字段 | 说明 |
| --- | --- | --- |
| `[charge]` | `stop_at_percent` / `resume_at_percent` | 充电上限与回差恢复阈值 |
| `[charge]` | `temp_stop_c` / `temp_resume_c` | 停充 / 恢复温度（带回差） |
| `[charge]` | `latch_per_plug` | 100% 满电锁存，仅当次插电有效，重插或重启即清除 |
| `[thermal]` | `guard_c` / `profile` | 触达温度上限时压低簇上限，回落后逐级恢复 |

## 实测数据（8Gen2 / babylon，v2.1.1）

**静态 3 分钟（亮屏静置，181 s，`time_in_state` 增量）**

| 簇 | 平均频率 | 最低档占比 | 最高档占比 |
| --- | --- | --- | --- |
| c0 | **455.8 MHz** | 82.25%（307200） | 4.76%（2016000） |
| c1 | **596.9 MHz** | 92.43%（499200） | 3.19%（2592000） |
| c2 | **635.9 MHz** | 97.50%（595200） | 2.22%（2342400） |

- 静态电流中位 **79 mA**、均值 **85.7 mA**（17 × 10 s 有效采样，`input_suspend=1` 真放电；首个采样点为充电切换瞬态，已剔除），4.33 V 约 **0.37 W**。
- 守护自身 CPU：9 ticks / 181 s ≈ **0.05% 单核**；voluntary ctxt 增量 404 / 181 s ≈ **2.2 次/秒**。
- 静置期 `max_freq` 全程保持 2016000 / 2592000 / 2342400，未被封顶。
- 与官方调度同场景对照：官方平均频率 772.4 / 611.8 / 666.5 MHz、电流中位 151 mA。v2.1.0 A/B 口径频率差 −1.4% / −2.6% / −1.3%；v2.1.1 本轮复测 c0 地板由厂商 QoS 放行到 307200（v2.1.0 时为 672000），因此 c0 平均频率 455.8 MHz **不能与上一轮直接横比**。中位电流 79 mA 仅为单轮记录，**不作为功耗结论，不宣称“功耗大幅下降”**。

**冷启动（force-stop 确认无 pid 后，各 3 次）**

| 应用 | TotalTime | 总帧 | Janky | 90th | 99th |
| --- | --- | --- | --- | --- | --- |
| 设置 | 398–424 ms | 59 | 2（3.39%） | 23–32 ms | 69–77 ms |
| 微信 | 442–649 ms | 54–58 | 25.86%–26.47% | 250–350 ms | 300–550 ms |
| Chrome | 650–777 ms | 113–199 | 0.90%–1.71% | 12 ms | 250–350 ms |

微信冷启动仍是弱项：官方同场景基线为 22.4%–23.4%（90th 250 ms），本版 25.9%–26.5%、90th 250 ms。启动窗口内 c1/c2 已处于硬件上限，把 lift 降到 200 ms / 0.50 反而得到 25.00%，说明剩余卡顿来自微信主线程、IO 与 MIUI `perfservice`（logcat 可见 `Skipped 31/52 frames`、`onResume took 156ms`、`MiuiPerfServiceClient ... Slow handle traversal`），不是 CPU 地板不足。**目标“<10% / 90th<100ms”未达成，不虚报。**

**热切换**

| 场景 | 指标 |
| --- | --- |
| 微信应用内 6 次底栏切换 | 232 帧，Janky 12（**5.17%**），50/90/99 = 5 / 24 / 129 ms |
| 最近任务卡片回切设置 | 87 帧，Janky 3（**3.45%**），50/90/99 = 7 / 14 / 65 ms |

**突发与 Doze**

- 通知弹出后峰值可达各簇硬件上限，但超过最低档的累计时间仅 **0.27 s / 0.15 s / 0.04 s**，单次抬频窗口 ≤ 0.5 s。
- 息屏 60 s：`dumpsys deviceidle` → `mState=IDLE`；守护累计 **0 ticks**，全部事件源阻塞于 `epoll_wait`。

## 已知限制

- **c0 地板受厂商 PM QoS 限制**：`scaling_min_freq` 被固定在 672000，写 307200 会被立即回读为 672000。这是静态频率不能更低的主因；绕过需停用 perfservice，已明确禁止。
- **微信冷启动 25.9%–26.5%（90th 250 ms）**：与厂商首帧渲染路径、微信主线程/IO 相关，短窗口抬频无法消除；官方基线亦为 22.4%–23.4%。
- **静默期存在写抖动**：厂商 perfservice 会周期性抬高 c1/c2 地板，对账后写回，实测约 2–3 次/秒。
- **机型覆盖**：仅 8Gen2 完成完整标定与真机验证；其他 SoC 走自适应档案，能力不足时守护自动转 observe-only，不做降级采样。
- **充电控制**：逻辑已在沙盒全场景验证，但本轮未做真机充电验证。
- 所有 `cpufreq` 写入以 `live_max == 硬件上限` 为前置条件，不满足即整体拒绝。

## 回滚

```sh
# 一键还原原始 governor/min/max（任意时刻可执行）
sh /data/adb/modules/aegis_scheduler/scripts/restore.sh

# 经控制 socket 还原并释放守护记录
/data/adb/modules/aegis_scheduler/bin/tempoctl \
  --config /data/adb/tempo/config.toml \
  --profile /data/adb/tempo/profile/babylon.toml \
  restore --socket /data/adb/tempo/run/control.sock
```

在模块管理器中禁用或删除模块即可；`uninstall.sh` 会自动还原 cpufreq、恢复充电、清理守护进程。历史模块目录建议保留（仅置 `disable`），以便随时切回。

## 从源码构建

- 守护：Rust workspace（`crates/`），`cargo build --release --target aarch64-linux-android`
- WebUI：`webui/`，构建产物同步到 `packaging/webroot/`（8 个文件）
- APP：`app/`，Kotlin + Jetpack Compose，`./gradlew :app:assembleRelease`
- 模块打包：`make package` 输出 `Aegis-v<ver>.zip`

> 工程路径含中文时请设置 `CARGO_TARGET_DIR` 到纯 ASCII 路径，否则部分链接步骤会失败。

## 许可

以 **GPL-3.0-or-later** 发布，完整文本见 [LICENSE](LICENSE)。
`Copyright (c) 2026 默默的阿文`
