# Aegis 真机验证记录

> §1–7 为 v0.1.0 早期真机数据（进程名/路径沿用旧 `tempod` 标识）；
> §8 起为 v2.0.0 正式版数据（守护名 `Aegis`，versionCode=3）。

设备：骁龙 8Gen2（`kalama` / `babylon`，本机），KernelSU root。
对照组：厂商默认调度（历史调度模块已 disable，旧守护不参与；
所有厂商服务原样运行）。实验组：Aegis 守护运行并只发布 CPU 地板。

## 1. 静态 A/B（亮屏静置，90 s，按 `time_in_state` 加权平均）

| 簇 | 厂商默认 | Aegis | 比值 |
| --- | --- | --- | --- |
| policy0（小核） | 603,727 kHz | 307,751 kHz | 0.510 |
| policy3（中核） | 630,182 kHz | 499,645 kHz | 0.793 |
| policy7（大核） | 869,616 kHz | 595,475 kHz | 0.685 |

结论：三个簇均 ≤ 0.90× 官方，静态达标。静置期守护 `write` 计数不增长，
`quiet=true`，无周期抬频。

## 2. 触摸抬频与回落

用 `sendevent` 直接向触摸节点注入按下（绕开 InputManager，确保走 evdev）：

| 时刻 | policy0 / policy3 / policy7 |
| --- | --- |
| 触摸前（静默） | 307,200 / 499,200 / 595,200 |
| 按下 +120 ms | 1,785,600 / 2,457,600 / 2,342,400 |
| 按下 +370 ms | 307,200 / 499,200 / 595,200 |

抬频窗口由一次性 `timerfd` 结束，无额外周期定时器；回落 ≤ 250 ms。
`touch.events` 计数随注入递增，验证触摸源确实被事件循环消费。

## 3. 启动与切换（`com.miui.notes`，`dumpsys gfxinfo framestats`）

| 场景 | `am start -W` 首帧耗时 | 帧数 | p50 | p95 | >16.67 ms |
| --- | --- | --- | --- | --- | --- |
| 冷启动（force-stop 后） | 1162 ms | 85 | 2.2 ms | 33.6 ms | 7（8.2%） |
| 应用内热切换（浏览器 → 笔记） | 453 ms | 5 | 2.7 ms | 62.7 ms | 2（40.0%） |
| 最近任务/桌面回切 | 690 ms | 5 | 1.9 ms | 122.6 ms | 2（40.0%） |

说明：热切换与回切只统计到首屏重建的前几帧，样本量小，百分比仅作参考；
首帧耗时与冷启动 p95 为稳定指标。

## 4. Doze 与守护自身开销

息屏 60 s（`input keyevent 223`）后：

- `dumpsys deviceidle`：`mState=IDLE`（已进入 Doze）。
- `tempod` 进程 CPU：息屏 60 s 内累计 **0 ticks**（10 ms/ticks，即 <10 ms）；
  亮屏静置 30 s 同样为 0 ticks。
- 守护无固定周期定时器；息屏后仅保留事件源（BPF ring、evdev、uevent、
  inotify），全部阻塞在 `epoll_wait`。

## 5. FAS（帧感知，默认关闭）

- 默认 `[fas] enabled=false, apps=[]`：`status.fas.active=false`，不执行任何
  `dumpsys gfxinfo`。
- 将 `com.miui.notes` 写入 `/data/adb/tempo/fas.json` 并回到该应用：
  `status.fas.active=true`，仅对白名单前台应用取帧。
- 清空白名单：下一次触摸事件重读 sidecar，`active=false`，即时关闭。
- 白名单应用 `gfxinfo` 95th percentile 超过 16 ms 时抬频，2 s 事件节流。

## 6. 充电控制

- 节点自动发现：`/sys/class/qcom-battery/input_suspend`（stop=1 / start=0）。
- 实测电量 86–89% 时按上限停止充电，`status` 显示 `charge.stopped=true`、
  电池状态 `Discharging`；降到恢复阈值后自动恢复。
- 模块启动与卸载都会执行 `scripts/charge_resume.sh`，避免「停止充电」状态
  在卸载后残留；不写厂商温控与充电限流节点。

## 7. 厂商调度屏蔽（可逆）

`shield.sh apply` 在开机时执行，`shield.state` 记录：

```
svc:miuibooster
svc:perf-hal-2-3
pid:3277:
pid:3279:
pid:3280:
```

即停止 `miuibooster` 与 `perf-hal-2-3`，`SIGSTOP` 暂停 3 个 `millet_monitor`
进程。`mi_thermald`、`android.thermal-hal`、`millet_core` 内核模块均未触碰。

恢复：`sh /data/adb/modules/tempo_sched/scripts/shield.sh restore`（模块卸载时
自动调用），随后 `tempoctl restore` 还原 cpufreq 原值。

## 8. v2.0.0 息屏 3 分钟采样（2026-11-07 05:05–05:08）

测试机（babylon/8Gen2），飞行模式，电量 100（`mIsPowered=false`），屏幕关闭，
`dumpsys deviceidle` 为 `mState=IDLE`。采样窗口 `uptime 5291.99 → 5472.04`
（180.05 s）。

| 指标 | 采样前 | 采样后 | 增量 |
| --- | --- | --- | --- |
| 守护 `Aegis` utime/stime | 15 / 83 | 15 / 83 | **0 tick** |
| voluntary / nonvoluntary ctxt | 1117 / 566 | 1117 / 566 | **0 / 0** |
| c0 `672000` / `787200` tick | 474271 / 51392 | 492244 / 51444 | +17973 / +52 |
| c1 `614400` / `844800` tick | 476305 / 48918 | 494292 / 48951 | +17987 / +33 |
| c2 `864000` / `1132800` tick | 492866 / 33421 | 510849 / 33460 | +17983 / +39 |

上限 `787200 / 844800 / 1132800`、min `672000 / 614400 / 864000` 前后完全
一致，无厂商回写导致的漂移；c1 `729600`、c2 `998400` 仅个位数增量，其余
高频档位全程为 0。180 秒内守护未产生任何一次唤醒。
