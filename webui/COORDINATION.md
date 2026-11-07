# Tempo UI ↔ 守护 待协调清单

- 日期：2026-11-06
- 提交方：UI 任务（`webui/` + `app/`）
- 接收方：调度守护任务（`tempod` / `tempowerd` / `tempoctl`）
- 详细契约：[CONTRACT.md](CONTRACT.md) §2、§3、§7（本节只列缺口与验收口径，字段细节不重复抄写）

UI 侧不改动 `crates/`、`probe/`、`docs/behavior-spec.md`。以下字段名均为**提案**，
最终以 `docs/behavior-spec.md` 更新为准；UI 在规格冻结前不会按猜测字段解析。

---

## A. 缺失命令（UI 已按“可降级”实现）

| 命令 | 提案请求 | 提案响应 | 缺失时 UI 行为 |
|---|---|---|---|
| `config.apply` | `{"cmd":"config.apply","toml":"<完整 config.toml>","expect_schema":1}` | `{"ok":true,"applied":true,"restart_required":false}` 或 `{"ok":false,"error":"config_rejected","detail":"..."}` | 生成完整 TOML → `config.toml.new` → `tempoctl validate` → 备份 → 原子 rename；只能保证落盘 |
| `log.tail` | `{"cmd":"log.tail","limit":200}` | `{"ok":true,"lines":[{"ts":<毫秒>,"level":"warn","msg":"..."}]}` | 读 `/data/adb/tempo/log` 最新文件 `tail -n 200`；无日志显示“日志未启用” |
| `power.get` | `{"cmd":"power.get"}` | 见 §B 充电/热状态 | 显示“充电守护未启用”，只保留阈值编辑 |

验收要点：

1. `config.apply` 必须内部完成“解析 → 校验 → 原子替换 → inotify 重载”全流程，
   失败时保留上一份有效配置并返回具体字段与原因；**不能**让 UI 直接写文件。
2. `log.tail` 需有上限与稳定排序，单次响应不超过 200 行。
3. 三个命令的未知字段处理与现有命令一致：不崩、不断连接。

---

## B. `status` 缺口（UI 已降级或标记为未知）

| 缺口 | 当前 UI 处理 | 提案字段（仅供规格参考） |
|---|---|---|
| 实际频率 | 刷新时客户端读 `policy*/scaling_cur_freq` | `clusters[].cur_khz` |
| 充电状态 | 通过 `power.get` 显示电量、温度与是否充电 | `charge: {plugged, state, latch, stopped_reason}` |
| 100% 锁存 | 页面按 `stop_at_percent == 100` 派生 `latch_per_plug`，80/90 不保持 | 守护按同一规则判定，详见 `CHARGE-CONTROL.md` |
| 温度保护 | 显示电池温度与停充/恢复温度，无单独热上限控件 | `thermal: {temp_c}` |
| 预约时段 | 客户端计算并显示当前是否允许充电 | 守护按 `CHARGE-CONTROL.md` 读取 `charge_plan.json` 后执行 |
| 配置生效代际 | 保存后不做断言 | `config_generation` |
| 是否需要重启 | 文案提示“生效状态未知” | `restart_required` |
| 能力探测 | 固定走文件写入 | `capabilities: ["config_apply","log_tail","power_get"]` |

验收要点：新增字段必须保持向后兼容（旧客户端不因未知字段报错；UI 侧已做可选解析）。

---

## C. 对齐节奏

1. `docs/behavior-spec.md` 冻结字段后，请在该文件里给出 `status` / `config.apply` /
   `log.tail` / `power.get` 的完整示例响应。
2. UI 按规格切换：`status.capabilities` 含 `config_apply` → 优先 UDS，
   否则继续文件写入流程；两者结果都回读 `config.get` 比对。
3. 实际频率字段到位后，UI 删除 sysfs 读取，避免双重来源。
4. 变更 `status` 顶层结构（`stage` / `mode` / `scheduler_writes`）需提前告知 UI，
   这些字段已进入总览页文案。
