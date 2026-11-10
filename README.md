# Aegis

**v3.6.5 · versionCode 23**｜Android arm64 CPU 分档自适应调频、可选帧反馈（Aegis Frame）、充电控制。
模块 id：`aegis_scheduler`；APP 包名：`com.aegis.scheduler`；支持通过 KernelSU / Magisk 安装。
作者：**默默的阿文**。本项目为独立编写与独立署名，不包含第三方模块源码，详见 [PROVENANCE.md](PROVENANCE.md)。

模块根据设备实际 cpufreq policy 生成档案，**主动控制每簇 `scaling_min_freq` 和 `scaling_max_freq`**。本次改进自主控制，但不锁频；真实负载可达硬件允许的 max，仍受温控、驱动和外部约束。正常控制不改 governor，保留厂商温控及 thermal / perf HAL。**厂商 XML 覆盖 apply 仍为 `disabled_unverified_hal_timing`，不能声称已完全屏蔽官方调度。** 厂商 QoS、温控和其他写入者仍可能覆盖或限制结果，不能承诺所有手机兼容、零掉帧或固定省电幅度。

默认不按场景或负载压低最高频率，也不会锁定最高频率；省电上限仅在用户为某簇选择小于 `1` 的比例时启用。复核退避、帧反馈超时、启动日志退避和探针熄屏早退均不代表实测功耗或续航收益。厂商策略须校验机型及 OTA 指纹哈希；指纹哈希失配时跳过覆盖，未知机型回退，不强行套用其他机型策略。

## 安装、检查与跨机适配

1. 确认设备已 root，保留可用的模块禁用/救砖途径。先禁用其他调频及充电控制模块并重启，避免备份到其限制值；仅自动迁移旧 `tempo_sched`，不会自动识别所有竞争模块。
2. 首次安装前保存现有 CPU policy、充电状态及旧配置；升级前备份 `/data/adb/tempo/`。**不要把另一台手机的 `profile/` 或 `orig/` 拷到本机使用。**
3. 在模块管理器刷入 `Aegis-v3.6.5.zip` 并重启。模块包含匹配 APK，开机后尝试自动安装/更新；安装失败见 `/data/adb/tempo/Aegis.log`，下次启动重试，也可手动安装 `Aegis-v3.6.5.apk`。已有不同签名 APP 可能无法覆盖安装。
4. 打开 Aegis，在 root 管理器中为 APP **单独授权超级用户权限**。随模块安装不等于已获 root；也可使用模块管理器支持的 WebUI。
5. 服务等待开机完成（最多 180 秒）、解锁检查（最多 120 秒）及额外 30 秒，再生成档案、校验配置、备份并启动守护；默认还有 20 秒 cpufreq 只读保护。不要把启动保护期当成调频失效。
6. 检查状态、探针、频表和读回错误，再逐项检查触摸/长按、冷启动、前台切换、阅读、视频和息屏；充电只允许隔离假节点的沙盒测试，禁止真机充电控制测试。每次只改一类参数，保留原配置和同条件对照记录。

以下在设备的 root shell 执行：

```sh
CTL=/data/adb/modules/aegis_scheduler/bin/tempoctl
$CTL status
$CTL profile
$CTL validate
$CTL power
$CTL logs --lines 80
```

- `status` 用于确认探针、写入开关/错误、当前档位和充电控制状态；`power` 是实时节点快照。目标区间不等于驱动已接受，更不等于当前频率恒定。
- 开机按 `policy*`、关联 CPU、频率边界与 capacity 生成 `/data/adb/tempo/profile/device.toml`，不限定三簇或某个 SoC。capacity 缺失时以最大频率估算角色，并非真实性能测量。
- 频表探测依次尝试 `scaling_available_frequencies`、`stats/time_in_state`、`scaling_available_freqs`，再补最小/最大端点。没有完整频表时无法凭空恢复全部 OPP；稀疏频表会使分档变粗。writer 自身的贴档只读取 `scaling_available_frequencies`，其他来源仍须真机验证驱动接受度。
- 缺少 policy/有效档案会停止启动；探针附着失败时不启用 CPU writer。屏幕事件不可用时按亮屏处理；充电开关不可用时不能停充。这些降级不表示整机已兼容。
- 更换内核/ROM 后重新核实节点、频表和充电语义。不要为消除错误去关闭 thermal、修改节点权限或同时运行多个 writer。

## 控制逻辑与边界

- 每簇从实际频表按索引抽取档位；默认最多 8 档，可设 6–10，原始频点不足时保留现有档位。负载经 EMA 平滑，升档需确认，常规降档一次一档；亮屏、前台切换、应用启动有独立短窗口。**单纯负载上升不作为应用启动事件。**
- 同一触摸接触会话固定一次加档目标，不因持续移动或长按叠加；静态长按不重复续 boost；prime 角色不参与触摸加档，但仍可响应负载、启动和帧压力。
- 低负载下中核偶发顶频来自厂商 `walt`：`pl=1` 的预测会把短时唤醒放大成一次最高频，实测 10 秒轻滑命中 `2707200` 达 80 次。`[governor]` 段只写 `walt/pl` 与 `walt/{up,down}_rate_limit_us`（默认关预测、升频 40 ms 节流、降频不节流），**不设置任何频率上限、不改 min/max**；同场景命中降到 0、平均频率 1187→1038 MHz、顶频驻留 9~10 ms→0~1 ms，Settings 滑动 0 掉帧、冷启动耗时无回归。降频加节流会把高频“挂”住并抬高平均频率，故默认不给降频节流。
- 阅读/视频/输入法场景按前台包分类。单次 UI 帧超预算提供短时救援；连续至少 3 次有效超预算反馈才提高阅读/视频自适应地板，健康反馈重置连续计数，反馈间隔过长重新计数。场景改变重置自适应地板；这不是视频解码质量检测。
- 模块写两端并读回验证，必要时先降低 min 或先提高 max，避免上下限次序冲突。外部改写/驱动拒绝会报告错误并有限退避重试。观察到外部降低 max 后，floor/cap 不越过该约束；只有外部明确放宽才解除。该机制不是内核原子仲裁，无法保证捕获两次读取之间的瞬时限制。正常写入不改 thermal 节点；对 governor 的唯一写入是上述换档节奏（`walt/pl`、`walt/{up,down}_rate_limit_us`），不改档位表与 min/max。内核温控仍可能钳制频率。
- **不是零轮询**：BPF 有需求观察窗口，探针增加熄屏早退；CPU 区间复核在稳定时退避，异常时仍需检查和重试，看门狗也保留周期检查。帧反馈请求采用最新单槽与超时约束；启动日志读取失败采用指数退避。充电沙盒逻辑仍包含预约/停充复核、涓流及写失败重试，控制台在可见总览刷新状态。这些都是代码层面的唤醒与失败路径改进，不能据此声称真机省电。

### 帧反馈与控制界面

APP / WebUI 共用控制服务和配置，提供状态、调度、帧反馈选包、充电与日志。应用列表异步加载；包名是匹配依据，显示名称不参与调度。帧反馈默认关闭，需打开总开关并选中应用；空列表不是“全部应用”。

帧反馈在选中的前台应用上异步读取 `dumpsys gfxinfo <包名> framestats`，仅使用新增帧（最多最近 60 帧）的最慢耗时反馈。它测量的是 **Android UI 帧时间，不是视频解码帧、视频输出帧或全部 GPU 渲染路径**；没有有效新帧不能证明流畅，不能以此保证视频 0 掉帧。帧预算需匹配刷新率/场景，默认 16 ms 不适合所有应用。

### 充电控制

**本候选版充电仅允许沙盒测试，禁止真机充电控制测试或真机启用验证。** 以下仅说明代码逻辑，不是硬件安全性或充停效果的验收结论。

- 默认 80% 停充、降至 70% 复充；温度默认 45°C 停充、降至恢复门槛 40°C 或以下后解除温度阻断。优先级为温度、100% 满充锁存、普通上限、预约窗口；解除一项阻断不保证立即充电。
- 上限设为 100% 时，**电量显示 100 并不会立即断充**：需驱动 `status=Full`，或持续小电流证据（默认绝对值不高于 100 mA 持续 9 秒；可附加电压门槛）才锁存。锁存按插电会话保持，重新插电、重启或相关配置改变会重置。电流单位、正负号、电池状态必须按本机驱动核实。
- 预约支持 `everyday` / `workdays` / `custom`、周一至周日 `1..7`、最多 6 个 `HH:MM` 时段；跨零点的后半段归属开始日。停用或无有效时段不限制充电，开始与结束相同不是全天。
- 从已知候选节点中选第一个可写开关：`qcom-battery/input_suspend` 或 battery 下的 `input_suspend`、`battery_input_suspend`、`charging_enabled`、`battery_charging_enabled`、`charge_enabled`。`input_suspend` 类停/开为 `1/0`，enabled 类为 `0/1`。可写不代表语义一定正确，可能同时影响输入供电，并非通用旁路充电。
- 写入失败须以错误和读回结果为准，不能把逻辑“允许/禁止”当作硬件事实。沙盒使用成对的 `AEGIS_BATTERY_DIR` / `AEGIS_CHARGE_SWITCH` 隔离假节点；**沙盒逻辑验证不能证明真实节点、充停与恢复安全，当前禁止转为真机测试**，勿在日常服务中设置测试环境变量。

## 配置速查

实际配置：`/data/adb/tempo/config.toml`；默认模板：[packaging/config.toml](packaging/config.toml)。`schema = 1`，未知字段会被拒绝；旧数据目录名 `tempo` 与程序名 `tempoctl` 仍沿用，不要自行改名。帧反馈侧车 `aegis-frame.json`、预约 `charge_plan.json` 与主配置同目录，建议用界面维护，避免主配置与侧车互相覆盖。

下表默认值指**模块模板**，不是 Rust 内部缺省值；完整约束以 [config.rs](crates/tempo-core/src/config.rs) 的 `validate()` 为准。

| 类别 | 当前生效字段及含义 |
| --- | --- |
| `[daemon]` | `log_level="off"`（默认不写日志，可用界面临时改 `info` 排查）；`profile_path="/data/adb/tempo/profile/device.toml"`（模块服务也显式传入该路径） |
| `[governor]` | `smooth=true` 总开关；`prediction=false`（对应 `walt/pl`）；`up_rate_limit_us=40000`（0–100000）、`down_rate_limit_us=0`（0–200000）、`reapply_ms=60000`（0–600000）。只改换档节奏，不设置任何频率上下限；界面上这五项保存后立即生效 |
| `[events]` | `runtime_quantum_us=250000`（500–300000 µs）、`demand_step=0.10`（0.01–0.20）：探针观察与发布阈值；`screen_path=""` 自动发现屏幕节点；`quiet_decay_ms=60`：最近触摸后的静默等待 |
| `[control]` 静默 | `floor_enter_ratio=0.05`、`floor_exit_ratio=0.12`（退出必须高于进入）、`floor_settle_ms=60`；`quiet_decay_ms + floor_settle_ms ≤ 300` |
| 负载分档 | `tier_table_max=8`（6–10）、`up_confirm_ms=120`（40–400）、`down_step_ms=45`（15–200）、`max_up_step=2`（1–4）、`demand_smooth_ms=200`（50–1000） |
| 交互窗口 | `touch_lift_tiers=1`（1–3）、`touch_lift_ms=150`（20–600）、`launch_lift_ms=1000`（200–1500）、`launch_tier_ratio=0.62`（0.30–0.90）；比例按档位索引计算，不是固定 GHz |
| 目标与可选上限 | `prime_cap_ratio=0.70`、`ime_cap_ratio=0.45` 约束调度目标档位，不是最高频率限制。`[control.cluster_cap_ratio]` 安装/开机时按真实簇拓扑自动生成（1 簇、2 簇、3 簇机型都各自适配，并清理不属于本机的旧键）；`1.0` 不施加省电上限，`0.5` 按硬件最高频率的 50% 向下贴档，不低于硬件最低频率。温控及外部限制仍有效。 |
| 帧反馈 | `frame_slack_ms=3.0`、`frame_burst_ms=150`、`frame_burst_level=0.70`、`pressure_hold_ms=1000`：超预算余量、短时救援及持续压力保持 |
| 启动保护 | `startup_guard_ms=20000`（5000–120000）：守护启动后的 cpufreq 只读期，不表示充电控制也暂停 |
| `[aegis_app]` | `reading_floor_tier` / `video_floor_tier` / `default_floor_tier` 默认 0，允许 0–5、按实际档位截断；`reading_packages` / `video_packages` / `ime_packages` 为分类包名列表，各最多 64 项，不等于帧反馈选包 |
| `[aegis_frame]` | `enabled=false`、`apps=[]`（最多 64）、`frame_budget_ms=16.0`（6–40 ms）；侧车保存界面选包 |
| `[charge]` | `stop_at_percent=80`（50–100）、`resume_at_percent=70`（至少 20 且小于上限）；`temp_stop_c=45.0`、`temp_resume_c=40.0`（恢复温度必须低于停充温度） |
| 满充证据 | `full_current_ma=100`（20–1000 mA）、`full_hold_seconds=9`（1–60 s）、可选 `full_voltage_mv`（3000–5000 mV，默认不配置） |
| `[thermal]` | `guard_c=45.0`（20–80°C）：旧兼容字段；当前 daemon 不据此施加附加 CPU 上限，原厂硬件温控保持有效 |
| `[ui]` | `language="zh"`：界面语言配置；当前界面主要为中文 |

**旧兼容字段不等于生效调参项。** 当前分档控制不读取以下字段来决定频率，但仍解析并校验，勿随意删除约束或填写越界值：

- `[control]` 的 `quiet_ratio`、`quiet_cap`、`floor_exit_ms`、`fg_fast_ms`、`floor_residence_ms`、`drop_ratio`、`drop_factor`、`touch_lift_level`、`touch_ramp_ms`、`touch_ramp_stages`、`touch_decay_ms`、`launch_lift_level`、`prime_lift_level`、`prime_load_cap`、`ceiling_headroom`、`reserve_ratio`、`step_hysteresis`、`parked_headroom`，及 `[control.role_cap]` 全组。
- `[events].frame_stride` 不用于当前 gfxinfo 抽帧；`[thermal].profile` 未选择实际温控曲线；`[charge].latch_per_plug` 的实际锁存逻辑由 `stop_at_percent == 100` 推导。

3.4 已从默认模板移除未定义且不生效的 `flat_ceiling_bias`、`task_fit_margin`、`max_single_burst_us`、`log_cooldown_us`、`pose_swap_grace_us`。迁移旧配置时也应删除这些项，再执行 `--check`；不要把不存在的参数当作调优开关。

修改前复制配置为候选文件，经校验再应用（root shell）：

```sh
cp /data/adb/tempo/config.toml /data/local/tmp/aegis-candidate.toml
# 编辑候选文件后：
$CTL --config /data/local/tmp/aegis-candidate.toml \
  --profile /data/adb/tempo/profile/device.toml validate
$CTL apply --file /data/local/tmp/aegis-candidate.toml
```

查看返回的 `ok` / `restart_required`。改变探针窗口、发布阈值、日志级别、档案路径或启动保护需重启守护（推荐重启设备）；应用配置可能重置控制状态，之后重新检查。服务启动时无效配置会尝试 `.bak`，再尝试模块默认配置，仍无效则不接管。

## 界面参数怎么设（教学）

APP 与 WebUI 使用同一份配置。滑杆**必须按住小圆圈拖动**才改值，点轨道空白处不会跳变；松手后才写入配置草稿，再点“保存”才下发。数字框可直接输入，保存后界面会以守护读回的结果回填。

### 变频节流（walt）

| 参数 | 默认 | 怎么设 |
| --- | --- | --- |
| 启用换档接管 | 开启 | 关闭后完全保持厂商 walt，下面四项同时失效。只有想对比原厂行为时才关 |
| 预测换档 | 关闭 | 对应 `walt/pl`。低占用时偶尔跳到最高频，主要就是它把短时唤醒放大了；保持关闭 |
| 升频最小间隔 | 40000 µs | 两次升频的最小间隔。0 = 不限制；越大平均频率越低。轻滑场景 40000 已能明显压下顶频尖峰；调更大要自己重测冷启动和滑动是否仍 0 掉帧 |
| 降频最小间隔 | 0 | 0 = 负载一落就降档。**不要加**：加节流会把高频“挂”住，平均频率反而更高 |
| 参数补写周期 | 60000 ms | 被厂商或其它模块改回默认时的重写周期。0 = 只在启动/保存时写一次。频繁被改回再调小 |

这五项**都不是频率上限**，也不会锁最高频；它们只决定多久允许换一次档。保存后立即生效，不需要重启。

### 每簇自愿省电（选配）

- 界面按设备**真实簇拓扑**生成滑杆，例如 `CPU 0, 1, 2`、`CPU 3, 4, 5, 6`、`CPU 7`；两簇机型就只出现两行。刷同一个 ZIP 不会套用别的手机的模板，开机时也会自动补键、删掉不属于本机的旧键。
- `1.0` = 不限制（默认）；`0.8` = 该簇最高频率约 80%，向下贴到实际 OPP；低于硬件最低频时夹到最低频。
- 这是**唯一**会主动锁该簇最高频的地方。不确定就保持 1.0，先用“变频节流”改善尖峰。

### 静默与压缩 / 触摸与帧 / 换档与目标档位

- 静默组决定前台空闲时能压多低。“进入阈值”越低越容易进入静默，“退出阈值”必须更高，两者相差太小会来回抖。
- 触摸组只给一次加档（`touch_lift_tiers`），长按/持续滑动不会叠加；调大更容易跟手，但也更容易把中核拉高。
- 目标档位比例（`prime_cap_ratio`、`ime_cap_ratio`）限制的是**调度目标档位**，不是硬件上限；真实重载仍可继续往上走。

### 采样与确认

- 采样越稀，守护自身唤醒越少；`runtime_quantum_us` 与 `demand_step` 改大更省电，但反应变慢。改这两项需要重启服务。
- 日志级别默认 `off`，完全不写日志文件。需要排查时临时改 `warn`/`info`，排查完改回 `off` 并保存。

### 充电与温度

- 停充阈值 80%、恢复阈值 70% 是最常用的组合；恢复必须低于停充。
- 上限设 100% 时不会在显示 100% 时立刻断充，要等涓流小电流持续足够久才锁存停充。
- 温度保护到温停充、回落复充；这是充电保护，不改 CPU 频率。

## 备份与回滚

写入前在 `/data/adb/tempo/orig/` 保存本次开机的原始 min/max/governor，按 boot id 识别；守护另存 `run/cpufreq-backup.json`。**恢复只写 min/max，不改 governor、thermal 或限制节点**，跨开机/跨手机旧备份不可用于强行恢复。

```sh
# 在线恢复 CPU 上下限，并暂停本守护实例继续调频
/data/adb/modules/aegis_scheduler/bin/tempoctl restore

# 完整停用：先在模块管理器禁用，然后在 root shell 清理当前实例
sh /data/adb/modules/aegis_scheduler/uninstall.sh

# 服务已停止时的离线 CPU 恢复（仅当前 boot 备份有效）
sh /data/adb/modules/aegis_scheduler/scripts/restore.sh
```

确认读回成功后重启；要切回旧模块，应先停用本模块并重启，再启用旧模块，不要同时运行。直接运行离线恢复脚本不能阻止仍存活的 writer 再写入。正常退出会尝试恢复 CPU 和充电开关原值；异常退出看门狗兜底针对 CPU，**不保证强杀后充电开关自动恢复**，遇到停充需检查并使用已核实的本机恢复方式或重启。开机健康标记未清除而再次启动时，服务会禁用模块；先看日志再决定是否重新启用。

## 从源码构建（Windows PowerShell）

准备 Rust/Cargo 与 `aarch64-linux-android` target、Android NDK（默认 API 26）、探针要求的 Rust nightly/rust-src 与 `bpf-linker`（见 `probe/rust-toolchain.toml`）、JDK 17+、项目兼容的 Gradle、Android SDK 36、Build Tools 35.0.0 或 36.0.0、Python 3。首次依赖下载需要网络或完整缓存。输出路径使用纯 ASCII。

在仓库根目录执行，路径按本机修改：

```powershell
$env:ANDROID_NDK_HOME = 'D:\Android\ndk\android-ndk-r29'
$env:ANDROID_HOME = 'D:\Android\Sdk'
$env:TEMPO_ANDROID_TARGET_DIR = 'D:\aegis-build\android'
$env:TEMPO_PROBE_TARGET_DIR = 'D:\aegis-build\probe'
$env:AEGIS_APK_OUTPUT_DIR = 'D:\aegis-build\output'
rustup target add aarch64-linux-android

# 探针；脚本要求 bpf-linker.exe 所在目录，自动调用时默认 D:\dev\bpflinker
.\scripts\build-probe.ps1 -BpfLinker 'D:\dev\bpflinker'
# 自动重建探针，再执行 cargo build --target aarch64-linux-android --workspace --release
.\scripts\build-android.ps1 -Release
# 重建 WebUI、同步 APP assets、Gradle release、正式签名验证并输出 APK
.\app\build.ps1
.\scripts\build-module.ps1 -Version '3.6.5' `
  -BuildDir $env:TEMPO_ANDROID_TARGET_DIR `
  -Apk 'D:\aegis-build\output\Aegis-v3.6.5.apk' `
  -OutputDir 'D:\aegis-build\output'
```

`build-module.ps1` 只打包现有 Android 二进制、`webui/dist/` 和匹配的已签名 APK，不会代替编译。需要直接使用 Cargo 时，先构建非空 `probe/dist/tempo.bpf.o`，设置 NDK linker、CC/AR 和 `CARGO_TARGET_DIR`（参照 `scripts/build-android.ps1`），再运行 `cargo build --release --target aarch64-linux-android --workspace`。不能只编译宿主程序后改名打包。

正式 APP 构建需本机 `app/keystore/keystore.properties` 提供 `storeFile`、`storePassword`、`keyAlias`、`keyPassword` 并指向正式密钥库；发布脚本要求 Aegis 正式证书、v2/v3 签名且不可调试，不会回退 debug。**密钥、密码、签名配置、SDK 本机路径及下载依赖不要提交仓库。** 候选产物版本应为 3.6.5 / 23，脚本从 Gradle 读取版本并核验成品；依赖缺失或校验失败必须停止发布，不得跳过签名检查。

## 验证状态与许可

此 README 描述当前实现与验证方法，**不是泛机型验收报告**。未运行的构建、回归、跨机节点验证及功耗测试不记为成功；旧版本的帧率、电流和频率记录不能代表本版结果，故不在此沿用。正式适配需记录设备/内核/ROM、参数、节点读回、测试条件及同条件对照；降低频率不自动等于降低整机功耗。

### v3.6.5 本机实测（小米 MIX FOLD 3 / 8 Gen 2 领先版，SM8550-AC）

- 超大核硬件最高频 `cpuinfo_max_freq = 3360000`，`profile/device.toml` 已包含该频点；直接把 `scaling_max_freq` 写为 `3360000` 可稳定保持，说明没有被模块或内核硬锁。守护的调度上限只会 ≤ 硬件最高频，重载时可达硬件最高频。
- 唯一会主动锁定某簇最高频的是 `[control.cluster_cap_ratio]`；默认 `1.0` 不限制。
- 中核（policy3）设置页滑动实测：
  - **轻量滑动**（每 1.2 秒一次、每次约 0.4 秒）：平均 **955 MHz**，0 掉帧（99th 14 ms）——达到 844–1000 MHz 的目标。
  - **持续快速滑动**（连续 10 秒双向快滑）：约 **1.23 GHz**，0 掉帧（99th 9 ms）。这一场景的平均值由守护按负载设置的地板决定，不是 walt；要再压低只能减少地板档位，属于限制频率，不做。
  - **冻结守护、只看 walt**（同条件对照）：厂商平均 1187–1208 MHz、顶频命中 80 次；本版默认（关预测 + 40 ms 升频节流 + 不限制降频）约 1038 MHz、命中 0 次；再加到 100 ms 约 1028 MHz（收益趋近 0）。冷启动无回归。

GPL-3.0-or-later，见 [LICENSE](LICENSE)。Copyright (c) 2026 默默的阿文。
