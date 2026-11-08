# Aegis

**自适应 CPU 调度 + FAS 帧感知 + 充电控制**，面向骁龙 8Gen2 深度标定，并对其他 SoC 做尽力适配。

Aegis 是一个完全独立实现的 Android 调频模块（KernelSU / Magisk）。一套参数覆盖冷启动、应用内热切换、最近任务回切、突然触摸、通知弹窗等全部场景；后台常驻 100% 事件驱动，零周期轮询，息屏不阻止 Doze。

- 许可证：**GPL-3.0-or-later**
- 版权：`Copyright (c) 2026 默默的阿文`
- 模块 id：`aegis_scheduler`｜对外显示名：**Aegis**
- 版本：`v2.5`（`versionCode=11`）
- 模块描述：`自适应 CPU 调度+FAS + 充电控制`
- 首发承诺机型：骁龙 8Gen2（`kalama` / `babylon`）

> 本项目自 2026 年起由 **默默的阿文** 独立编写，不是任何现有项目的 fork，不包含任何第三方项目的源码、注释、命名、配置模板、脚本模板或构建产物。

---

## 特性

| 能力 | 说明 |
| --- | --- |
| **自适应单档调度** | 统一的自适应模型，不区分游戏/日常/省电档位，按实时负载自动升降频 |
| **静默锁最低档 / 需求窗口放开** | 静默时 mask 挡住 `scaling_min_freq`，并把 `scaling_max_freq` 临时压到同一硬件最小档，簇只能跑最低档；启动、触摸、亮屏立刻先恢复 max 再 `umount` 三簇，完全走厂商加速路径；控制器确认静默且距最近需求 ≥1 s 后才重钉 |
| **档位渐进** | 抬频三级递进（约 20 ms/级，60 ms 到达目标档），回落按滞回逐级下降（单步最多减半），不会从最低频直跳高频 |
| **启动边沿** | 检测到应用启动（静默→前台、前台负载≥0.30）时控制器进入抬频状态；mask 模式下由需求窗口把 cpufreq 完全交还厂商，控制器不写节点（`pin_min`），启动帧率由厂商 perfservice 负责 |
| **FAS 帧感知**（默认关闭） | 仅对用户在 WebUI 手动勾选的应用生效，前台切换时解析一次并缓存，事件触发而非轮询；WebUI 与 APP 均显示中文应用名 |
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
| `Aegis-v2.5.apk` | 独立控制端（包名 `com.aegis.scheduler`，WebView 壳，读写同一 socket） |

> 同时只保留一个控制端（`com.aegis.scheduler`）。若设备上存在旧包名的控制端请先卸载，避免两个进程同时写配置。

## 工作原理

1. **探针**：BPF 统计各簇运行时间与需求，按 50 ms 观察窗口累积；只有需求变化超过独立阈值才发布事件，避免事件率虚高。
2. **静默判据**：进入静默用瞬时比率（`floor_enter_ratio` 持续 `floor_settle_ms`），退出用快速滤波并要求持续窗口（`floor_exit_ratio` / `floor_exit_ms`），单点尖峰被过滤；触摸、帧超时、前台切换、亮屏会立即打断静默并抬频。
3. **启动边沿**：满足 `was_quiet && foreground_active && max(foreground_busy) ≥ 0.30 && 距上次启动 ≥ 800 ms` 时，控制器清空静默/回落状态，三簇地板一次性抬到 `launch_lift_level=1.00`，保持 `launch_lift_ms=800`，之后按 `touch_decay_ms=45` 逐级回落。
4. **地板钉住 + 封顶**：静默期 `scaling_min_freq` 由 bind mount 钉在硬件最小档（`mask_min.sh`）。注意厂商写入这个节点时内容会落进 mask 文件，所以真正把簇压住的是同一时刻把 `scaling_max_freq` 也降到硬件最小档（`min == max`）；若厂商 boost 又把 max 写高，200 ms 采样内的 `assert_cap` 会重新压回。
5. **需求窗口**：钉住会屏蔽厂商自己的启动 boost，因此钉住是可释放的——`ActivityTaskManager` 的 `START`、evdev 触摸、亮屏会立即**先恢复 max 再** `umount` 三簇 mask，让启动/交互完全走官方路径；控制器判定系统已静默且距最近一次需求（启动/触摸/亮屏）≥1 s 后，才写回硬件最小档、`mount --bind` 并重新封顶。仅在静默时钉住，因此静态更省电，而启动与滑动不吃亏。
6. **控制器不写 max**：调度控制器本身任何阶段都不写 `scaling_max_freq`；唯一的 max 写入来自 mask 的需求窗口机制，且任何启动/触摸事件都会先把它放开。

## 安装

1. **卸载旧模块**：模块管理器里删除或禁用历史调度模块（本模块会自动给旧目录打 `disable`，但推荐手动卸载以免残留两份 WebUI）。旧目录请保留而非删除，便于回滚。
2. 通过 **KernelSU / Magisk 刷入** `Aegis-v2.5.zip`。
3. 重启，等待 `sys.boot_completed=1`。开机阶段不写任何 cpufreq；开机完成并稳定后才接管。
4. 打开 **WebUI**（模块页面内嵌），或安装 `Aegis-v2.5.apk`。
5. 首次运行 APP 需在 KernelSU「超级用户」页为 **Aegis** 打开开关。未授权时界面提示「控制服务未就绪」，仅控制命令不可用。

## 使用

### WebUI

- 首页：实时频率、电量、温度、充电状态、当前前台包名
- 调度：静默阈值、抬频档位、回落速度（修改后 `config.apply`，必要时重启守护）
- FAS：总开关 + 应用勾选列表（写入 `/data/adb/tempo/fas.json`，列表为空即完全关闭）；应用名来自 APP 导出的 `/data/adb/tempo/labels.json`（中文优先），该文件缺失或超过 30 天时回退显示包名
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
| `touch_lift_level` | 0.30 | 抬频目标（簇跨度的比例，只到中档，按压不飙高档） |
| `touch_ramp_ms` / `touch_ramp_stages` | 60 / 3 | 三级抬频，约 20 ms 一级 |
| `touch_decay_ms` | 45 | 回落每一级的最小间隔 |
| `drop_factor` | 2.0 | 单次回落最多降到当前的 1/2 |
| `launch_lift_ms` / `launch_lift_level` | 1000 / 1.00 | 应用启动边沿的短窗口高地板 |
| `frame_burst_ms` / `frame_burst_level` | 150 / 0.70 | 帧超时突发的抬频窗口与目标 |
| `prime_lift_level` | 0.45 | 超大核的事件抬频上限（启动/触摸都受此约束） |
| `prime_load_cap` | 0.78 | 超大核的持续负载上限，只有重载帧压力才逼近顶档 |
| `ceiling_headroom` | 0.18 | 软上限：`scaling_max_freq = 地板 + 该比例 × 硬件跨度` |
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

## 实测数据（8Gen2 / babylon）

### v2.5（最新）

四场景均为**亮屏静态、不操作**，官方（mask 全放开）与 v2.5 同页面/同内容、各 180 s，`time_in_state` 增量 + `/proc/stat` 占用率。

| 场景 | c0 均频（3×A510） | c1 均频（4×A710/A720） | c2 均频（1×X3） | 最低档占比 c0/c1/c2 | jank | p90 |
| --- | --- | --- | --- | --- | --- | --- |
| 小说正文静止（番茄） | 875 → **307 MHz**（−65%） | 912 → **499 MHz**（−45%） | 631 → **595 MHz**（−6%） | 10.4/42.0/98.1% → **100/100/100%** | n/a（无新帧） | n/a |
| 视频静止播放（抖音，180 s） | 1230 → **307 MHz**（−75%） | 1140 → **499 MHz**（−56%） | 620 → **595 MHz**（−4%） | 0.0/0.0/98.7% → **100/100/100%** | 0 → **0**（21 486 帧） | 23 → **29 ms** |
| 输入法打字（设置 9 键，60 s） | 1152 → 1159 MHz（+0.6%） | 1051 → **1077 MHz**（+2.5%） | 742 → 757 MHz（+2.0%） | 0/59.3/94.2% → 0.5/56.6/93.6% | 42.6% → 41.7% | 21 → 25 ms |
| 长按倍速（B 站视频，60 s） | 1534 → 1541 MHz（+0.4%） | 1062 → 1041 MHz（−1.9%） | 654 → **630 MHz**（−3.6%） | 0/0/97.5% → 0/0/**98.6%** | 0.81% → 3.17% | 6 → 8 ms |

- **小说静止**：三簇 100% 时间落在硬件最低档，且 20 s 一次的即时采样 9/9 次都在最低档——低频不是"平均下来低"，而是持续最低。
- **视频静止**：最低档连续播放 180 s、21 486 帧、Janky 0；p90 29 ms 仍在 30 fps 的 33 ms 预算内。最低档即为实测下限，未再上探。
- **输入法**：c1 稳定在 1.05–1.08 GHz，未回到 2.18 GHz 的整簇抬频；抬频由厂商路径负责，模块不写。
- **长按倍速**：c2 630 MHz、98.6% 时间在最低档，未出现超大核冲顶（此前问题为 0.82 GHz，现仍不冲顶）。
- 输入法/长按两场景发生在**需求窗口内**（触摸已释放钉住），所以频率与官方基本一致——这两个场景只要求"不比官方更激进"，v2.5 满足。

**冷启动（同会话交错 A/B，每次切换后 5 s 收敛，force-stop 后 `am start -W -S`）**

| 应用 | ON 中位 | OFF 中位 | Δ |
| --- | --- | --- | --- |
| 抖音 | 861 ms | 885 ms | −2.7% |
| 番茄 | 2119 ms | 2050 ms | +3.4% |
| 淘宝 | 1893 ms | 1878 ms | +0.8% |
| 微信 | 2063 ms | 2035 ms | +1.4% |
| 设置 | 1032 ms | 1151 ms | −10.3% |

全部落在 ±10% 内；微信在 `force-stop + -S` 压力下有原生偶发卡死（官方态同样复现，属测试口径下的厂商/微信问题，非模块引入）。

### 历史：v2.1.1（机制已变，仅存档）

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

- **mask 文件不是硬保证**：厂商写 `scaling_min_freq` 时内容会落进 mask 文件，所以静默期真正把频率压住的是 `scaling_max_freq = hw_min` 的封顶；厂商在 touch boost 尾期若又写高 max，200 ms 采样内的 `assert_cap` 会压回，期间可能有极短反弹（本轮未观测到肉眼可见问题）。
- **交互期间完全放开**：封顶只在静默期成立；触摸、启动、亮屏会先恢复 max 再释放 min，因此输入法/长按等场景频率与官方一致（这是为"不卡"做的取舍，不是遗漏）。
- **视频 p90 29 ms（官方 23 ms）**：最低档连续 180 s 零丢帧，但帧时间波动略大；更高码率/60 fps 片源需要按逐档下降法重新标定地板。
- **微信 force-stop 冷启**：本机在 `-S` 冷启压力下有原生偶发卡死，官方态同样复现；不作为模块缺陷，也不宣称"微信永不白屏"。
- **机型覆盖**：仅 8Gen2 / babylon 完成四场景 + 冷启验证；其他 SoC 走自适应档案，能力不足时转 observe-only。
- **充电控制**：逻辑在沙盒验证，本轮未做真机充电验证。

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
