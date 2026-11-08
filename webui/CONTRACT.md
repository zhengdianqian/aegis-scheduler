# Tempo UI 接口契约（WebUI / APP 共用）

- 版本：v1.0
- 日期：2026-11-06
- 适用：`webui/`、`app/`（`com.aegis.scheduler`）
- 唯一字段真值来源：`crates/tempo-core/src/config.rs`、`crates/tempod/src/ipc.rs`
- 本文件只描述 UI 侧怎么读、怎么写、字段怎么命名；不修改核心与守护。

---

## 1. 传输层

两种客户端都通过 **root shell** 访问同一个 UDS 与同一份配置文件，因此 WebUI 与 APP 的
字段命名、读取语义、写入流程完全一致。

| 客户端 | 执行通道 | 说明 |
|---|---|---|
| WebUI | KernelSU WebUI 注入的 `window.ksu.exec(cmd, opts, cb)` | 管理器内 WebView；无 `ksu` 时进入演示模式 |
| APP | `libsu`（`Shell.getShell()` / `ShellUtils.execCmd`） | 原生 root shell |

固定路径：

```text
CFG      /data/adb/tempo/config.toml
CFG_NEW  /data/adb/tempo/config.toml.new
CFG_BAK  /data/adb/tempo/config.toml.bak
PROFILE  /data/adb/tempo/device.toml
SOCK     /data/adb/tempo/run/control.sock
LOGDIR   /data/adb/tempo/log
PROP     /data/adb/modules/tempo_sched/module.prop
```

`tempoctl` 统一调用形式（客户端不依赖 PATH）：

```sh
tempoctl --config /data/adb/tempo/config.toml --profile /data/adb/tempo/device.toml status --socket /data/adb/tempo/run/control.sock
```

> 客户端应优先使用模块安装目录下的绝对路径（`/data/adb/modules/tempo_sched/...`），
> 找不到时回退 `command -v tempoctl`。两条路径都由客户端探测，不写死。

---

## 2. 命令能力矩阵

### 2.1 当前已实现（P2 观察模式，可直接使用）

| 命令 | 请求 | 响应 | 用途 |
|---|---|---|---|
| `ping` | `{"cmd":"ping"}` | `{"ok":true,"service":"tempod","version":"x.y.z"}` | 存活探测 |
| `status` | `{"cmd":"status"}` | 见 §3 | 总览 / 状态页 |
| `profile.get` | `{"cmd":"profile.get"}` | `{"ok":true,"profile":{...}}` | 簇拓扑、频率表 |
| `config.get` | `{"cmd":"config.get"}` | `{"ok":true,"toml":"..."}` | 参数页读取 |
| `config.validate` | `{"cmd":"config.validate"}` | `{"ok":true}` / `{"ok":false,"error":"config_rejected","detail":"..."}` | 校验当前文件 |

未知命令返回 `{"ok":false,"error":"unknown_command"}`，连接不断。

> 参数页当前用 `tempoctl --config ... --profile ... config` 读取、用 `... validate` 校验，
> 与 `config.get` / `config.validate` 读写同一份文件，且不要求守护在线。

### 2.2 规划中（尚未实现，客户端必须可降级）

| 命令 | 规划响应 | 缺失时的降级 |
|---|---|---|
| `config.apply` | `{"ok":true,"applied":true,"restart_required":bool}` | 走 §5 文件写入流程 |
| `log.tail` | `{"ok":true,"lines":[{"ts":..,"level":"warn","msg":".."}]}` | 直接 `tail` 日志文件 |
| `power.get` | 充电状态 / 温度 / 热上限 | 显示“充电守护未启用” |

客户端探测能力：仅当 `status.capabilities` 数组包含 `"config_apply"` 时优先用
`config.apply`；当前 `status` 没有该字段，因此一律走文件写入流程。
这一步是为了后续升级不需要改 UI。

---

## 3. `status` 数据模型（只读）

```json
{
  "ok": true,
  "service": "tempod",
  "version": "0.1.0",
  "stage": "P2-observe-only",
  "mode": "observe-only",
  "scheduler_writes": false,
  "schema": 1,
  "platform": "kalama",
  "sdk": 36,
  "cluster_count": 3,
  "clusters": [
    {
      "cpus": [0, 1, 2],
      "role": "efficiency",
      "min_khz": 307200,
      "max_khz": 2016000,
      "capacity": 266,
      "busy": 0.0,
      "foreground": 0.0
    }
  ],
  "uptime_ms": 123456,
  "probe": {
    "state": "starting|ready|disabled",
    "cpu_count": 8,
    "events": 1234,
    "active_events": 123,
    "error": null
  },
  "screen": {
    "on": true,
    "source": "/sys/class/backlight/panel1-backlight"
  },
  "control": {
    "quiet": true,
    "decisions": [
      {"cluster": 0, "ceiling_khz": 307200, "reason": "quiet", "changed": false}
    ]
  }
}
```

约定：

- `clusters` 顺序 = 簇下标（`cluster` 字段就是该下标）。
- `role` ∈ `efficiency | balanced | prime`。
- `reason` ∈ `quiet | screen_off | touch_lift | frame_burst | foreground | load | thermal | unchanged`。
- `busy` / `foreground` 为 0..1。
- `decisions` 可能为空（守护尚未做出决策）；为空时 UI 以 `min_khz` 作为兜底显示。
- `probe.error` 非空 → 总览显示能力告警（调度不可用/观察模式）。
- 事件率 = `events / (uptime_ms/1000)`（次/秒）；UI 只在刷新时计算，不做定时轮询。
- **当前实际频率不在 status 中**，客户端在用户触发刷新时读取：
  `/sys/devices/system/cpu/cpufreq/policy*/scaling_cur_freq`（单位 kHz）。
  读不到就显示 `--`，不伪造。

### 3.1 模块信息（非 status）

| 数据 | 读取 |
|---|---|
| 模块版本 / 作者 / 描述 | `cat /data/adb/modules/tempo_sched/module.prop`（`id/tempo_sched`） |
| 模块是否停用 | `[ -f /data/adb/modules/tempo_sched/disable ]` |
| 守护版本 | `status.version` |

`module.prop` 存在时 UI 显示“已安装”；否则显示“未检测到模块（独立运行）”。

---

## 4. 配置字段清单（写模型）

字段名与 `config.rs` 完全一致（`deny_unknown_fields`，客户端不得发送额外键）。
所有数值均为“单一自适应模式”参数，不提供模式切换、不提供应用白名单。

### 4.1 分组、默认值、合法区间

| 分组 | 字段 | 默认 | 区间 | 单位 | UI 控件 |
|---|---|---|---|---|---|
| daemon | `log_level` | `warn` | `trace/debug/info/warn/error` | — | 下拉 |
| events | `runtime_quantum_us` | 16000 | 500–50000 | µs | 数字 |
| events | `frame_stride` | 2 | 1–16 | 帧 | 数字 |
| events | `quiet_decay_ms` | 60 | 20–2000 | ms | 数字 + 联动 |
| control | `quiet_ratio` | 0.50 | 0.05–0.95 | 比例 | 滑杆 |
| control | `quiet_cap` | 0.28 | 0.05–0.90 | 比例 | 滑杆 |
| control | `floor_enter_ratio` | 0.05 | 0.005–0.20 | 比例 | 滑杆 |
| control | `floor_exit_ratio` | 0.12 | 0.02–0.40 | 比例 | 滑杆 |
| control | `floor_settle_ms` | 60 | 10–1000 | ms | 数字 + 联动 |
| control | `floor_exit_ms` | 60 | 20–500 | ms | 数字 |
| control | `fg_fast_ms` | 100 | 20–500 | ms | 数字 |
| control | `floor_residence_ms` | 0 | 0–2000 | ms | 数字 |
| control | `drop_ratio` | 0.08 | 0.01–0.30 | 比例 | 滑杆 |
| control | `drop_factor` | 4.0 | 1.0–16.0 | 倍 | 滑杆 |
| control | `touch_lift_ms` | 150 | 20–600 | ms | 数字 |
| control | `touch_lift_level` | 0.85 | 0.10–1.0 | 比例 | 滑杆 |
| control | `frame_slack_ms` | 3.0 | 0.5–20.0 | ms | 滑杆 |
| control | `frame_burst_ms` | 150 | 40–1000 | ms | 数字 |
| control | `frame_burst_level` | 0.95 | 0.10–1.0 | 比例 | 滑杆 |
| control | `reserve_ratio` | 1.05 | 1.0–1.25 | 倍 | 滑杆 |
| control | `step_hysteresis` | 0.03 | 0.002–0.20 | 比例 | 滑杆 |
| control.role_cap | `efficiency` | 1.00 | 0.20–1.0 | 比例 | 滑杆 |
| control.role_cap | `balanced` | 1.00 | 0.20–1.0 | 比例 | 滑杆 |
| control.role_cap | `prime` | 0.85 | 0.20–1.0 | 比例 | 滑杆 |
| charge | `stop_at_percent` | 80 | 50–100 | % | 数字 |
| charge | `resume_at_percent` | 70 | 20–stop | % | 数字 + 联动 |
| charge | `temp_stop_c` | 45.0 | 20–70 | °C | 数字 |
| charge | `temp_resume_c` | 40.0 | < temp_stop | °C | 数字 + 联动 |
| charge | `latch_per_plug` | 派生 | `stop_at_percent == 100` | — | 无独立控件，保存时自动写入 |
| thermal | `guard_c` | 45.0 | 20–80 | °C | 数字 |
| thermal | `profile` | `auto` | 非空字符串 | — | 文本 |
| ui | `language` | `zh` | 非空 | — | 下拉（zh/en） |
| fas | `enabled` | false | bool | — | 总开关（默认关） |
| fas | `packages` | `[]` | 包名数组 | — | 逐应用开关，仅勾选应用参与 |
| 顶层 | `schema` | 1 | 必须 1 | — | 只读 |
| daemon | `profile_path` | `/data/adb/tempo/device.toml` | 非空 | 路径 | 只读 |

`[fas]` 语义：默认关闭；关闭或 `packages` 为空时 FAS 完全不工作；仅对 `packages` 内应用启用帧感知；不提供 only/exclude 模式。字段名为提案，最终以 `behavior-spec.md` 为准。

#### 4.1.1 预约充电侧车（`/data/adb/tempo/charge_plan.json`）

用于表达“允许充电的时间段”，不写入核心配置，避免扩展 `[charge]` 字段触发未知字段拒绝。

| 字段 | 默认 | 含义 |
|---|---|---|
| `schema` | 1 | 侧车版本 |
| `enabled` | false | 预约总开关；关闭时不限制充电 |
| `mode` | `everyday` | `everyday` / `workdays` / `custom` |
| `weekdays` | `[1..7]` | 自定义星期，1=周一…7=周日 |
| `windows` | `[{start,end}]` | 允许充电时段，`HH:MM`，最多 6 段 |
| `updated_ms` | 0 | 写入时间戳 |

语义：时段按本机本地时间判断；`end <= start` 视为跨零点；**跨零点时段归属开始日**（周一 23:00–周二 07:00 属于周一）；`enabled=true` 且没有任何有效时段时不限制充电。消费方（充电守护）应在每个判定周期读取该文件并据此停充/恢复。

充电状态机、优先级与边界详见 [CHARGE-CONTROL.md](CHARGE-CONTROL.md)；页面与 APP 均调用 `src/charge-model.js` 的同一套纯函数。

### 4.2 跨字段校验（客户端即时校验 + 服务端最终校验）

| 规则 | 失败提示 |
|---|---|
| `schema == 1` | 契约版本不匹配 |
| `floor_enter_ratio <= drop_ratio` | 进入静默阈值不能高于快落阈值 |
| `floor_exit_ratio > floor_enter_ratio` | 退出静默阈值必须高于进入阈值 |
| `quiet_decay_ms + floor_settle_ms <= 300` | 静默进入预算超 300ms |
| `resume_at_percent < stop_at_percent` | 恢复电量必须低于停充电量 |
| `resume_at_percent >= 20` | 恢复电量不得低于 20% |
| `temp_resume_c < temp_stop_c` | 恢复温度必须低于停充温度 |
| 所有区间见 §4.1 | 字段越界 |

客户端校验只用于即时提示；**最终以 `tempoctl validate` 结果为准**。

### 4.3 共享内部模型（JSON，WebUI 与 APP 一致）

```json
{
  "schema": 1,
  "daemon": {"log_level": "warn", "profile_path": "/data/adb/tempo/device.toml"},
  "events": {"runtime_quantum_us": 16000, "frame_stride": 2, "quiet_decay_ms": 60},
  "control": {
    "quiet_ratio": 0.50, "quiet_cap": 0.28,
    "floor_enter_ratio": 0.05, "floor_exit_ratio": 0.12,
    "floor_settle_ms": 60, "floor_exit_ms": 60, "fg_fast_ms": 100, "floor_residence_ms": 0,
    "drop_ratio": 0.08, "drop_factor": 4.0,
    "touch_lift_ms": 150, "touch_lift_level": 0.85,
    "frame_slack_ms": 3.0, "frame_burst_ms": 150, "frame_burst_level": 0.95,
    "reserve_ratio": 1.05, "step_hysteresis": 0.03,
    "role_cap": {"efficiency": 1.0, "balanced": 1.0, "prime": 0.85}
  },
  "charge": {
    "stop_at_percent": 80, "resume_at_percent": 70,
    "temp_stop_c": 45.0, "temp_resume_c": 40.0, "latch_per_plug": false
  },
  "thermal": {"guard_c": 45.0, "profile": "auto"},
  "ui": {"language": "zh"}
}
```

UI 内部统一用该 JSON 模型；与 TOML 的互转由客户端完成（TOML 是数据库，JSON 是内存视图）。

---

## 5. 写入语义（配置下发）

### 5.1 标准流程（不依赖守护新增命令）

1. 由内存模型生成**完整** TOML（所有分组都写出，避免默认值歧义）。
2. 写入 `CFG_NEW`（`cat > ... <<'EOF'`，保持 UTF-8、LF）。
3. 校验：`tempoctl --config CFG_NEW --profile PROFILE validate`
   - 退出码 0 且输出 JSON `ok=true` → 继续；
   - 否则删除 `CFG_NEW`，UI 展示 `detail`，**不提交**。
4. 备份：`cp -f CFG CFG_BAK`。
5. 原子替换：`mv -f CFG_NEW CFG`（同目录 rename）。
6. 结果判定：
   - `capabilities` 含 `config_apply` → 直接使用 `config.apply` 响应；
   - 否则以 `status` 的 `schema` 与 `config.get` 回读是否等于新模型作为“已落盘”依据；
   - 热更新是否立即生效**不由 UI 断言**：UI 显示“已保存”，并附带
     “守护热更新状态未知/需重启”提示，直到 `status` 里出现 `config_generation`
     或 `restart_required` 字段（见 §7 待协调项）。

### 5.2 并发与失败

- 同一时刻只允许一个写流程（UI 用互斥标记），避免两次 `CFG_NEW` 互相覆盖。
- 任一步失败：中止、保留 `CFG` 原文件、展示失败步骤与 stderr 摘要。
- 不调用 `kill`/重启守护；重启入口不放在 UI（由模块脚本负责）。

### 5.3 回滚

UI 提供“恢复上一份配置”：`cp -f CFG_BAK CFG` + 回读。仅在 `CFG_BAK` 存在时可用。

---

## 6. 日志读取

- 优先 `log.tail`（未实现）。
- 降级：`ls -1t LOGDIR | head -1` 取最新文件，`tail -n 200`；无目录时显示“日志未启用”。
- UI 只做“用户点击刷新/打开页面时读取”，不做定时轮询。
- 展示字段：时间、级别、消息；支持级别过滤与复制。
- 日志内容受 `daemon.log_level` 影响，UI 不修改日志文件。

---

## 7. 待协调项（不在本端修改，需守护/打包侧确认）

| # | 事项 | 影响 | 当前处理 |
|---|---|---|---|
| C1 | `status` 无 `capabilities` | 无法自动探测 `config.apply` | 固定走 §5 文件写入 |
| C2 | `config.apply` 未实现 | 写入只能落盘，无法确认热更新 | UI 标注“已保存，生效状态未知” |
| C3 | `log.tail` 未实现 | 依赖日志文件命名 | 读最新文件，缺失时降级 |
| C4 | 充电/热运行时状态缺失（`tempowerd` 未实现） | 充电页只能编辑阈值，不能展示实时状态 | 显示“充电守护未启用” |
| C5 | 实际频率不在 `status` | 总览频率靠客户端读 sysfs | 刷新时读 `scaling_cur_freq`，失败显示 `--` |
| C6 | `tempod` 是否已接 inotify 热更新 | 保存后是否立即生效未知 | 文案不断言，等 C2/C7 |
| C7 | 建议 `status` 增加 `config_generation`、`restart_required`、`capabilities` | 可精确判断生效 | 客户端已预留字段解析 |
| C8 | 模块打包需把 `webui/dist` 放到 `webroot/` | WebUI 入口 | 由打包任务接入，路径见 §9 |
| C9 | APP 正式签名密钥 | 分发 | 先 debug 签名验证，正式密钥由项目所有者提供 |
| C10 | Magisk 无 WebUI 注入 | WebUI 仅 KernelSU 可用 | APP 为通用入口 |

---

## 8. 页面 / 界面结构

### 8.1 WebUI（`webui/dist`，KernelSU webroot 静态页）

技术：原生 JS（classic scripts）+ CSS，无构建依赖、无外部 CDN、无第三方 UI 框架，`file://` 直接可开；
资源全部自绘（内联 SVG），配色与组件命名自有。

导航：顶部标题 + 底部四页签（总览 / 参数 / FAS / 充电）+ 可关闭的日志抽屉。

| 页 | 内容 | 数据源 |
|---|---|---|
| 总览 | 运行状态卡（版本/平台/静默/运行时长）、按运行时实际簇数动态渲染的频率卡（角色、当前频率、上限、负载/前台）、厂商调度屏蔽的中性提示、刷新按钮 | `status` + `scaling_cur_freq` + `module.prop` |
| 参数 | 四组：静默与压缩、触摸与帧、换档与角色上限、事件采样；滑杆/数字框、即时校验、恢复默认（分组）、保存栏、回滚上一份 | `config.get` → TOML→JSON |
| FAS | 总开关（默认关）、逐应用开关（名称/包名/图标/搜索）、未选应用即不工作的说明 | `pm list packages` + `config.get`（`[fas]`） |
| 充电 | 实时状态卡（电量、是否充电、是否被本模块停充、距上限、预约时段、电池温度）、充电上限与恢复阈值（选择 100% 时先涓流，真正充满后停充并保持，需重新插电恢复）、停充/恢复温度、预约时段编辑 | `config.get` + `power.get` + `charge_plan.json` |
| 日志抽屉 | 运行日志开关、200 行尾部、上限 300 行滚动缓冲、切换参数后自动刷新；可完全关闭 | 日志文件 / `log.tail` |

交互约定：所有写操作需二次确认（底部“保存并校验”）；失败展示具体校验原因；
演示模式不显示额外标识。

### 8.2 APP（`com.aegis.scheduler`，Kotlin + WebView 容器）

底部导航四页，与 WebUI 同构、同字段、同接口：

| 页 | 内容 | 与 WebUI 差异 |
|---|---|---|
| 状态 | 模块状态/版本/静默、按运行时簇数动态展示的频率与上限、刷新、root 状态 | 增加 root 授权状态与“重新检测” |
| 参数 | 同四组参数 + 滑杆 + 保存/回滚 | 保存前调用同一 `validate` |
| FAS | 总开关 + 应用列表逐应用开关 | 使用 PackageManager 取真实名称与图标 |
| 充电 | 阈值/温度/预约 | 同上 |
| 日志 | 尾部日志 + 级别过滤 + 复制分享 | 增加“分享日志” |

APP 不内嵌 WebView；所有数据通过 libsu 调用同一命令，解析同一 JSON DTO。

---

## 9. 产物与路径

```text
webui/
├─ CONTRACT.md          # 本文件
├─ CHARGE-CONTROL.md    # 充电状态机行为规格
├─ COORDINATION.md      # 与守护侧的待协调项
├─ src/                 # index.html / app.js / charge-model.js / api.js / shell.js / mock.js / toml.js / style.css
├─ test/                # charge-model.test.js（Node 独立沙盒）
├─ build.ps1            # 复制 src → dist（无压缩、无网络依赖）
└─ dist/                # 交付物：index.html 及全部脚本、样式
```

打包侧只需把 `webui/dist/*` 放进模块的 `webroot/`（或 KernelSU 约定目录），
入口 `index.html` 不需要任何构建步骤。APP 构建产物输出到
`D:\dev\tempo-build-android\tempo-app\`，与调度侧构建目录隔离。

---

## 10. 命名与合规

- 界面文案中文，字段名沿用 `config.rs` 的 snake_case；不引入任何上游字段、配色、组件名。
- 图标内联 SVG 自绘；无外部字体、无网络图片。
- 源码文件带 `SPDX-License-Identifier: GPL-3.0-or-later` 与 `Copyright (C) 2026 默默的阿文`。
