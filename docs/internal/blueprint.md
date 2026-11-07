# Tempo · 阶段 0 方案

**单 FAS + 单自适应调度 + 充电控制 + 温控（clean-room 重写）**

- 文档版本：v0.1（方案稿，未进入实现）
- 日期：2026-11-06
- 目标设备（已用 adb 确认）：Xiaomi MIX Fold 3 / `babylon` / `2308CPXD0C` / Android 16 / `kalama`（SM8550-AC）/ arm64-v8a
- 已安装上游模块：`aegis_power_matrix`（当前 `global_mode=balance`）
- 本阶段只产出方案，不写实现代码、不改上游源码、不刷机。

---

## 0. 结论摘要

1. **可以做到独立署名，但前提是“隔离式重写”，不是改名/改结构。** 上游源码、脚本、配置模板、资源、二进制、构建链必须整段丢弃；新工程只保留“功能需求”和“可观察行为”，并使用自有命名、自有配置格式、自有算法表达。
2. **单 FAS + 单自适应是可行的。** 用一套连续控制器覆盖待机/阅读/视频/滑动/游戏，不靠模式切换；FAS 不再是“按应用白名单开关”，而是对任何正在产出帧的前台应用生效。
3. **“静态不虚高”与“亮屏也能落到最低频”合并为一条 Quiet Path（静默路径）**：无触摸、无新帧、前台树无有效负载达到稳定窗口后，直接把每个簇压到该簇**真实最低可用频率**；任何触摸/新帧/前台切换立即一次性抬起，用滞回和最小驻留避免来回抖动。
4. **跨 SoC 自适应分三层**：结构可自动推导（簇数、簇归属、频率表、capacity、角色、封顶向量）；策略参数需实机标定（帧余量、静默阈值、热/充电节点差异）；未验证平台只能“安全可用”，不能承诺同等功耗/手感。方案明确写出边界，不做“全自动适配所有机器”的含糊承诺。
5. **功耗目标改为“严格低于上游 4.6.1”**，不是持平：静态阅读为一级场景，目标是能量显著低于上游；日常/视频不劣化；游戏以“不掉帧、功耗不高于上游”为底线。验收用同机 A/B、外部只读统计，不把测量开销算进模块。
6. **100% 事件驱动、零固定周期轮询**：控制决策只由 eBPF 累计事件、输入事件、帧事件、屏幕/充电/热 uevent、前台变化触发；仅在“抬频窗口/静默衰减”存在时挂一次性 deadline，窗口结束即撤销，空闲时 `epoll_wait(-1)` 无唤醒。

---

## 1. 范围与硬约束

### 1.1 只保留四项能力

| 能力 | 边界 |
|---|---|
| 单一 FAS 调度 | 一套规则；不区分游戏/日常/待机模式；不维护应用模式白名单；只对“正在产帧的前台应用”生效 |
| 单一自适应模式 | 一套参数覆盖全部场景；场景差异由连续信号（负载、帧、触摸、屏幕、热、充电）体现，不靠离散模式切换 |
| 充电控制 | 充满即停、按次插电锁存、阈值可配、温控降流/停充；C 实现 |
| 温控 | 设备热 profile 投放 + 运行时热余量约束；不接管厂商全部热管理，只做调度侧封顶与充电侧保护 |

明确不做：冻结应用、IO 调度改写、CPU idle governor 改写、杀 logd、伪装温度节点、应用级规则、在线更新、多模式切换、上游的任何“补丁式”特判。

### 1.2 硬约束

- **事件驱动**：禁止固定周期读取 sysfs/proc、禁止固定 tick 控制循环、禁止固定间隔 shell 轮询；允许“事件触发的一次性 timerfd”。
- **Doze 友好**：息屏后除充电/热/屏幕事件外不唤醒；无事件时进程阻塞。
- **功耗**：静态阅读场景必须**低于**上游 4.6.1；其他场景不高于上游；帧率/卡顿不劣化。
- **静态不虚高**：负载不变时不得抬频、不得停在高频不下来；亮屏低负载可落到每簇最低频。
- **跨 SoC**：优先 babylon；其他簇数/频率结构尽量自动推导；不能自动推导的部分必须明确标注并走标定。
- **工程隔离**：新工程 `tempo-sched/`，不改 `Aegis-Scheduling-4.6.1/` 与 `Aegis-Scheduling-main-7b406f2/`。
- **构建路径**：所有构建产物指向纯 ASCII 路径（如 `D:\dev\tempo-build`），工作目录在 `OneDrive\文档` 下，禁止把 `target/` 留在中文路径。

---

## 2. 上游依赖与清理清单

### 2.1 必须整段丢弃（不复制、不改写、不打包）

| 上游路径/产物 | 处理 | 原因 |
|---|---|---|
| `src/**`（27 个 Rust 文件，含 `scheduler/`、`scheduler/fas/`、`monitor/`） | 全删，不参考实现 | 表达层核心；只允许提炼“行为需求”，不允许带着代码结构重写 |
| `yumi-ebpf/**`（`cpu.rs`、`fps-*.rs`、`frame.rs`） | 全删 | 探针代码、map 结构、事件字段均属上游表达 |
| `xtask/**` | 全删 | 构建/打包链整体替换 |
| `webui/**`（Vue 源码、`shield.png`、i18n、bridge、mock、views） | 全删 | UI 资源、交互与文案全部重做 |
| `module/**`（`module.prop`、`service.sh`、`customize.sh`、`uninstall.sh`、`scripts/`、`tools/aegis_adapt.sh`、`config/*`、`rules.yaml`、`charge_guard.conf`、`charge_guard*`、`core/bin/Aegis`、`thermal_manager`、`update_prop`、`busybox`、`webroot`、`app/*.apk`） | 全删 | 脚本模板、配置字段/注释、二进制、APK、WebUI 产物、第三方 busybox 均不进入新工程 |
| `tools/**`（`Aegis_一键适配.sh`、`aegis_cpu_probe.sh`） | 全删 | 适配脚本模板与探测输出格式重写 |
| `prebuilt/**`（历史 zip、APK） | 全删 | 历史构建产物 |
| `ci-artifacts/**` | 全删 | 旧二进制 |
| `updateInformation/**` | 全删 | 更新元数据/变更日志模板 |
| `docs/**`（README、调参说明、上游 README 备份、交付报告） | 全删 | 文档与参数表重写；只允许用“实测事实”，不允许复制文本 |
| `.github/**`、`.cargo/**`、`Cargo.toml`、`Cargo.lock`、`build.rs` | 全删/重写 | 构建链与依赖锁定重新建立 |
| 上游所有配置键名、日志文案、i18n `.ftl`、版本号字符串、模块 id、包名、图标 | 禁止复用 | 属于表达与品牌标识 |

**特别提示**：上游 `module/config/config.yaml` 与 `rules.yaml` 只作为“当前设备行为对照”，已拉到 `_reference/device-20261106/`（不在新工程内）。它们**不得**作为新工程的配置模板、字段来源或默认值文件。

### 2.2 功能等价、实现自写

| 功能 | 上游承载 | 新实现（自有结构） | 关键差异 |
|---|---|---|---|
| 线程/簇运行时间统计 | eBPF `sched_switch` + 线程 hash | `probe/runtime`：每 CPU 累加器 + 需求边沿事件 | 不做“每次切换都上报”；按累计运行量/边沿触发，减少唤醒 |
| 帧时间采集 | uprobe + perf/ringbuf | `probe/frame`：自有 uprobe 目标与事件结构 | 只采前台产帧进程；帧事件可合并，不逐帧唤醒控制环 |
| 前台识别 | 周期读 cpuset/top-app | `sources/topapp`：进程创建/退出/切换事件驱动刷新 | 不固定 100ms 读；读到的成员集合缓存 |
| 屏幕状态 | netlink `power`/`backlight` | `sources/screen`：自有 uevent 解析与状态机 | 保留事件源思路，重新实现 |
| 触摸抬频 | evdev 只读 + 300ms 去抖 | `sources/touch`：epoll 阻塞读，无超时轮询 | 按下→一次性 lift；不驻留 |
| 负载采样 | 100ms/500ms 采样线程 | `control/demand`：按事件累计量产出需求快照 | 无固定 tick；空闲可不产生样本 |
| 负载调频 | CLG 状态机/公式 | `control/adaptive`：自有连续控制器 + 静默路径 | 不使用上游增益曲线/字段/驻留语义 |
| FAS | 应用白名单 + 档位 + PID/加载特判 | `control/frame`：帧截止误差 + 一次性 burst 预算 | 单规则集；无 per-app profile；无模式 |
| 配置加载/热更新 | 3s 轮询 / inotify | `config`：inotify 事件 + 原子替换 + schema 校验 | 无周期轮询；配置损坏回退上一版 |
| 频率执行 | cpufreq sysfs 写入 | `actuate/cpufreq`：每簇 ceiling/floor 成对写入 + 回滚 | 统一“上限控制”，静默时上限=最低频 |
| 充电控制 | C 守护 + shell 轮询 | `power/charge`：C + uevent 事件 | 无 shell 循环；锁存状态落盘 |
| 温控 | `thermal_manager` 二进制（无源码） | `power/thermal`：C + 热区事件/配置投放 | 不复制二进制；热区差异做能力探测 |
| 模块安装/卸载/适配 | shell 模板 | `packaging/`：自有脚本 + 设备 profile 生成器 | 自有文件名/字段/输出格式 |
| WebUI | Vue 产物 | `ui/web`：Svelte + Vite + TS 重做 | 新信息架构、新图标、新文案 |
| APP | 仅 APK，无源码 | `ui/android`：Kotlin + Compose + libsu 重做 | 新包名、新签名、新界面 |
| 构建/打包 | xtask + GitHub Actions | `Makefile`/`just` + 脚本 + 可选 CI | 产物名、目录、版本规则全部自有 |
| 日志/i18n | log4rs + fluent | `tracing`/自有日志 + 自有文案表 | 不沿用文案与键名 |

### 2.3 可以保留的“非表达性事实”

- Linux/Android 公共接口：`/sys/devices/system/cpu/cpufreq/policy*`、`/dev/cpuset/top-app`、`/dev/input/event*`、netlink `KOBJ_UEVENT`、`/sys/class/power_supply/*`、`/sys/class/thermal/*`。
- 标准算法与协议：PID/前馈/滞回、epoll、ringbuf、inotify、udev uevent、TOML/JSON。
- 硬件事实：本机簇拓扑、频率表、capacity、最大/最小频率（需在新工程中重新探测并落盘）。
- 第三方库：`aya`/`libbpf-rs`、`mio`、`serde`、`clap`、`tracing`、NDK 工具链等（按其自身许可证使用，不复制上游封装代码）。
- GPL-3.0 许可证正文本身（从官方文本引入，不复制上游 `LICENSE` 文件的排版/附加说明）。

### 2.4 构建链清理

- 不沿用 `xtask`、`cargo xtask`、上游 CI workflow、`module.prop` 生成逻辑、zip 目录约定。
- 新构建：`make build-daemon`、`make build-power`、`make build-web`、`make build-apk`、`make pack`；所有 `CARGO_TARGET_DIR`、`OUT_DIR`、NDK 临时目录指向 ASCII 路径。
- 新产物命名：`tempo-<version>-<gitshort>-<timestamp>.zip`（版本唯一真值来自新工程的 `packaging/module.properties`，字段名自有）。
- 不打包 busybox；如需 shell 工具，优先系统 `toybox`，并在依赖清单中说明。

---

## 3. 目标架构

### 3.1 目录树

```
tempo-sched/
├─ Cargo.toml                  # Rust workspace（仅自有 crate）
├─ crates/
│  ├─ tempod/                  # 调度守护（Rust）
│  │  ├─ src/main.rs
│  │  ├─ src/event_loop.rs     # mio/epoll，无固定 tick
│  │  ├─ src/sources/
│  │  │  ├─ probe.rs           # eBPF reader / 事件解码
│  │  │  ├─ touch.rs
│  │  │  ├─ screen.rs
│  │  │  ├─ topapp.rs
│  │  │  └─ power.rs           # 充电/热 uevent 订阅
│  │  ├─ src/control/
│  │  │  ├─ demand.rs          # 需求快照与静默判定
│  │  │  ├─ adaptive.rs        # 单模式连续控制器
│  │  │  └─ frame.rs           # 单规则 FAS
│  │  ├─ src/actuate/cpufreq.rs
│  │  ├─ src/profile.rs        # device.toml 读取/校验
│  │  ├─ src/config.rs         # TOML + inotify
│  │  ├─ src/ipc.rs            # Unix socket
│  │  └─ src/state.rs
│  └─ tempoctl/                # CLI：status/config/logs/calibrate
├─ probe/                      # eBPF 源（C 或 aya-ebpf，自有 map/字段）
│  ├─ runtime.bpf.c
│  ├─ frame.bpf.c
│  └─ build.rs / Makefile
├─ power/                      # C 守护：充电 + 温控
│  ├─ main.c
│  ├─ uevent.c
│  ├─ charger.c
│  ├─ thermal.c
│  └─ policy.c
├─ ui/
│  ├─ web/                     # Svelte + Vite + TS
│  └─ android/                 # Kotlin + Compose + libsu
├─ packaging/
│  ├─ module/                  # 自有 service/uninstall/customize 脚本
│  ├─ profile-gen.sh           # 设备结构探测 → device.toml
│  ├─ build-module.sh
│  └─ module.properties
├─ tests/
│  ├─ bench/                   # 真机 A/B、静态功耗、帧统计
│  └─ fixtures/
├─ docs/
│  ├─ blueprint.md             # 本文
│  ├─ behavior-spec.md         # 黑盒行为规格（实现前冻结）
│  └─ test-plan.md
├─ PROVENANCE.md               # 来源与 clean-room 记录
├─ README.md
└─ LICENSE
```

### 3.2 进程与线程划分

| 进程 | 语言 | 职责 | 常驻/唤醒 |
|---|---|---|---|
| `tempod` | Rust | 调度：事件源、需求计算、单模式控制、FAS、频率写入、CLI/UDS | 常驻；无事件时阻塞 |
| `tempowerd` | C | 充电阈值/锁存、温控 profile 投放、热/充电 uevent | 常驻；epoll 阻塞 |
| `tempoctl` | Rust | 状态查询、配置校验/应用、日志、标定入口 | 按需进程，不常驻 |
| WebUI | Svelte 静态页 | 通过 KernelSU `ksu.exec` 调 `tempoctl` | 仅在管理器内打开 |
| APP | Kotlin/Compose | 通过 `libsu` 调 `tempoctl`，展示同一数据模型 | 按需 |

`tempod` 内部线程建议：
- **主线程**：`mio` Poll + 控制决策 + 频率写入（避免跨线程锁）。
- **探针线程**：ringbuf 读取、解码、批量投递；只做解析，不做 sysfs 写。
- **配置线程**：inotify 监听配置变更，校验后通过通道投递。
- 不设“采样线程”，不设固定 tick 线程。

`tempowerd` 单线程 epoll：netlink uevent（`power_supply`、`thermal`）、一次性 deadline（充电锁存/热保护复核）；无 shell 循环。

### 3.3 事件模型与“零固定周期轮询”

| 事件源 | 触发方式 | 产生的事件 |
|---|---|---|
| eBPF `sched_switch` 累加器 | 全设备累计运行时间；达到 `runtime_quantum_us` 或出现空闲边沿 | `DemandEdge`（含簇、累计忙碌、空闲、时间戳） |
| eBPF frame uprobe | 前台进程提交帧 | `FrameTick`（可合并 stride） |
| evdev | `poll/epoll` 阻塞读，无 timeout | `TouchDown`、`TouchUp` |
| 屏幕 | netlink `power`/`backlight` uevent | `ScreenOn/Off` |
| 前台 | 进程 exec/exit/切换事件驱动刷新 `top-app` | `ForegroundChanged` |
| 充电 | `power_supply` uevent | `ChargerChanged`、`BatteryChanged` |
| 热 | `thermal_zone` uevent（能力探测） | `ThermalTrip` |
| 配置 | inotify | `ConfigChanged` |

**一次性 deadline 的边界**：仅当存在“触摸抬频窗口、静默衰减窗口、帧 burst 预算、充电锁存复核”时，使用 `timerfd` 挂一个到期时间；到期即执行并撤销。没有窗口时 `epoll_wait(-1)` 无限阻塞。这样既满足“零固定周期轮询”，也允许有限时间的控制动作。

**eBPF 开销控制**：
- BPF 侧只做整数累加与阈值比较，不做字符串、不做逐切换 ringbuf 输出。
- 每个事件只携带必要字段；控制线程批量处理。
- 空闲时 BPF 不主动唤醒用户态（除非窗口 deadline 已挂）。
- 帧事件按 `frame_sample_stride` 合并，避免 120/144Hz 下每帧唤醒。

### 3.4 模块边界与接口

- **事件总线**：内部 `crossbeam`/`mpsc` 通道；事件类型固定、可序列化，便于单测。
- **控制器接口**：
  - 输入：`DemandSnapshot { cluster_id, busy_ns, idle_ns, ts, fg_active, frame_state, thermal_cap }`
  - 输出：`Decision { cluster_id, ceiling_khz, reason }`
  - 纯函数化：给定状态+配置+快照，输出决策；不直接读 sysfs，便于离线回放测试。
- **执行器接口**：`apply(ceiling_khz)` 只写 cpufreq 上限；`floor()` 写“上限=该簇最低频”；失败回滚到上一安全值并上报。
- **CLI/UDS**：`/data/adb/tempo/run/control.sock`，JSON Lines；命令：`status`、`profile.get`、`config.get`、`config.validate`、`config.apply`、`log.tail`、`calibrate`。目录 `0700`，仅 root。
- **配置与状态**：`/data/adb/tempo/config.toml`、`/data/adb/tempo/device.toml`（生成、只读）、`/data/adb/tempo/state/`、`/data/adb/tempo/log/`。

### 3.5 配置格式（自有字段，TOML）

```toml
schema = 1

[daemon]
log_level = "warn"              # 默认低日志，减少 I/O
profile_path = "/data/adb/tempo/device.toml"

[events]
runtime_quantum_us = 16000      # 累计运行量触发，不是定时器
frame_stride = 2                # 每 2 帧合并一次事件
quiet_decay_ms = 120            # 从“有负载”到允许静默的一次性窗口

[control]
quiet_ratio = 0.50              # 低负载目标 = 实测需求 × 该系数（<1，防虚高）
quiet_cap = 0.28                # 前台树整体空闲时的额外上限
role_cap = [1.00, 1.00, 0.85]   # 按角色，不按 CPU 编号
floor_enter_ratio = 0.03        # 低于该需求进入静默路径
floor_settle_ms = 120           # 静默确认窗口
floor_residence_ms = 200        # 最低频最小驻留，防来回抖
drop_ratio = 0.08               # 接近空载才允许快速下探
drop_factor = 4.0
touch_lift_ms = 150             # 触摸一次性抬频窗口
touch_lift_level = 0.85
frame_slack_ms = 3.0            # 帧截止余量
reserve_ratio = 1.05            # 重载留余量
step_hysteresis = 0.02          # 频率档位滞回，防抖动

[charge]
stop_at_percent = 80
resume_at_percent = 70
temp_stop_c = 45
temp_resume_c = 40
latch_per_plug = true

[thermal]
guard_c = 45
profile = "auto"

[ui]
language = "zh"
```

`device.toml` 由安装时探测生成，示例：

```toml
schema = 1
platform = "kalama"
sdk = 36
[[cluster]]
cpus = [0, 1, 2]
role = "efficiency"
min_khz = 307200
max_khz = 2016000
capacity = 266
freq_khz = [307200, 441600, ...]
[[cluster]]
cpus = [3, 4, 5, 6]
role = "balanced"
min_khz = 499200
max_khz = 2803200
capacity = 811
[[cluster]]
cpus = [7]
role = "prime"
min_khz = 595200
max_khz = 3360000
capacity = 1024
```

### 3.6 语言选型

- **调度守护：Rust**。理由：事件驱动循环与 eBPF 加载生态成熟（`aya`/`libbpf-rs`），内存安全，便于把控制器写成纯函数做离线回放；常驻内存和启动开销可控。
- **充电/温控：C**。理由：只做 sysfs/uevent 与简单状态机，无运行时依赖，静态编译体积小，故障隔离；符合“充电控制优先 C”。
- **WebUI：Svelte + Vite + TS**。理由：产物小、无虚拟 DOM 运行时，适合 KernelSU WebView；不沿用上游 Vue 结构和组件。
- **APP：Kotlin + Jetpack Compose + libsu**。理由：原生、可签名分发、与 WebUI 共享同一 CLI/JSON 数据模型；不包 WebView 套壳。
- **备选**：若希望减少维护面，APP 可用 Compose + 直接 UDS（root）而非 libsu shell；两者都可行，列入决策。

---

## 4. 单模式自适应设计

### 4.1 输入信号（全部事件驱动）

| 信号 | 来源 | 用途 |
|---|---|---|
| 每簇需求 `d_i` | eBPF 累计 busy/(busy+idle) | 主控制量 |
| 前台活跃度 | top-app 成员 + 前台最忙线程 | 区分“整机忙”与“前台真忙” |
| 帧状态 | 前台帧事件间隔、截止误差 | FAS 余量、静默解除 |
| 触摸 | evdev 按下/抬起 | 一次性抬频、静默解除 |
| 屏幕 | uevent | 息屏静默/亮屏恢复 |
| 热余量 | thermal uevent/配置 | 动态封顶 |
| 充电/电量 | power_supply uevent | 充电热保护、低电量策略 |
| 前台切换 | exec/exit/top-app 事件 | 重置控制器、一次性冷启动预算 |

### 4.2 连续控制器（无模式）

对每个簇 `i`：

1. 由事件窗口得到 `d_i`（0~1）。
2. 基础目标 `base_i = f_i(d_i)`：低负载段用 `quiet_ratio` 压缩，高负载段用 `reserve_ratio` 留余量；中间连续插值，无阶跃。
3. 叠加一次性修正：触摸 `lift`、帧截止 `frame_reserve`、前台切换 `warm_start`。
4. 施加角色上限 `role_cap[role_i]`、静默上限 `quiet_cap`、热/充电上限。
5. 与上一档比较：`step_hysteresis` 内不换档；跨档才写 sysfs。
6. 输出 `ceiling_khz`；静默路径直接输出 `min_khz`。

**没有 hold 状态、没有模式枚举、没有 per-app 规则。** 控制器的全部状态是：上一档、静默窗口开始时间、一次性 lift/burst 到期时间、当前热/充电上限。

### 4.3 Quiet Path：亮屏低负载也落到真实最低频

这是本方案的关键路径，独立于息屏逻辑：

- **进入条件**（全部满足）：
  - 亮屏；
  - 无触摸、无新的前台帧事件（显示 vsync 不算）；
  - 前台树最忙线程需求低于 `floor_enter_ratio`；
  - 持续 `floor_settle_ms`；
  - 无待处理的帧截止/热保护动作。
- **动作**：每个簇把频率上限写成该簇 `min_khz`（不是“归一化后的低档”），并保持 `floor_residence_ms`。
- **退出条件**（任一）：
  - 触摸按下；
  - 前台新帧事件且帧截止误差超阈值；
  - 前台切换/新进程；
  - 需求越过 `floor_enter_ratio + step_hysteresis`；
  - 热/充电策略要求抬升。
- **退出动作**：一次性抬到“基础目标 + 帧/触摸余量”，不做逐档爬升；随后按静默窗口衰减。
- **防抖**：`floor_residence_ms` 只限制“最低频→更高”的切换频率；最低频期间不产生任何抬频。频率档位比较用 `step_hysteresis`（相对频率间隔），避免在相邻档之间来回写。
- **防卡顿回弹**：
  - 触摸 lift 覆盖首帧（默认 150ms，按设备标定）；
  - 帧截止控制器在退出的同一决策内给出余量，不等下一拍；
  - 前台切换给一次性 warm start（有上限、会衰减）；
  - 如果 200ms 内出现两次以上“最低频→抬升”，自动把静默确认窗口加倍一次（自愈，不永久改参）。

这与“静态不虚高”是同一机制的两面：**没有负载就不抬频；抬频必须有事件理由；负载不变就不换档。**

### 4.4 设备参数自动生成（结构层）

安装/首次启动时 `packaging/profile-gen.sh` 只读探测：

1. 枚举 `/sys/devices/system/cpu/cpufreq/policy*`；用 `related_cpus` 得到簇归属。
2. 读 `cpuinfo_min_freq`、`cpuinfo_max_freq`、`scaling_available_frequencies`（缺失时回退 `time_in_state` 的频率键）。
3. 读 `cpu_capacity`；缺失时用 `max_khz × 核数` 作为相对权重，并标记 `capacity_source=estimated`。
4. 按 capacity 排序分配角色：
   - 最低簇 = `efficiency`
   - 最高簇且单核/显著高 capacity = `prime`
   - 其余 = `balanced`（可有多个）
   - 2 簇设备：`efficiency` + `balanced`（最高簇不强行标 prime）
5. 生成 `device.toml`：簇数 N、每簇 CPU 列表、频率表、角色、`min_khz`、`max_khz`、权重（自有公式 `cap_i / cap_min`）。
6. 启动时校验 live 拓扑与 `device.toml`；不一致则重新生成并记录，不直接套旧表。

### 4.5 跨 SoC 自适应：能力边界（明确）

| 能力 | 自动推导 | 需实机标定 | 说明 |
|---|---|---|---|
| 簇数量、簇归属 | ✅ | — | `policy*` + `related_cpus`；异常拓扑需人工确认 |
| 每簇频率表、最低/最高频 | ✅ | — | `scaling_available_frequencies`/`time_in_state` 回退 |
| capacity/角色/权重 | ✅（多数） | 部分 | 无 `cpu_capacity` 时用估算并标记，效果不承诺 |
| 每簇上限向量长度 | ✅ | — | 按簇数生成，不写死 3 簇 |
| 静默阈值、帧余量、touch lift | — | ✅ | 与屏幕刷新率、应用渲染、调度延迟相关 |
| 热区/充电节点名称与语义 | 部分 | ✅ | 厂商节点差异大，需能力探测 + 标定 |
| eBPF tracepoint/uprobe 可用性 | 部分 | ✅ | 内核版本/符号差异；不可用需降级或明确不支持 |
| 功耗不高于上游 | — | ✅ | 只能在同机同 ROM 上验证，不能跨机型承诺 |
| 游戏帧稳定性 | — | ✅ | 帧节奏与 GPU/显示相关，需逐机验证 |

**支持分级**：
- **Tier A（承诺）**：`babylon` / 8Gen2 / Android 16 / KernelSU，完成全部验收。
- **Tier B（结构支持，尽力）**：2/3/4 簇、常见 8Gen3/天玑 9200+ 类设备；自动生成 profile，跑基础冒烟；功耗/手感不承诺。
- **Tier C（不承诺）**：非常规拓扑、无 cpufreq、无 capacity、eBPF 不可用、内核符号受限；只保证“不写入、不崩溃、明确提示”。

**必须明说的做不到**：
- 不能仅凭静态探测就知道某簇在某个频率下的真实能效；需要同机标定。
- 不能保证所有 SoC 都达到“低于上游 4.6.1”；上游本身是针对 8Gen2 调过的。
- 不能在没有该机型的情况下验证触摸延迟、帧截止、热行为。
- 不能自动适配内核级差异（例如某些内核禁止写 `scaling_max_freq`、或 governor 不响应）。

### 4.6 场景行为（同一套参数）

| 场景 | 行为 |
|---|---|
| 息屏待机 | 全簇上限压到最低；除充电/热/屏幕事件外不唤醒；无周期任务 |
| 亮屏静态阅读 | 无触摸/无新帧→Quiet Path，全簇真实最低频；页面翻动触摸→150ms lift→按需求回落 |
| 视频播放 | 帧事件稳定；FAS 只补足帧截止余量，不做静态抬频；解码负载低时仍可下探 |
| 信息流滑动 | 触摸 lift + 需求上升；松手后静默窗口→最低频；无 hold |
| 游戏 | 单 FAS 规则：帧截止误差大→一次性 burst；稳定后衰减；无应用白名单、无档位模式 |
| 充电 | C 守护独立控制停充/恢复/锁存；调度侧按热余量封顶 |
| 热保护 | 热 uevent 触发上限下调；冷却后按事件恢复，不周期轮询 |

### 4.7 与当前设备基准配置的对照（仅作行为参考）

已从真机拉取当前生效配置（`_reference/device-20261106/up-config.yaml`、`up-rules.yaml`），当前 `global_mode=balance`。其中体现“静态不虚高”的机制：

| 上游机制（仅对照） | 行为含义 | 新工程的自有表达 |
|---|---|---|
| 低负载目标系数 < 1 | 目标低于实测需求，静态不追高 | `quiet_ratio`（默认 0.50，按机标定） |
| 前台树整体空闲额外上限 | 整机有后台负载但前台无有效工作时仍压频 | `quiet_cap`（默认 0.28） |
| 按簇封顶，超大核更低 | 超大核不参与无效高频 | `role_cap`（按角色，如 prime 0.85） |
| 接近空载才快速下探 | 避免交互中误判空载导致锯齿 | `drop_ratio` + `drop_factor`（0.08 / 4.0） |
| 触摸按下短窗口抬频 | 只补首帧，不驻留 | `touch_lift_ms` + `touch_lift_level`（150ms / 0.85） |
| 无 hold/无驻留 | 负载一掉就回落 | Quiet Path + 一次性窗口，无持续驻留 |

**注意**：这些数值只是“当前设备可观察行为”的参考，不作为新工程默认配置模板；新工程默认值由 `profile-gen` + 标定流程产生，且字段名、结构、注释全部不同。

### 4.8 量化判定口径（静态/低负载）

以下为**验收口径**，具体阈值在 Tier A 设备上先测上游基线再冻结：

| 指标 | 口径 | 目标 |
|---|---|---|
| 最低频可达 | 亮屏静态 10min，每簇 `time_in_state` 中最低频占比 | 三簇均 ≥ 95%（不含首 60s 稳定期） |
| 静态频率分布 | 每簇 p95/p99 频率 | p95 ≤ 最低频 + 1 档；p99 ≤ 最低频 + 1 档（无触摸/无新帧窗口） |
| 无负载抬频 | 连续 10min 无触摸/无新帧，统计高于最低频的连续区间 | 0 个 >100ms 的区间；抬频次数 ≤ 1 次/10min（且必须可解释） |
| 掉频响应 | 最后一次触摸/帧事件 → 回到最低频的时间 | ≤ 150ms（P95），无超过 300ms 的尾部 |
| 抖动 | 1s 内频率档位切换次数 | 静态 ≤ 1 次；滑动 ≤ 4 次；无相邻档来回 >2 次 |
| 亮屏低负载能量 | 同机 A/B，30–60min，固定亮度/同一页面/同一电量区间 | 新模块 ≤ 0.90× 上游（目标 -10%）；至少 95% CI 不重叠且均值更低 |
| 日常/视频 | 同上 | ≤ 1.00× 上游（不劣化） |
| 游戏 | 帧 jank + 能量 | jank 不高于上游；能量 ≤ 1.00× 上游 |
| 守护开销 | `tempod` CPU 时间 / 单核；静态唤醒次数 | CPU < 0.2%；唤醒 < 0.2 次/s；无固定 timer |
| eBPF 开销 | 系统级 sched_switch 处理增量 | < 0.5% 单核（用 A/B 或 perf 估算） |

**证明“比上游功耗低”的方法**：

1. 同机、同 ROM、同亮度、同应用/页面、同电量区间（40–70%）、不充电、同一网络状态。
2. A/B 交替：上游 30min → 新模块 30min → 上游 30min → 新模块 30min，至少 3 组；每组前 2min 丢弃，取稳定窗口。
3. 能量口径：
   - `/sys/class/power_supply/battery/charge_counter` 前后差值 × 电压（或 `dumpsys batterystats --charged` 的 mAh 差）；
   - 有条件的用外部 USB 功率计交叉验证。
4. 频率口径：测试脚本在开始/结束各读一次 `time_in_state`（**不由守护周期读取**），算各簇各档驻留占比、p95。
5. 唤醒/开销口径：用 ftrace 统计 `tempod` 的 `sched_wakeup` 次数；用 `/proc/<pid>/stat` 的 utime+stime；用 `dumpsys power` 看 wakelock。
6. 帧口径：`dumpsys gfxinfo <pkg> reset` + 滑动/翻页脚本 + `dumpsys gfxinfo` 读 janky 比例；必要时 Perfetto 帧时间线。
7. 数据纪律：只采稳定窗口；异常值人工复核后剔除并记录原因；不自动挑选“好看”的区间；用户确认后才开始正式采集。

**如何避免采样本身带来额外功耗**：
- 守护不周期读 sysfs；频率表、capacity、节点路径在启动时缓存。
- 控制环只处理 eBPF 边沿事件；不做每切换日志、不做每帧字符串格式化。
- 统计口径由外部测试脚本在端点读取，不增加常驻采样。
- 日志默认 `warn`，只记状态变化；日志落盘使用环形缓冲，避免持续写。
- 充电/温控 daemon 不用 shell 循环，不用 `sleep` 轮询。

---

## 5. 合规方案（独立署名）

### 5.1 能做到什么程度

**可以独立署名，但需要“实现隔离”**：
- 版权只保护“表达”，不保护功能、算法思想、系统接口。
- 如果新代码是独立编写的，可以采用自有许可证与署名。
- 但“读过上游代码后凭记忆重写”仍可能产出实质性相似表达；仅改名/改目录不算 clean-room。
- 本方案作者已读过上游以做清理清单；因此**实现阶段必须换一个未读上游的上下文/执行者**，只依据 `docs/behavior-spec.md`（黑盒行为规格）写代码。

### 5.2 明确禁止复用

- 源码、注释、变量/函数/类型命名、模块划分、错误文案、日志文案。
- 配置字段名、层级、注释、默认模板、YAML/JSON/TOML 结构（新工程使用自有 TOML schema）。
- shell 脚本逻辑、安装/卸载/适配脚本模板、模块 id、包名、版本字符串。
- 图标、启动图、WebUI 组件、CSS、i18n 文案、截图、APK、WebUI 构建产物。
- 二进制、`prebuilt` zip、`ci-artifacts`、`updateInformation`。
- 上游参数表、实测 CSV、交付报告文本。
- 上游品牌名（yumi、Aegis、aegis_power_matrix 等）与任何易混淆命名。

### 5.3 新工程声明建议

每个源文件头：

```
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 <作者/项目名>
// Tempo is an independent implementation. No upstream source code is included.
```

`README.md` 声明要点：
- Tempo 是独立实现，不基于、不包含、不派生自 yumi/Aegis 的源码、脚本、配置或资源。
- 行为规格来自公开的 Linux/Android 接口文档与自有真机黑盒测量。
- 列出第三方依赖与许可证（aya、mio、serde、clap、tracing、Kotlin/Compose、Svelte 等）。
- 不使用上游名称/图标；与上游无关联。
- 若未来发现任何文件与上游存在实质性相似，将立即移除并重写。

许可证建议：
- **首选 GPL-3.0-or-later**：与模块生态兼容，降低任何“派生”争议的处置成本；独立实现仍可自有署名。
- 备选 Apache-2.0 / MIT：仅在完成 clean-room 审计、确认无复制后使用。
- 文档可用 CC-BY-4.0 或与代码同许可证。

### 5.4 仍可能存在争议的点

| 争议点 | 风险 | 缓解 |
|---|---|---|
| 经验参数值（0.50、0.28、0.08、150ms 等） | 数值本身是事实，但成组出现可能被视为复制配置 | 重新标定生成自己的默认值；文档只写“参考基线”，不把上游 YAML 搬进仓库 |
| 算法思路（低负载压缩、快速下探、帧截止） | 思想不受版权保护，但表达顺序/结构可能相似 | 用不同数学表达、不同状态机、不同命名；先写规格再实现 |
| 文件布局/模块划分 | 通用工程结构通常不受保护，但过度一致会引发质疑 | 采用不同目录（`crates/`、`power/`、`ui/`、`packaging/`）与不同职责边界 |
| 配置 schema | 字段名/层级可受表达保护 | 使用 TOML + 自有字段名 + 自有默认值生成器 |
| UI 视觉/交互 | 图标、文案、布局可能受版权/商标保护 | 全新信息架构、原创图标、原创文案；不做像素级复刻 |
| 模块脚本结构 | Magisk/KernelSU 有标准接口，通用部分不可避免 | 只保留标准接口必需的最小脚本，其余逻辑进 Rust/C |
| 实测数据 | 数据库/表格可能受汇编版权保护 | 重新测量、重新汇总，不复制 CSV |

---

## 6. 分阶段计划与验收

### 6.1 阶段与里程碑

| 阶段 | 内容 | 交付/验收 | 用户确认点 |
|---|---|---|---|
| **P0 方案** | 本文 + 行为规格纲要 | 用户确认命名/许可证/支持范围/降级策略 | ✅ 必须 |
| **P1 基础骨架** | 新仓库、config schema、profile-gen、CLI/UDS、纯函数控制器骨架、离线单测 | ASCII 路径可构建；profile 与真机三簇一致（266/811/1024；307–2016/499–2803/595–3360）；不写 sysfs | 构建方案确认 |
| **P2 只读事件源** | eBPF runtime/frame、touch、screen、topapp、充电/热 uevent；只观测不执行 | 事件计数正确；静态 10min 守护唤醒 <0.2/s；CPU <0.2%；无固定 timer | ✅ 上机只读验证前 |
| **P3 调度核心** | 单自适应 + Quiet Path + 单 FAS；热部署灰度 | 静态最低频占比 ≥95%；无负载抬频 0；掉频 ≤150ms；滑动/游戏 jank ≤上游；静态能量 ≤0.90× 上游 | ✅ 热部署/刷入前；用户真机确认 |
| **P4 充电/温控** | C daemon：停充/恢复/锁存、热 profile、uevent | 充电阈值准确；锁存跨插电；热保护触发/恢复；无轮询；无重启 | ✅ 刷入前 |
| **P5 WebUI/APP** | Svelte WebUI + Compose APP；状态/配置/日志/充电/温控 | 功能对齐（无模式切换）；无上游资源；配置 round-trip；APK 签名 | UI 评审 |
| **P6 打包发布** | 模块 zip、安装/卸载/升级、回滚、README | 干净安装/卸载；配置保留；功耗验收全过；回滚可用 | ✅ 最终发布前 |

### 6.2 测试纪律（沿用以往经验）

- 先由用户在真机确认，再刷入；优先热部署守护，减少重启。
- 一次只改一个变量；不频繁重启；测试前固定亮度、网络、电量区间、后台应用。
- 采集前等待稳定窗口（≥60s，方差稳定）；异常波动人工复核，不自动抓取异常数据。
- 每个场景至少 3 组交替 A/B；记录环境、版本、commit、配置哈希。
- 模块安装/卸载只在用户确认后执行；保留回滚 zip 与旧二进制。

### 6.3 功耗验收（硬性）

1. **静态阅读（红果小说类）**：30–60min，亮屏、无触摸、同一页面；新模块平均能量 ≤ 0.90× 上游；三簇最低频占比 ≥95%；无 >100ms 的无故抬频。
2. **日常信息流**：滑动/切换；能量 ≤ 1.00× 上游；jank 不高于上游。
3. **视频**：能量 ≤ 1.00× 上游；帧稳定。
4. **游戏**：能量 ≤ 1.00× 上游；jank 不高于上游；无“加载后长期高频不落”。
5. **待机**：8h 息屏能量 ≤ 上游；守护唤醒 <0.2/s。
6. **开销**：`tempod` CPU <0.2% 单核；eBPF <0.5%；无固定周期 timer。

---

## 7. 需要用户决策的问题

1. **项目名/标识**：暂定 `Tempo`、模块 id `tempo_sched`、APP 包名 `dev.tempo.control`、二进制 `tempod`/`tempowerd`/`tempoctl`。是否采用？若你有偏好名称，请现在定，后续路径/包名/签名都依赖它。
2. **许可证**：建议 `GPL-3.0-or-later`（最稳妥）；若你坚持完全自有/宽松许可，则需接受更严格的 clean-room 审计与相似度复核。选哪个？
3. **首发支持范围**：只承诺 `babylon`（8Gen2/Android 16/KernelSU），其他 SoC 作为 Tier B 尽力适配；还是要求首发即支持天玑 9200+ 等第二台设备？（后者需要额外真机与标定周期）
4. **eBPF/内核能力不足时的降级**：严格事件驱动（不可用则不启用调度，明确提示）vs 允许“低频率一次性补偿采样”（会破坏零轮询硬约束）。建议前者。
5. **APP 形态**：Kotlin + Compose 原生（推荐）还是 WebView 套壳（工作量小但不符合“重做”）。
6. **旧配置迁移**：全新模块 id、不读取上游配置（推荐，避免任何模板复用）；是否需要一次性导入当前充电阈值/偏好（仅数值，不导入文件结构）？

---

## 附录 A：真机探测事实（2026-11-06）

- 设备：`2308CPXD0C` / `babylon` / Android 16 / `kalama` / arm64-v8a / KernelSU root 可用。
- 簇拓扑：
  - policy0：CPU 0–2，307200–2016000 kHz，capacity 266
  - policy3：CPU 3–6，499200–2803200 kHz，capacity 811
  - policy7：CPU 7，595200–3360000 kHz，capacity 1024
- tracepoint：`sched_switch` 可用；`drm_vblank_event` 可用。
- 充电节点：`battery`、`usb`、`wireless`、`ucsi-source-psy-*`。
- 热：大量 `cooling_device*` 与 thermal zone，需能力探测后使用。
- 当前上游配置/规则/充电配置已拉取到 `_reference/device-20261106/`（仅对照，不进入新工程）：
  - `up-config.yaml`：当前 `balance` 调度参数
  - `up-rules.yaml`：单模式/FAS 规则（当前 `global_mode=balance`、`fas_enabled=false`）
  - `up-charge.conf`：停充 80%、恢复 70%、高温停充 45°C、恢复 40°C、本次插电锁存开启

## 附录 B：行为规格（实现前需冻结）

实现前把以下内容写成 `docs/behavior-spec.md`，只描述“输入→输出/时序”，不含上游代码结构：
- 事件定义与时间语义；
- 需求窗口计算；
- Quiet Path 进入/退出时序；
- 帧截止与 burst 预算；
- 充电状态机（停充/恢复/锁存/热保护）；
- 热上限恢复；
- 配置 schema 与错误处理；
- 测试口径与期望指标。

实现者只读该规格与公开接口文档，不读上游仓库。
