# 5.2.0 真机验证清单（人工执行）

> 本清单覆盖：eBPF 负载采集精度增强包（含回退）、5.2.0 两项 BUG 修复
> （面板漂移、开机助推备份污染）、圆环 1:1 显示。全部命令以 `su` 执行；
> 设备：babylon / HyperOS Android 16 / walt / kalama（8 Gen 2）为基准机型。

## 0. 准备

- [ ] 部署 5.2.0 模块 ZIP（刷入后重启），确认 `module.properties` 显示
      `version=5.2.0 / versionCode=30`，APP 显示 v5.2.0。
- [ ] 确认配置保持 `[mode] name = "AegisAuto"`、`log_level` 建议临时调为
      `info`（验收后按需改回 `off`）。
- [ ] 旧备份自愈检查：`cat /data/adb/tempo/orig/policy0/min_freq` 应 ≤ 频率跨度
      中点（5.2 起 `cpufreq_backup.sh` 对同开机的旧备份做净化重写）；
      若文件仍为 2016000 这类助推值，手工 `rm -rf /data/adb/tempo/orig` 后
      重跑 `sh /data/adb/modules/aegis_scheduler/scripts/cpufreq_backup.sh`。

## 1. 负载源与回退（核心验收）

- [ ] **默认走精度增强包**：亮屏使用几分钟后
      `tempoctl --config /data/adb/tempo/config.toml status --socket /data/adb/tempo/run/control.sock`
      → `load_source == "ebpf"`、`load_source_reason == null`、`probe.state == "ready"`。
- [ ] **日志**：`grep "负载源" /data/adb/tempo/Aegis.log` 出现
      `精度增强包（eBPF）已启用`（仅启动时一条）。
- [ ] **WebUI 总览**：hero 卡片“负载源”显示 `eBPF 增强`。
- [ ] **回退验证（模拟加载失败）**：
      1. `echo unavailable > /data/adb/tempo/ebpf-probe.cache` 前先按格式写全
         （`kernel=$(uname -r)`、`version=5.2.0`、`result=unavailable`、
         `reason=manual test`），或直接删除 tempo.bpf.o 前提不可行——推荐写缓存；
      2. 重启守护（`pkill -f /data/adb/modules/aegis_scheduler/bin/Aegis` 后由
         service/watchdog 拉起，或重启设备）；
      3. 断言：`load_source == "proc_stat"`、`load_source_reason` 含
         `探测缓存命中` 与 `manual test`、`probe.state == "disabled"`、
         **调度仍正常**（mode.active 可接管、频率随负载变化）；
      4. 日志出现 `跳过 eBPF 加载`。
- [ ] **缓存失效重探**：把缓存里 `version` 改成 `5.1.0` 再重启 → 回到 eBPF
      路径且缓存重写为 `result=available`（或新原因的 unavailable）。
- [ ] **运行时安全阀**（可选，需构造条件）：日志出现
      `运行时安全阀…已永久回退` 后 `load_source` 变为 `proc_stat`，
      且频率决策无跳变（观察 `clusters[].target_khz` 平滑）。

## 2. 标准回退源的数据质量（proc_stat 路径）

- [ ] 回退状态下静置桌面 10 s：`clusters[].busy` 接近 0、`usage` 与 WebUI
      “占用·采样”一致（同口径），`foreground`（前台）≤ busy。
- [ ] 打开微博/信息流滑动：busy 升至与 CPU 占用工具（如 Scene/PerfDog）同量级；
      目标频率随负载上行；停止滑动 1 s 内回落。
- [ ] 静态限顶在回退源下仍生效：看小说 1 min，`status.control.static_cap == true`，
      顶频被 `quiet_ratio` 收缩。

## 3. BUG 修复回归（面板漂移导致永久待机 / 频率卡高）

- [ ] **面板监听**：启动日志 `display state source attached panels=…` 应列出
      **全部**面板（如 `panel0-backlight+panel1-backlight`）；`screen.source` 同步。
- [ ] 亮灭屏各 3 次（物理键）：`status.screen.on` 与实际一致（≤1 s 跟随）；
      亮屏后 `mode.active == true`（接管中），息屏后 `mode.active == false`
      且 min/max 交还。
- [ ] **开机卡高回归**（原 BUG 场景）：重启设备 → 解锁 → 静置 2 min →
      各簇 `scaling_min_freq == cpuinfo_min_freq`（无开机助推残留锁频）、
      `scaling_cur_freq` 随负载起落；`actual_freq_khz` 不再恒贴硬件最高频。
- [ ] 息屏 10 min（或过夜抽样）：`dumpsys battery` 无异常耗电回归；
      `status.screen.doze` 正确；唤醒后接管恢复。
- [ ] 备份净化：`cat /data/adb/tempo/run/cpufreq-backup.json` 的 `min_freq`
      均 ≤ 该簇频率跨度中点（开机助推值已被净化为硬件最低频）。

## 4. 圆环 1:1 显示（APP 与 WebUI）

- [ ] WebUI/APP 总览页三簇圆环：把 `scaling_cur_freq` 手工对比
      （`cat /sys/devices/system/cpu/cpufreq/policyX/scaling_cur_freq`），
      圆弧百分比 ≈ `cur ÷ cpuinfo_max_freq`（误差 ≤1 档显示取整），
      不再出现“2.02/2.016 GHz 却显示非满弧、1.0 GHz 却显示接近满弧”类错位。
- [ ] 5 s 轮询刷新与整页刷新口径一致。

## 5. 无回归项（与 5.1 行为一致）

- [ ] AegisAuto 接管：亮屏锁频路径 `mode.path == "pin"`（本机无 userspace）。
- [ ] 触摸抬档、启动抬档、输入法不来回跳动、看剧/小说静态压缩参数行为不变
      （本轮未动决策算法）。
- [ ] 帧感知默认关闭现状不变（`frame.enabled == false`）。
- [ ] 三层看门狗：`pkill -USR1` 心跳正常；`kill -9` 后 `restore.sh` 从
      净化后的备份恢复（min 回到硬件最低频而非助推值）。
- [ ] 宿主回归：`cargo test --workspace` 全绿（138 项）、`node --test webui/test/*.test.js` 全绿。

## 6. 留档

- [ ] 记录机型/内核（`uname -r`）、`load_source`、回退原因（如有）、
      开机 2 min 各簇 min/cur 频率截图，归档到发布 Issue。
