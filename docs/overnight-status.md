# Aegis v2.1.0 进度状态（覆盖写）

更新时间：2026-11-07 13:20（本地）
设备：测试机（babylon/8Gen2），模块 aegis_scheduler v2.1.0，单实例 Aegis，飞行模式，亮度 140 固定，常亮。
采样脚本：`/data/local/tmp/static_sample.sh`、`static_power.sh`、`burst.sh`、`test-coldstart2.sh`、`freq_trace.sh`、`aegis_cpu.sh`。

## 一、紧急故障处理（本轮最高优先级）

### 1. 现象与根因
- 现象：所有 App 无法启动，`am start` 约 15.2s 后回到桌面，`TotalTime/WaitTime` 恒为 15 秒级。
- 根因一（直接原因）：模块自己的 `scripts/shield.sh apply` 把 `perfservice`（pid 3303，高通 perf HAL）**SIGSTOP 冻结**，并 `ctl.stop` 了 `miuibooster`/`perf-hal-2-3`。所有 App 启动线程 `ActivityHelper → IPerfManager.setClientBinder` 的 binder 调用无响应，`bindApplication` 无法完成，被 AMS 以 `failed to complete startup (bg anr)` 杀掉。**这是模块自伤，不是 NoActive。**
- 根因二（设计缺陷）：v2.0.0 的 writer 把 `scaling_max_freq` 当 ceiling 封顶，实测被压到 `787200 / 844800 / 1132800`，远低于硬件上限，任何冷启动瞬时负载都撞顶。
- 处置：`kill -CONT 3303` 恢复 perf HAL；新 `shield.sh apply` 改为**空操作**（不停 `perfservice`、不停 `miuibooster`、不停 `perf-hal`、不改 thermal），`restore` 负责启动被旧版停掉的服务和进程。
- **结论：App 打不开的直接原因是 perfservice 被冻结，不是 max 封顶；但 max 封顶是必须修的真实隐患，已彻底移除。**

### 2. 设计修正（v2.1.0）
- **永不写 `scaling_max_freq`**：上限始终保持硬件最大值。仅在启动时做一次“向上修复”，若发现旧版本留下的低 ceiling，则写回 `cpuinfo_max_freq` 并回读实际生效值。
- **只用 `scaling_min_freq`**：把控制器目标作为“地板”，静态下探到厂商 min，触摸/前台切换短窗口抬地板，回落逐级下降。地板不是锁死，governor 仍可在 `[floor, hardware max]` 内自由升频。
- **硬约束**：每次写入前校验 `live_max >= 生效硬件上限`；不满足则拒绝写入并记 `ERROR` 日志（`writer.last_error`/status 可见）。
- **静态地板对账**：quiet 状态约每 1s 回读一次 `scaling_min_freq`，纠正厂商 perfservice 私自抬高的地板；若写入被厂商 QoS 拒绝（回读仍高于目标，例如 c0 固定 672000），则退避 15s，不与厂商互相拉扯。
- **原始值备份修正**：`cpufreq_backup.sh` 记录 `cpuinfo_min_freq/cpuinfo_max_freq`，不再记录可能被厂商 boost 污染的 live min/max（旧备份出现过 `policy0 min=max=2016000`）。
- 校验 `min <= max`、吸附真实档位、300ms quiet 沉降、30ms 写间隔。

### 3. 三簇生效范围（本轮实测）
| cluster | 硬件/生效 max_khz | 硬件 min_khz | 厂商 QoS 地板 | governor |
|---|---|---|---|---|
| c0 | 2016000 | 307200 | 672000 | walt |
| c1 | 2592000（cpuinfo 2803200，厂商夹紧） | 499200 | 无（可到 499200） | walt |
| c2 | 2342400（cpuinfo 3360000，厂商夹紧） | 595200 | 无（可到 595200） | walt |

> c1/c2 的 2592000/2342400 是厂商 perf/thermal 自行夹紧的结果，不是 Aegis 写入；Aegis 只在启动时向上修复一次，之后不再碰 max。c0 的 672000 地板来自厂商 PM QoS，写 307200 会立即被回读为 672000，属厂商行为。

## 二、恢复可用验证（守护 v2.1.0 运行中，force-stop 后冷启动）

| App | LaunchState | TotalTime | WaitTime | 结果 |
|---|---|---|---|---|
| 设置 com.android.settings/.Settings | COLD | 690ms | 694ms | 前台显示 |
| 笔记 com.miui.notes | COLD | 444ms | 449ms | 前台显示 |
| 微信 com.tencent.mm | WARM | 319ms | 345ms | 前台显示 |
| Chrome | COLD | 1137ms | — | 前台显示（故障恢复时） |
| 番茄小说 com.dragon.read | COLD | 1520ms | 1524ms | 前台显示 |

三个指定 App（设置/微信/浏览器）全部可正常打开。

## 三、修复后实测（v2.1.0，2026-11-07 12:00–13:10）

### 1. 静态不触碰 3 分钟（笔记前台、亮屏、飞行模式、断开充电输入测量）
采样 181.0s，`time_in_state` 增量（10ms/档）。

| cluster | 平均频率 | 最低档占比 | 最高档占比 | 其余档占比 |
|---|---|---|---|---|
| c0 | **761.3 MHz** | 87.3%（672000） | 3.55%（2016000） | 9.15% |
| c1 | **595.9 MHz** | 90.9%（499200） | 2.76%（2592000） | 6.34% |
| c2 | **657.7 MHz** | 94.8%（595200） | 2.95%（2342400） | 2.25% |

- Aegis 自身 CPU：utime 12 + stime 57 = 69 ticks / 181s = **0.38% 单核**（修复前同场景 4.68%，降至约 1/12）；物理解释为 `/proc/stack` 定位到的 thermal QMI 读放大已改为缓存 4 个 CPU/SoC/GPU/电池 zone。
- 静态功耗（`input_suspend=1` 真放电）：17 个 10s 采样，中位 **156mA**、均值 177.6mA；4.33V 约 **0.68W**（中位）。尖峰 245–274mA 多来自系统后台清理。
- 结论：静态下三簇都在最低档为主，无持续抬频；`max_freq` 全程保持 2016000/2592000/2342400，未被封顶。

### 2. 冷启动（force-stop 并确认无 pid 后启动）
| App | TotalTime / WaitTime | 总帧 | Janky 帧 | 50th / 90th / 99th |
|---|---|---|---|---|
| 设置 | **390 / 395 ms** | 59 | 2（3.39%） | 5 / 22 / 61 ms |
| 微信 | **446 / 450 ms** | 58 | 17（29.31%） | 40 / 300 / 300 ms |
| Chrome | **768 / 774 ms** | 113 | 2（1.77%） | 5 / 13 / 350 ms |

微信首帧后仍有 17 帧超过 16.7ms（集中在启动动画/消息列表渲染），是三者中最差的；设置与 Chrome 掉帧 <4%。

### 3. 热切换
| 场景 | 指标 |
|---|---|
| 微信应用内 3 次底栏切换（微信↔通讯录↔发现） | 132 帧，Janky 5（**3.79%**），50/90/99th = 5 / 29 / 109 ms |
| 后台卡片热切换（最近任务点设置卡片） | 87 帧，Janky 3（**3.45%**），50/90/99th = 8 / 14 / 65 ms |
| `am start` HOT（微信 / Chrome） | **213 ms / 269 ms** |

### 4. 通知突发抬频与回落（6s 窗口，t0 发通知）
t0 后 6.11s `time_in_state`：c0 69.7%@672000、6.4%@787200、4.4%@2016000；c1 87.9%@499200、2.5%@2592000；c2 99.2%@595200、0.7%@2342400。
- 峰值均能到达当前硬件上限（c0 2016000 / c1 2592000 / c2 2342400），但超过最低档的累计时间只有 **0.27s / 0.15s / 0.04s**，未见持续抬频，单次抬频窗口 ≤0.5s。
- 说明：该 6s 窗口前刚做完 force-stop 清理，对照组（无通知 6s）反而更忙（c0 81.6% 在 2016000），因此只能作为“突发时能瞬时到顶、随后立即回落”的定性+定量证据，不作为功耗对照。

### 5. 与官方调度同场景对照（各 180s，笔记前台、亮屏、飞行模式、断开充电输入）
| cluster | Aegis 平均频率 | 官方平均频率 | 差异 | Aegis 最高档占比 | 官方最高档占比 |
|---|---|---|---|---|---|
| c0 | 761.3 MHz | 772.4 MHz | **−1.4%** | 3.55% | 4.70% |
| c1 | 595.9 MHz | 611.8 MHz | **−2.6%** | 2.76% | 3.59% |
| c2 | 657.7 MHz | 666.5 MHz | **−1.3%** | 2.95% | 3.88% |

- 静态电流：Aegis 中位 156mA / 均值 177.6mA；官方中位 151mA / 均值 166.0mA。**差异在采样噪声内（中位 +3%，均值 +7% 且含 3 次后台清理尖峰），不能宣称静态功耗显著低于官方。**
- 三问结论：
  1. **该上频率时给足？** 是。冷启动 390–768ms、HOT 213–269ms，突发可达硬件上限，`max` 从未被压。
  2. **静态是否低于官方？** 频率层面是（平均 −1.3%~−2.6%，最高档时间 −20%~−25%）；功耗层面基本持平，受 c0 厂商 672000 地板与屏幕功耗主导，差异不显著。
  3. **有没有全核最低→2GHz 跳档？** 没有。全程 `max` 保持硬件上限，静态分布按真实档位逐级占用，未出现“全核锁最低再直接跳 2016000”的异常；c0 的 2016000 占比 3.55% 属正常调度尾部。

### 6. FAS 真机验证
| 条件 | status `fas` |
|---|---|
| 默认（fas.json enabled=false, packages=[]） | `enabled=false, active=false, apps=[]` |
| enabled=true + apps=["com.tencent.mm"]，微信前台 | **active=true**，apps 正确 |
| 同上，切到笔记前台 | **active=false**（仅勾选应用生效） |
| enabled=true + apps=[]，微信前台 | **active=false**（清空列表即失效） |

验证后已恢复默认关闭（`{"enabled": false, "packages": []}`）。

## 四、本轮不做

- 充电控制真机验证：按用户指示取消（80/90/100、100% 锁存、预约时段、温度停充均以沙盒结果为准）。

## 五、收口与交付

- `pidof tempod`：空；`pidof Aegis`：单实例（8945）。模块运行脚本中已无 `tempod` 引用（`service.sh`/`uninstall.sh` 的清理改为 `Aegis`）。
- 版本：`2.1.0`（Cargo workspace / module.properties / status 一致）。
- 产物：`D:\dev\tempo-build-android\Aegis-v2.1.0.zip`，内含新 `bin/Aegis`（2447216 字节）与更新后的 `service.sh`/`uninstall.sh`/`shield.sh`/`cpufreq_backup.sh`/README；`webroot` 8 个文件 SHA256 与 `webui/dist` 完全一致。
- 回滚路径：旧 `Aegis-v2.0.0.zip` 保留；`aegis_power_matrix` 仅 disable 保留；`/data/adb/tempo/orig` 记录 cpuinfo min/max 与原始 governor；`restore.sh`/`tempoctl restore` 可用。

## 六、已知风险

- c0 的 `scaling_min_freq` 被厂商 PM QoS 固定在 672000，Aegis 写 307200 无效；这是静态频率不能更低的主因，强行绕过需停 perfservice，已明令禁止。
- quiet 状态下厂商 perfservice 会周期性抬 c1/c2 地板，Aegis 对账后写回，实测 `writer.writes` 约 2–3 次/秒；CPU 已降至 0.38%，但写抖动仍存在，可后续把对账间隔从 1s 放宽到 2–3s 再评估。
- `foreground_pids` 长期偏大（145–469），前台包名解析依赖 `dumpsys` 兜底；不阻塞功能，需后续定位。
- 微信冷启动 Janky 29.3%、99th 300ms，仍是弱项；与厂商首帧渲染相关，需单独 profile。

## 七、v2.1.1 启动边沿、开销收敛与发布（2026-11-07 13:00–15:00）

### 1. 启动边沿

- 控制器新增 `on_app_launch(ts_us)`：满足 `was_quiet && foreground_active && max(foreground_busy) ≥ 0.30 && 距上次启动 ≥ 800 ms` 时，清空静默/回落状态，三簇地板一次性抬到 `launch_lift_level=1.00`，保持 `launch_lift_ms=800`，随后按 `touch_decay_ms=45` 回落。
- `status` 新增 `launch.lifts`；实测边沿在冷启动时触发，`writer.error=null`。
- 试验：`launch_lift_ms=200 / level=0.50` 得到微信 25.00%（90th 250ms），并不优于 800/1.00，确认 lift 不能去掉、也不能只给半档。

### 2. 开销收敛

- BPF 事件门限 20ms → 50ms；前台包名解析 500ms → 2s，且启动后 1.5s 内跳过；thermal 500ms → 1.5s。
- quiet 对账 1s → 3s；QoS 拒绝退避 15s → 30s。
- 最终静态守护 CPU 9 ticks / 181s ≈ **0.05% 单核**，voluntary ctxt 404 / 181s ≈ **2.2 次/秒**（v2.1.0 为 0.38% / ~97 次/秒级）。

### 3. 发现并修复 c2 起始上限误判（关键）

- 现象：v2.1.1 首次部署后 `writer.error = "cluster 2 live ceiling 2342400 below hardware max 2726400; write refused"`，c2 地板被厂商每次抬到 2092800 后无法写回，首轮静态 180s 中位电流 170mA。
- 根因：启动时向上修复 `scaling_max_freq=3360000` 后只回读一次，厂商 QoS 异步分步夹紧（3360000 → 2726400 → 2342400），把瞬态 `2726400` 当成生效硬件上限，导致后续 `live_max(2342400) < hw_max(2726400)` 永久拒写。
- 修复：写上限后在 300ms 沉降窗口内回读 6 次，取最低观测值作为 `hw_max`（厂商只会向下夹紧，取最低值安全）。
- 验证：重启后 `writer.error=null`，c2 地板回落 595200，`writes` 正常增长。

### 4. 最终静态 3 分钟（修复后，笔记前台、亮屏 140/4095、飞行模式、`input_suspend=1`）

采样 181.1s，`time_in_state` 增量（10ms/档）：

| cluster | 平均频率 | 最低档占比 | 最高档占比 |
|---|---|---|---|
| c0 | **455.8 MHz** | 82.25%（307200） | 4.76%（2016000） |
| c1 | **596.9 MHz** | 92.43%（499200） | 3.19%（2592000） |
| c2 | **635.9 MHz** | 97.50%（595200） | 2.22%（2342400） |

- 电流：18 个 10s 采样，首个为充电切换瞬态（−406mA），剔除后 17 个有效值中位 **79mA**、均值 **85.7mA**，4.33V 约 **0.37W**。
- `max_freq` 全程 2016000/2592000/2342400；`min_freq` 307200/499200/595200 均保持，未被厂商回写。
- 与官方基线（772.4/611.8/666.5 MHz、中位 151mA）相比：v2.1.0 A/B 口径为 −1.4%/−2.6%/−1.3%；本轮 c0 地板被 QoS 放行到 307200（v2.1.0 为 672000），c0 平均频率 455.8MHz 不可直接横比。**79mA 仅为单轮记录，不作为功耗结论，不写“功耗大幅下降”。**

### 5. 冷启动与热切换（同一 v2.1.1 配置）

| App / 场景 | TotalTime | 总帧 | Janky | 90th |
|---|---|---|---|---|
| 设置（3 次） | 398–424ms | 59 | 3.39% | 23–32ms |
| 微信（3 次） | 442–649ms | 54–58 | 25.86%–26.47% | 250–350ms |
| Chrome（3 次） | 650–777ms | 113–199 | 0.90%–1.71% | 12ms |
| 微信应用内 6 次切换 | — | 232 | 5.17% | 24ms |
| 最近任务回切设置 | — | 87 | 3.45% | 14ms |

- 微信仍高于官方同场景 22.4%–23.4%（90th 250ms），**目标 <10% / 90th<100ms 未达成**；logcat 证据为 `Skipped 31/52 frames`、`onResume took 156ms`、`MiuiPerfServiceClient ... Slow handle traversal`，启动窗口 c1/c2 已在硬件上限。
- 复测说明：后续一组冷启动因屏幕处于 Dozing、gfxinfo 仅记录 3–7 帧，Janky% 不可比，仅 TotalTime 有效（设置 407–481ms、Chrome 381–390ms、微信 446–523ms）；已在上表采用屏幕常亮、帧数完整的一组。

### 6. 发布物与版本

- 版本：Cargo workspace / `module.properties` / `status` 均为 `2.1.1`，`versionCode=5`；APP `versionName=2.1.1 / versionCode=5`。
- 模块包：`Aegis-v2.1.1.zip`（1,660,057 字节，SHA256 `99B067AD2900A01B1E92D6C791C1172E926E3DA7C888CC1F81C505DCE4AB87BE`）。
- APK：`Aegis-v2.1.1.apk`（971,016 字节，SHA256 `6FBF772BE9A1BD800B64B5B36B8BDB0A3B5649B4FED5386E5688DFC23BF5F6E6`）。
- `webroot` 8 个 Web 文件 SHA256 与 `webui/dist` 全量一致（icon.png 为模块图标，独立复制）。
- 设备收口：`pidof tempod` 为空，`/data/adb/tempo/tempod.new`、`tempod.log` 已删除；`pidof Aegis` 单实例。
- `aegis_power_matrix` 保持仅 `disable`，未删除；`restore.sh` / `shield.sh restore` / `tempoctl restore` 保留可用。

