# Aegis v2.1.1

> 自适应 CPU 调度 + FAS 帧感知 + 充电控制 · 面向骁龙 8Gen2 标定

本版在 v2.1.0 基础上加入**应用启动边沿抬频**，并进一步降低守护自身的解析与对账开销；APP 版本号与模块对齐。

---

## 变更

### 1. 应用启动边沿（新增）

- 满足 `was_quiet && foreground_active && max(foreground_busy) ≥ 0.30 && 距上次启动 ≥ 800 ms` 时触发一次启动边沿。
- 控制器清空静默/回落状态，三簇地板一次性抬到 `launch_lift_level = 1.00`，保持 `launch_lift_ms = 800`，随后按 `touch_decay_ms = 45` 逐级回落。
- 目标窗口覆盖首帧、dex/IO 与启动动画；不写 `scaling_max_freq`，仍只写 `scaling_min_freq`。
- `status` 新增 `launch.lifts` 计数，可直接确认边沿触发次数。

### 2. 降低守护开销

- BPF 事件发布门限 20 ms → 50 ms。
- 前台包名解析（`dumpsys`）门限 500 ms → 2 s，并在启动后 1.5 s 内跳过；减少启动瞬间的系统调用竞争。
- thermal 采样 500 ms → 1.5 s。
- quiet 地板对账 1 s → 3 s；厂商 QoS 拒绝后退避 15 s → 30 s。

### 3. 版本与打包一致性

- Cargo workspace、`module.properties`、`status.version` 统一为 `2.1.1`，`versionCode=5`。
- APP `versionName/versionCode` 由 `2.0.0/3` 对齐为 `2.1.1/5`，APK 文件名 `Aegis-v2.1.1.apk`。
- 清理设备上遗留的 `tempod.new` / `tempod.log`，仅保留单实例 `Aegis`。

---

## 实测数据（8Gen2 / babylon）

### 静态 3 分钟（181 s，`time_in_state` 增量）

| 簇 | 平均频率 | 最低档占比 | 最高档占比 |
| --- | --- | --- | --- |
| c0 | **455.8 MHz** | 82.25%（307200） | 4.76%（2016000） |
| c1 | **596.9 MHz** | 92.43%（499200） | 3.19%（2592000） |
| c2 | **635.9 MHz** | 97.50%（595200） | 2.22%（2342400） |

- 静态电流中位 **79 mA**、均值 **85.7 mA**（17 × 10 s 有效采样，`input_suspend=1` 真放电；首个采样点为充电切换瞬态，已剔除），4.33 V 约 **0.37 W**。
- 守护自身 CPU：9 ticks / 181 s ≈ **0.05% 单核**；voluntary ctxt 增量 404 / 181 s ≈ **2.2 次/秒**。
- `max_freq` 全程保持 2016000 / 2592000 / 2342400。
- 与官方同场景基线（772.4 / 611.8 / 666.5 MHz、中位 151 mA）相比：v2.1.0 A/B 口径平均频率低 **1.4% / 2.6% / 1.3%**；v2.1.1 本轮 c0 地板被厂商 QoS 放行到 307200（v2.1.0 时为 672000），c0 平均频率 455.8 MHz **不能与上一轮直接横比**。**中位电流 79 mA 仅为单轮记录，不作为功耗结论，不宣称“功耗大幅下降”。**

### 冷启动（force-stop 确认无 pid 后，各 3 次）

| 应用 | TotalTime | 总帧 | Janky | 90th | 99th |
| --- | --- | --- | --- | --- | --- |
| 设置 | 398–424 ms | 59 | 2（3.39%） | 23–32 ms | 69–77 ms |
| 微信 | 442–649 ms | 54–58 | 25.86%–26.47% | 250–350 ms | 300–550 ms |
| Chrome | 650–777 ms | 113–199 | 0.90%–1.71% | 12 ms | 250–350 ms |

- 微信官方同场景基线为 22.4%–23.4%（90th 250 ms）。本版 25.9%–26.5%、90th 250 ms，**目标“<10% / 90th<100ms”未达成**。
- 证据：启动窗口内 c1/c2 已处于硬件上限；把 lift 降到 200 ms / 0.50 得到 25.00%，无改善。logcat 可见 `Skipped 31/52 frames`、`onResume took 156ms`、`MiuiPerfServiceClient ... Slow handle traversal`，剩余卡顿在微信主线程/IO 与 MIUI perfservice，不是 CPU 地板不足。

### 热切换

| 场景 | 指标 |
| --- | --- |
| 微信应用内 6 次底栏切换 | 232 帧，Janky 12（**5.17%**），50/90/99 = 5 / 24 / 129 ms |
| 最近任务卡片回切设置 | 87 帧，Janky 3（**3.45%**），50/90/99 = 7 / 14 / 65 ms |

### 硬约束

- `status.clusters[].max_khz` 全程为 2016000 / 2803200 / 3360000（硬件上限），实际生效上限 2016000 / 2592000 / 2342400 为厂商夹紧值，Aegis 不写上限。
- `writer.error = null`；每次写入前校验 `live_max >= 硬件上限`。
- `shield.sh apply` 为空操作，`perfservice` / `perf-hal` / `miuibooster` 全部保持运行；`shield.sh restore` 保留可用。

---

## 安装与升级

1. 卸载旧模块目录或保持其 `disable`（保留目录便于回滚）。本版会自动给历史目录打 `disable`。
2. KernelSU / Magisk 刷入 `Aegis-v2.1.1.zip`，重启。
3. 开机完成并稳定后自动接管（`sys.boot_completed=1` 前不写任何节点）。
4. 打开 WebUI 或安装 `Aegis-v2.1.1.apk`（包名 `com.aegis.scheduler`，需在 KernelSU 授予超级用户）。

## 回滚

```sh
sh /data/adb/modules/aegis_scheduler/scripts/restore.sh
```

或在模块管理器禁用 / 删除模块（`uninstall.sh` 自动还原 cpufreq、恢复充电、清理守护）。

---

## 已知限制

- c0 的 `scaling_min_freq` 受厂商 PM QoS 固定为 **672000**，写 307200 无效；绕过需停用 perfservice，已明确禁止。
- 微信冷启动 **25.9%–26.5%**（90th 250 ms），为当前最弱项，与厂商首帧渲染路径相关。
- 静默期厂商 perfservice 会周期性抬 c1/c2 地板，对账后写回，实测约 2–3 次/秒。
- **充电控制本轮未做真机验证**（80/90/100 上限、100% 锁存、预约时段、温度停充均以沙盒结果为准）。
- 仅 8Gen2 完成完整标定；其他 SoC 走自适应档案，能力不足时守护自动转 observe-only。

## 产物与校验

| 文件 | 说明 |
| --- | --- |
| `Aegis-v2.1.1.zip` | KernelSU / Magisk 模块包（含 `bin/Aegis`、`tempoctl`、`webroot/`、脚本） |
| `Aegis-v2.1.1.apk` | 控制端（`com.aegis.scheduler`，v2.1.1 / 5） |

SHA256：

```text
99B067AD2900A01B1E92D6C791C1172E926E3DA7C888CC1F81C505DCE4AB87BE  Aegis-v2.1.1.zip
6FBF772BE9A1BD800B64B5B36B8BDB0A3B5649B4FED5386E5688DFC23BF5F6E6  Aegis-v2.1.1.apk
```

- `pidof tempod`：空；`pidof Aegis`：单实例。
- 版本：Cargo workspace / `module.properties` / `status` 均为 `2.1.1`，`versionCode=5`。
- `webroot` 8 个文件与 `webui/dist` SHA256 完全一致。

---

**完整过程数据：** 见 `docs/overnight-status.md`。
**版权：** `Copyright (c) 2026 默默的阿文` ｜ **许可：** GPL-3.0-or-later
