/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * charge-model.test.js —— 充电控制状态机的独立沙盒。
 *
 * 运行：node webui/test/charge-model.test.js
 * 不依赖真机、不依赖守护进程、不写任何文件，只跑纯函数并打印“预期 vs 实际”。
 */
'use strict';

const C = require('../src/charge-model.js');

const rows = [];
let passed = 0;
let failed = 0;

function visualWidth(text) {
  let width = 0;
  for (const ch of String(text)) {
    width += /[\u2e80-\u9fff\u3000-\u303f\uff00-\uffef]/.test(ch) ? 2 : 1;
  }
  return width;
}

function padEnd(text, width) {
  const value = String(text);
  return value + ' '.repeat(Math.max(0, width - visualWidth(value)));
}

function planOf(overrides) {
  return Object.assign({
    enabled: true,
    mode: 'everyday',
    weekdays: [1, 2, 3, 4, 5, 6, 7],
    windows: [{ start: '23:00', end: '07:00' }]
  }, overrides || {});
}

function input(overrides) {
  return Object.assign({
    now: new Date(2026, 10, 2, 12, 0),
    percent: 60,
    tempC: 33,
    plugged: true,
    stopAt: 80,
    resumeAt: 70,
    tempStop: 45,
    tempResume: 40,
    latchPerPlug: false,
    plan: { enabled: false, mode: 'everyday', weekdays: [1, 2, 3, 4, 5, 6, 7], windows: [{ start: '23:00', end: '07:00' }] }
  }, overrides || {});
}

function full(overrides) {
  return input(Object.assign({
    stopAt: 100,
    resumeAt: 70,
    latchPerPlug: true,
    currentMa: 1200,
    statusFull: false
  }, overrides || {}));
}

/* 涓流阶段的时间轴：T0 为 2026-11-02 12:00:00，tick(n) 表示第 n 秒。 */
const T0 = new Date(2026, 10, 2, 12, 0, 0).getTime();
function tick(seconds) {
  return new Date(T0 + seconds * 1000);
}

function trickle(overrides) {
  return full(Object.assign({ percent: 100, currentMa: 80, statusFull: false }, overrides || {}));
}

function describeResult(result, withPhase) {
  return (result.allowCharge ? '允许' : '停充') + ' | ' + result.action + ' | ' + (result.primary || '-') +
    (withPhase ? ' | ' + result.phase : '');
}

function describeExpected(expected) {
  return (expected.allow ? '允许' : '停充') + ' | ' + expected.action + ' | ' + (expected.primary || '-') +
    (expected.phase ? ' | ' + expected.phase : '');
}

function record(scenario, step, expected, actual, pass, note) {
  if (pass) passed += 1;
  else failed += 1;
  rows.push({
    scenario,
    step,
    expected,
    actual,
    pass,
    note: note || ''
  });
}

function simulate(scenario, steps) {
  let state = C.createState({});
  steps.forEach((item, index) => {
    const label = item.label || ('步骤 ' + (index + 1));
    const result = C.step(state, item.input);
    state = result.state;
    const expectedText = describeExpected(item.expect);
    const actualText = describeResult(result, !!item.expect.phase);
    const pass = result.allowCharge === item.expect.allow &&
      result.action === item.expect.action &&
      (result.primary || '-') === (item.expect.primary || '-') &&
      (item.expect.phase ? result.phase === item.expect.phase : true);
    record(scenario, label, expectedText, actualText, pass, result.reasons.map((reason) => reason.code).join('>'));
  });
  return state;
}

function check(scenario, step, expectedText, actualText, pass) {
  record(scenario, step, expectedText, actualText, pass);
}

/* ---------- 1. 普通上限 80% ---------- */
simulate('普通上限 80%', [
  { label: '79% 充电中', input: input({ percent: 79 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '到达 80%', input: input({ percent: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '回落到 75%', input: input({ percent: 75 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 71%', input: input({ percent: 71 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 70%', input: input({ percent: 70 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '继续回落到 69%', input: input({ percent: 69 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '再次到达 82%', input: input({ percent: 82 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } }
]);

/* ---------- 2. 普通上限 90% ---------- */
simulate('普通上限 90%', [
  { label: '89% 充电中', input: input({ percent: 89, stopAt: 90, resumeAt: 80 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '到达 90%', input: input({ percent: 90, stopAt: 90, resumeAt: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '回落到 85%', input: input({ percent: 85, stopAt: 90, resumeAt: 80 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 80%', input: input({ percent: 80, stopAt: 90, resumeAt: 80 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '再次到达 82%', input: input({ percent: 82, stopAt: 90, resumeAt: 80 }), expect: { allow: true, action: 'none', primary: null } }
]);

/* ---------- 3. 100% 涓流至满充：锁存与重新插电 ---------- */
simulate('100% 涓流至满充 / 重新插电', [
  { label: '99% 充电中', input: full({ percent: 99, now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'charging' } },
  { label: '到达 100%（大电流）', input: full({ percent: 100, currentMa: 1200, now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流 0s（80mA）', input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流 5s', input: trickle({ now: tick(5) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流满 9s', input: trickle({ now: tick(10) }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } },
  { label: '锁存后保持（0mA）', input: trickle({ currentMa: 0, now: tick(13) }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '锁存后回落到 95%', input: full({ percent: 95, currentMa: 0, now: tick(20) }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '拔出充电器', input: full({ percent: 95, plugged: false, now: tick(30) }), expect: { allow: true, action: 'none', primary: null } },
  { label: '重新插电（100% 大电流）', input: full({ percent: 100, currentMa: 1200, now: tick(40) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重新插电后涓流 0s', input: trickle({ now: tick(40) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重新插电后涓流 5s', input: trickle({ now: tick(45) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重新插电后涓流满 9s', input: trickle({ now: tick(50) }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } },
  { label: '再次锁存后回落到 99%', input: full({ percent: 99, currentMa: 0, now: tick(55) }), expect: { allow: false, action: 'hold', primary: 'full_latch' } }
]);

/* ---------- 4. 上限配置变更 ---------- */
simulate('上限配置变更', [
  { label: '100% 策略下 85%', input: full({ percent: 85, currentMa: 1200 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '100% 策略下充满（大电流）', input: full({ percent: 100, currentMa: 1200 }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '改为 80% 上限', input: input({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '回落到 70%', input: input({ percent: 70 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '改回 100% 上限', input: full({ percent: 70 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '改回后充满（大电流）', input: full({ percent: 100, currentMa: 1200 }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } }
]);

/* ---------- 5. 预约时段：跨零点与跨星期 ---------- */
function planCase(label, plan, date, expectedAllowed, expectedDetail) {
  const state = C.planState(plan, date);
  const actualText = (state.allowed ? '允许' : '停充') + '：' + state.detail;
  const pass = state.allowed === expectedAllowed && state.detail.includes(expectedDetail);
  check('预约时段', label, (expectedAllowed ? '允许' : '停充') + ' 且详情含「' + expectedDetail + '」', actualText, pass);
}

planCase('周一 23:30 进入时段', planOf(), new Date(2026, 10, 2, 23, 30), true, '本段还可充 7 小时 30 分（07:00 结束）');
planCase('周二 02:00 跨零点仍在时段', planOf(), new Date(2026, 10, 3, 2, 0), true, '本段还可充 5 小时（07:00 结束）');
planCase('周二 07:00 退出时段', planOf(), new Date(2026, 10, 3, 7, 0), false, '今天 23:00 恢复充电（16 小时后）');
planCase('周二 12:00 时段外', planOf(), new Date(2026, 10, 3, 12, 0), false, '今天 23:00 恢复充电（11 小时后）');
planCase('周一 23:00 起始边界', planOf(), new Date(2026, 10, 2, 23, 0), true, '8 小时（07:00 结束）');
planCase('自定义只选周一：周二 02:00 归属周一', planOf({ mode: 'custom', weekdays: [1] }), new Date(2026, 10, 3, 2, 0), true, '5 小时');
planCase('自定义只选周一：周二 23:30 不生效', planOf({ mode: 'custom', weekdays: [1] }), new Date(2026, 10, 3, 23, 30), false, '周一 23:00 恢复充电（5 天 23 小时 30 分后）');
planCase('自定义只选周一：周三 02:00 不生效', planOf({ mode: 'custom', weekdays: [1] }), new Date(2026, 10, 4, 2, 0), false, '周一 23:00 恢复充电（5 天 21 小时后）');
planCase('自定义只选周日：周一 02:00 归属周日', planOf({ mode: 'custom', weekdays: [7] }), new Date(2026, 10, 9, 2, 0), true, '5 小时');
planCase('自定义只选周日：周一 23:30 不生效', planOf({ mode: 'custom', weekdays: [7] }), new Date(2026, 10, 9, 23, 30), false, '周日 23:00 恢复充电（5 天 23 小时 30 分后）');
planCase('同日时段 11:00 在内', planOf({ mode: 'custom', weekdays: [1], windows: [{ start: '10:00', end: '12:00' }] }), new Date(2026, 10, 2, 11, 0), true, '1 小时（12:00 结束）');
planCase('同日时段 12:00 结束边界', planOf({ mode: 'custom', weekdays: [1], windows: [{ start: '10:00', end: '12:00' }] }), new Date(2026, 10, 2, 12, 0), false, '周一 10:00 恢复充电（6 天 22 小时后）');
planCase('预约关闭', planOf({ enabled: false }), new Date(2026, 10, 3, 12, 0), true, '当前不限制充电时段');
planCase('开启但无有效时段', planOf({ windows: [] }), new Date(2026, 10, 3, 12, 0), true, '没有时段时保持正常充电');

/* ---------- 6. 预约时段进入 / 退出时停充与恢复 ---------- */
simulate('预约时段进出', [
  { label: '周二 12:00 时段外', input: input({ now: new Date(2026, 10, 3, 12, 0), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '周二 23:30 进入时段', input: input({ now: new Date(2026, 10, 3, 23, 30), plan: planOf() }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '周三 07:00 退出时段', input: input({ now: new Date(2026, 10, 4, 7, 0), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '周三 12:00 保持停充', input: input({ now: new Date(2026, 10, 4, 12, 0), plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'plan_window' } }
]);

/* ---------- 7. 预约时段与 100% 涓流锁存的优先级 ---------- */
simulate('预约 vs 100% 涓流锁存', [
  { label: '周一 23:30 时段内 99%', input: full({ now: new Date(2026, 10, 2, 23, 30), percent: 99, plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段内到 100%（大电流）', input: full({ now: new Date(2026, 10, 2, 23, 30), percent: 100, plan: planOf() }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '时段内涓流开始', input: trickle({ now: new Date(2026, 10, 2, 23, 30, 10), plan: planOf() }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '时段内涓流满 9s', input: trickle({ now: new Date(2026, 10, 2, 23, 30, 20), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } },
  { label: '跨到周二 12:00 时段外', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, currentMa: 0, plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '时段外拔出', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, plugged: false, plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段外重新插电（100%）', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, currentMa: 1200, plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '等到 23:30 进入时段', input: trickle({ now: new Date(2026, 10, 3, 23, 30), plan: planOf() }), expect: { allow: true, action: 'resume', primary: null, phase: 'trickle' } },
  { label: '恢复后首个充电采样', input: trickle({ now: new Date(2026, 10, 3, 23, 30, 1), plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段内涓流满 9s', input: trickle({ now: new Date(2026, 10, 3, 23, 30, 10), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
]);

/* ---------- 8. 温度停充与回落复充 ---------- */
simulate('温度保护', [
  { label: '33°C 正常', input: input({ percent: 60, tempC: 33 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '达到 45°C', input: input({ percent: 60, tempC: 45 }), expect: { allow: false, action: 'stop', primary: 'temp_high' } },
  { label: '回落到 42°C（未到恢复线）', input: input({ percent: 60, tempC: 42 }), expect: { allow: false, action: 'hold', primary: 'temp_high' } },
  { label: '回落到 40°C', input: input({ percent: 60, tempC: 40 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '继续降到 39°C', input: input({ percent: 60, tempC: 39 }), expect: { allow: true, action: 'none', primary: null } }
]);

/* ---------- 9. 温度与预约 / 上限的优先级 ---------- */
const tempPriorityState = simulate('温度优先级', [
  { label: '周二 12:00 时段外 33°C', input: input({ now: new Date(2026, 10, 3, 12, 0), percent: 70, tempC: 33, plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '时段外 45°C 且 85%', input: input({ now: new Date(2026, 10, 3, 12, 0), percent: 85, tempC: 45, plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'temp_high' } },
  { label: '温度回落后仍 85%', input: input({ now: new Date(2026, 10, 3, 12, 0), percent: 85, tempC: 40, plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 70% 但仍时段外', input: input({ now: new Date(2026, 10, 3, 12, 0), percent: 70, tempC: 40, plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'plan_window' } },
  { label: '等到 23:30 进入时段', input: input({ now: new Date(2026, 10, 3, 23, 30), percent: 70, tempC: 40, plan: planOf() }), expect: { allow: true, action: 'resume', primary: null } }
]);

/* 原因排序：温度 > 上限 > 预约 */
(function () {
  const result = C.step(tempPriorityState, input({
    now: new Date(2026, 10, 3, 12, 0), percent: 85, tempC: 45, plan: planOf()
  }));
  const order = result.reasons.map((reason) => reason.code).join('>');
  check('温度优先级', '45°C + 85% + 时段外的原因顺序', 'temp_high>upper_bound>plan_window', order, order === 'temp_high>upper_bound>plan_window');
})();

/* ---------- 10. 插拔、重启、配置变更后的状态恢复 ---------- */
simulate('插拔 / 重启 / 配置变更', [
  { label: '79% 充电中', input: input({ percent: 79 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '80% 停充', input: input({ percent: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '拔出充电器', input: input({ percent: 80, plugged: false }), expect: { allow: true, action: 'none', primary: null } },
  { label: '重新插电 78%（新会话）', input: input({ percent: 78 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '新会话到达 80%', input: input({ percent: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } }
]);

/* 重启走全新状态机，不能沿用上一次的内部状态。 */
(function () {
  const boots = [
    { label: '重启时 100%（100% 策略）', input: full({ percent: 100 }), expect: { allow: true, action: 'none', primary: null } },
    { label: '重启时 85%（80% 策略）', input: input({ percent: 85 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
    { label: '重启时 46°C', input: input({ percent: 60, tempC: 46 }), expect: { allow: false, action: 'stop', primary: 'temp_high' } },
    { label: '重启时 78%', input: input({ percent: 78 }), expect: { allow: true, action: 'none', primary: null } }
  ];
  boots.forEach((item) => {
    const result = C.boot(item.input);
    record('重启', item.label, describeExpected(item.expect), describeResult(result), result.allowCharge === item.expect.allow && result.action === item.expect.action && (result.primary || '-') === (item.expect.primary || '-'));
  });
})();

simulate('配置变更恢复', [
  { label: '80% 策略下 80% 停充', input: input({ percent: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '上限改为 90%，当前 85%', input: input({ percent: 85, stopAt: 90, resumeAt: 80 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '到达 90%', input: input({ percent: 90, stopAt: 90, resumeAt: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '上限改回 80%，当前 85%', input: input({ percent: 85 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 70%', input: input({ percent: 70 }), expect: { allow: true, action: 'resume', primary: null } }
]);

/* ---------- 12. 100% 涓流专项 ---------- */

/* 12.2 涓流中拔出重插：重新插电后涓流计时清零，必须重新满 9 秒 */
simulate('涓流中拔出重插', [
  { label: '涓流 0s', input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流 5s', input: trickle({ now: tick(5) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流中拔出', input: trickle({ now: tick(6), plugged: false }), expect: { allow: true, action: 'none', primary: null } },
  { label: '重新插电（大电流）', input: full({ percent: 100, now: tick(20), currentMa: 1200 }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重插后涓流 0s', input: trickle({ now: tick(20) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重插后涓流 5s（旧计时已清零）', input: trickle({ now: tick(25) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '重插后涓流满 9s', input: trickle({ now: tick(30) }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
]);

/* 12.3 涓流中温度超限：温度优先停充，回落恢复后涓流计时重新开始 */
simulate('涓流中温度超限', [
  { label: '涓流 0s', input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流 5s 达 45°C', input: trickle({ now: tick(5), tempC: 45 }), expect: { allow: false, action: 'stop', primary: 'temp_high' } },
  { label: '回落到 40°C', input: trickle({ now: tick(8), tempC: 40 }), expect: { allow: true, action: 'resume', primary: null, phase: 'trickle' } },
  { label: '恢复后涓流 0s', input: trickle({ now: tick(8), tempC: 40 }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '恢复后涓流满 9s', input: trickle({ now: tick(17), tempC: 40 }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
]);

/* 12.4 涓流中进入 / 退出预约时段 */
simulate('涓流中预约时段进出', [
  { label: '周一 23:30 时段内涓流 0s', input: trickle({ now: new Date(2026, 10, 2, 23, 30, 0), plan: planOf() }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '时段内涓流 5s', input: trickle({ now: new Date(2026, 10, 2, 23, 30, 5), plan: planOf() }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '周二 07:00 退出时段', input: trickle({ now: new Date(2026, 10, 3, 7, 0, 0), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '时段外保持', input: trickle({ now: new Date(2026, 10, 3, 12, 0, 0), plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'plan_window' } },
  { label: '周二 23:00 重新进入时段', input: trickle({ now: new Date(2026, 10, 3, 23, 0, 0), plan: planOf() }), expect: { allow: true, action: 'resume', primary: null, phase: 'trickle' } },
  { label: '进入后首个充电采样', input: trickle({ now: new Date(2026, 10, 3, 23, 0, 1), plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '进入后涓流满 9s', input: trickle({ now: new Date(2026, 10, 3, 23, 0, 10), plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
]);

/* 12.5 涓流中把上限改成 90% */
simulate('涓流中改上限为 90%', [
  { label: '涓流 0s', input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '涓流 5s', input: trickle({ now: tick(5) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '改为 90% 上限（当前 100%）', input: input({ percent: 100, stopAt: 90, resumeAt: 80 }), expect: { allow: false, action: 'stop', primary: 'upper_bound' } },
  { label: '回落到 85%', input: input({ percent: 85, stopAt: 90, resumeAt: 80 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 80%', input: input({ percent: 80, stopAt: 90, resumeAt: 80 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '再改回 100% 上限（当前 85%）', input: full({ percent: 85 }), expect: { allow: true, action: 'none', primary: null } }
]);

/* 12.6 重启时处于涓流阶段：不沿用旧锁存，需重新满充 */
(function () {
  const cases = [
    { label: '重启时 100% + 大电流', input: full({ percent: 100, currentMa: 1200 }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
    { label: '重启时 100% + 涓流电流', input: trickle({}), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
    { label: '重启时电池直接报 Full', input: full({ percent: 100, statusFull: true, currentMa: 0 }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
  ];
  cases.forEach((item) => {
    const result = C.boot(item.input);
    record('重启时处于涓流阶段', item.label, describeExpected(item.expect), describeResult(result, true), result.allowCharge === item.expect.allow && result.action === item.expect.action && (result.primary || '-') === (item.expect.primary || '-') && result.phase === item.expect.phase);
  });
})();

/* 12.7 电流抖动不误判满充：短暂回升必须重新累计 9 秒 */
simulate('电流抖动不误判满充', [
  { label: '涓流 0s（80mA）', input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '4s 电流回升 1200mA', input: trickle({ currentMa: 1200, now: tick(4) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '8s 回落到 80mA', input: trickle({ now: tick(8) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '12s 再次回升 1200mA', input: trickle({ currentMa: 1200, now: tick(12) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '15s 回落到 80mA', input: trickle({ now: tick(15) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '20s 仍低电流（累计仅 5s）', input: trickle({ now: tick(20) }), expect: { allow: true, action: 'none', primary: null, phase: 'trickle' } },
  { label: '24s 满 9s', input: trickle({ now: tick(24) }), expect: { allow: false, action: 'stop', primary: 'full_latch', phase: 'stopped' } }
]);

/* ---------- 11. 派生规则与计划规范化 ---------- */
check('派生规则', '80% 不保持', 'false', String(C.deriveLatch(80)), C.deriveLatch(80) === false);
check('派生规则', '90% 不保持', 'false', String(C.deriveLatch(90)), C.deriveLatch(90) === false);
check('派生规则', '100% 保持', 'true', String(C.deriveLatch(100)), C.deriveLatch(100) === true);
check('派生规则', '字符串 100% 保持', 'true', String(C.deriveLatch('100')), C.deriveLatch('100') === true);

(function () {
  const plan = C.normalizePlan({
    enabled: true,
    mode: 'custom',
    weekdays: [3, 1, 1],
    windows: [
      { start: '10:00', end: '10:00' },
      { start: '23:00', end: '07:00' },
      { start: '25:00', end: '07:00' }
    ]
  });
  const pass = plan.weekdays.join(',') === '1,3' && plan.windows.length === 1 && C.isValidWindow(plan.windows[0]);
  check('计划规范化', '去重星期、丢弃无效时段', 'weekdays=1,3 windows=1', 'weekdays=' + plan.weekdays.join(',') + ' windows=' + plan.windows.length, pass);
})();

/* ---------- 配置字段契约：界面 → 模型 → TOML ---------- */
(function () {
  const assert = require('node:assert/strict');
  const fs = require('node:fs');
  const path = require('node:path');
  const vm = require('node:vm');
  const sandbox = { window: {} };
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/toml.js'), 'utf8'), sandbox);
  const T = sandbox.window.Tempo.Toml;
  const app = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
  const groups = app.slice(app.indexOf('  var LOG_LEVELS ='), app.indexOf('  function escapeHtml'));
  const validation = app.slice(app.indexOf('  function validateConfigModel('), app.indexOf('  function firstErrorText('));
  Object.assign(sandbox, { Tempo: { Toml: T }, cfgRangeText: () => 'range' });
  vm.runInContext(groups + validation, sandbox);
  const defaults = T.defaults();
  const model = T.normalize({});
  assert.equal(Object.keys(sandbox.validateConfigModel(model)).length, 0);
  sandbox.CFG_FIELDS.forEach((def) => {
    const value = T.readPath(model, def.path);
    assert.notEqual(value, undefined, def.path + ' must survive normalization');
    assert.equal(value, T.readPath(defaults, def.path));
    if (def.kind !== 'select') {
      assert.ok(Number.isFinite(value) && value >= def.min && value <= def.max, def.path);
      const edited = T.clone(model);
      T.writePath(edited, def.path, def.max);
      const doc = T.applyModel({ control: { startup_guard_ms: 23000 } }, edited, sandbox.CFG_WRITE_PATHS);
      const parsed = T.parse(T.stringify(doc));
      assert.equal(T.readPath(T.normalize(parsed), def.path), def.max, def.path + ' roundtrip');
      assert.equal(parsed.control.startup_guard_ms, 23000, 'preserve hidden parameters');
      T.writePath(edited, def.path, def.max + 1);
      assert.ok(sandbox.validateConfigModel(edited)[def.path], def.path + ' upper bound');
      T.writePath(edited, def.path, def.min - 1);
      assert.ok(sandbox.validateConfigModel(edited)[def.path], def.path + ' lower bound');
      if (def.integer) {
        T.writePath(edited, def.path, def.min + 0.5);
        assert.ok(sandbox.validateConfigModel(edited)[def.path], def.path + ' integer');
      }
    }
  });
  const contracts = {
    demand_smooth_ms: [200, 50, 1000], floor_exit_ratio: [0.12, 0.02, 0.40],
    down_step_ms: [45, 15, 200], touch_lift_tiers: [1, 1, 3],
    launch_tier_ratio: [0.62, 0.30, 0.90], tier_table_max: [8, 6, 10],
    up_confirm_ms: [120, 40, 400], max_up_step: [2, 1, 4],
    prime_cap_ratio: [0.70, 0.30, 0.90], ime_cap_ratio: [0.45, 0.25, 0.80]
  };
  const rust = fs.readFileSync(path.join(__dirname, '../../crates/tempo-core/src/config.rs'), 'utf8');
  const rustDefaults = rust.slice(rust.indexOf('impl Default for ControlConfig'), rust.indexOf('impl Default for SceneConfig'));
  Object.entries(contracts).forEach(([name, [value, min, max]]) => {
    assert.equal(defaults.control[name], value, name + ' default');
    assert.equal(Number(rustDefaults.match(new RegExp('\\b' + name + ': ([0-9._]+)'))[1].replace(/_/g, '')), value, name + ' Rust default');
    const def = sandbox.CFG_FIELD_MAP['control.' + name];
    assert.equal(def.min, min, name + ' min');
    assert.equal(def.max, max, name + ' max');
    const explicit = T.normalize({ control: { [name]: max } });
    assert.equal(explicit.control[name], max, name + ' explicit value');
  });
  const quantum = sandbox.CFG_FIELD_MAP['events.runtime_quantum_us'];
  assert.equal(quantum.min, 500);
  assert.equal(quantum.max, 300000);
  [500, 50000, 300000].forEach((value) => {
    const edited = T.clone(model);
    edited.events.runtime_quantum_us = value;
    assert.equal(sandbox.validateConfigModel(edited)['events.runtime_quantum_us'], undefined);
  });
  [model.control.floor_enter_ratio, model.control.floor_enter_ratio - 0.01].forEach((value) => {
    const edited = T.clone(model);
    edited.control.floor_exit_ratio = value;
    assert.ok(sandbox.validateConfigModel(edited)['control.floor_exit_ratio']);
  });
  check('配置契约', '字段默认值、边界、回写及交叉校验', '全部通过', '全部通过', true);
})();

for (const missingPercent of [false, true]) {
  simulate('传感器缺失重置证据 ' + missingPercent, [
    { input: trickle({ now: tick(0) }), expect: { allow: true, action: 'none' } },
    { input: trickle({ now: tick(5), percent: missingPercent ? null : 100, currentMa: null }), expect: { allow: true, action: 'none' } },
    { input: trickle({ now: tick(10) }), expect: { allow: true, action: 'none' } },
    { input: trickle({ now: tick(19) }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
  ]);
}
simulate('高电压不能独立满充', [
  { input: trickle({ now: tick(0), currentMa: null, fullVoltageMv: 4300, voltageMv: 4400 }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(20), currentMa: 500, fullVoltageMv: 4300, voltageMv: 4400 }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(30), fullVoltageMv: 4300, voltageMv: 4400 }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(39), fullVoltageMv: 4300, voltageMv: 4400 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
]);
simulate('实际开关停充零电流无效', [
  { input: trickle({ now: tick(0), currentMa: 0, chargingSuspended: true }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(20), currentMa: 0, chargingSuspended: true }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(30) }), expect: { allow: true, action: 'none' } },
  { input: trickle({ now: tick(39) }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
]);

/* ---------- 输出 ---------- */
const columns = [
  { title: '场景', width: 24 },
  { title: '步骤', width: 30 },
  { title: '预期', width: 34 },
  { title: '实际', width: 34 },
  { title: '结果', width: 6 }
];

const line = columns.map((column) => padEnd('', column.width)).join(' | ');
const divider = columns.map((column) => '-'.repeat(column.width)).join('-+-');

console.log(line);
console.log(divider);
rows.forEach((row) => {
  console.log([
    padEnd(row.scenario, columns[0].width),
    padEnd(row.step, columns[1].width),
    padEnd(row.expected, columns[2].width),
    padEnd(row.actual, columns[3].width),
    padEnd(row.pass ? 'PASS' : 'FAIL', columns[4].width)
  ].join(' | '));
});

console.log(divider);
console.log('总计 ' + rows.length + ' 项：通过 ' + passed + '，失败 ' + failed + '。');

/* UI 行为回归：在无浏览器沙盒内驱动真实页面函数和异步桥。 */
async function uiRegression() {
  const assert = require('node:assert/strict');
  const fs = require('node:fs');
  const vm = require('node:vm');
  const path = require('node:path');
  const timers = new Map();
  let sequence = 0;
  let listWrites = 0;
  const list = { set innerHTML(value) { listWrites += 1; this.html = value; } };
  const row = { dataset: { pkg: 'test.app' }, classList: { toggle() {} }, setAttribute(key, value) { this[key] = value; } };
  const fields = {};
  const card = { querySelector(selector) {
    return fields[selector] || (fields[selector] = { style: { setProperty(key, value) { this[key] = value; } } });
  } };
  const document = {
    readyState: 'loading', hidden: false, addEventListener() {},
    getElementById(id) { return id === 'fas-list' ? list : null; },
    querySelector() { return null; },
    querySelectorAll(selector) { return selector === '[data-pkg]' ? [row] : selector === '.cluster' ? [card] : []; }
  };
  const window = {
    Tempo: { Charge: C }, JSON, location: { search: '' },
    setTimeout(fn, delay) { const id = ++sequence; timers.set(id, { fn, delay }); return id; },
    clearTimeout(id) { timers.delete(id); }, addEventListener() {}
  };
  const context = vm.createContext({ window, document, console, Promise, Date });
  let source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
  source = source.replace('  if (document.readyState', '  window.testUi = { state: state, rows: renderAppRows, toggle: toggleApp, input: onInput, start: startOverviewPolling, stop: stopOverviewPolling, visible: onVisibilityChange, loadFas: ensureFasLoaded, setApi: function(value) { api = value; } };\n  if (document.readyState');
  vm.runInContext(source, context);
  const ui = window.testUi;
  ui.state.tab = 'fas';
  ui.state.fasLoaded = true;
  ui.state.fas.enabled = true;
  ui.state.fasApps = Array.from({ length: 1001 }, (_, i) => ({ package: 'test.app' + i }));
  assert.equal((ui.rows().match(/data-pkg=/g) || []).length, 60);
  ui.state.fasQuery = 'test.app1000';
  assert.match(ui.rows(), /data-pkg="test.app1000"/);
  ui.setApi({ saveFas: () => Promise.resolve({ error: null }) });
  ui.toggle('test.app');
  assert.equal(row['aria-pressed'], 'true');
  assert.equal(listWrites, 0, '勾选不能重建应用列表');
  ui.input({ target: { id: 'fas-search', value: 'first' } });
  ui.input({ target: { id: 'fas-search', value: 'test.app1000' } });
  assert.equal([...timers.values()].filter(t => t.delay === 180).length, 1);
  const debounce = [...timers.values()].find(t => t.delay === 180);
  debounce.fn();
  assert.equal(listWrites, 1);
  timers.clear();
  ui.state.tab = 'overview';
  ui.state.data = { status: { clusters: [{ min_khz: 300000, max_khz: 2000000 }] } };
  let reads = 0;
  let complete;
  ui.setApi({ queryFrequencies() { reads += 1; return new Promise(resolve => { complete = resolve; }); }, queryUsage: () => Promise.resolve({ values: [], windowMs: 500 }) });
  ui.start();
  const poll = [...timers.values()].find(t => t.delay === 5000);
  timers.clear();
  poll.fn();
  ui.start();
  const busyPoll = [...timers.values()].find(t => t.delay === 5000);
  timers.clear();
  busyPoll.fn();
  assert.equal(reads, 1, '总览采样必须单飞');
  document.hidden = true;
  ui.start();
  assert.equal(timers.size, 0, '隐藏时必须停止总览计时器');
  complete([]);
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  assert.equal(timers.size, 0, '后台采样完成后不能重新启动轮询');
  document.hidden = false;
  window.__AEGIS_FOREGROUND__ = false;
  ui.start();
  assert.equal(timers.size, 0, '原生后台状态同样停止轮询');

  window.__AEGIS_FOREGROUND__ = true;
  ui.setApi({ queryFrequencies: () => Promise.resolve([1200000]), queryUsage: () => Promise.resolve({ values: [0.42], windowMs: 500 }) });
  ui.start();
  const livePoll = [...timers.values()].find(t => t.delay === 5000);
  timers.clear();
  livePoll.fn();
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  assert.equal(fields['.ring__inner b'].textContent, '1.20');
  assert.equal(fields['.bar--usage em'].textContent, '42%');
  assert.equal(listWrites, 1, '实时采样不能重建页面');
  ui.stop();

  vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/shell.js'), 'utf8'), context);
  let requested;
  const shell = new window.Tempo.Shell.NativeShell({ apps(id, force) { requested = { id, force }; } });
  const pending = shell.apps(true);
  assert.equal(requested.force, true);
  window.AegisShellResolve(requested.id, 0, JSON.stringify({ apps: [{ package: 'test.app' }] }), '');
  assert.equal((await pending).apps[0].package, 'test.app');
  assert.equal(Object.keys(shell.pending).length, 0);
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/api.js'), 'utf8'), context);
  const api = new window.Tempo.Api({ exec: () => Promise.resolve({ code: 0, stdout: Array.from({ length: 1001 }, (_, i) => '__USER__ test.app' + i).join('\n') }) });
  assert.equal((await api.queryPackages()).apps.length, 1001, 'WebUI不能截断800个应用');
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../src/mock.js'), 'utf8'), context);
  window.location.search = '?demo=1';
  const demoApi = new window.Tempo.Api(window.Tempo.Shell.create());
  timers.clear();
  const demoFrequencies = demoApi.queryFrequencies(window.Tempo.Mock.status.clusters);
  [...timers.values()].forEach(timer => timer.fn());
  timers.clear();
  assert.deepEqual(Array.from(await demoFrequencies), [307200, 499200, 595200], 'demo真实policy 0/3/7必须全部匹配');
  const mappedApi = new window.Tempo.Api({ exec: () => Promise.resolve({ code: 0, stdout: '7 595200\n0 307200\n3 499200\n' }) });
  assert.deepEqual(Array.from(await mappedApi.queryFrequencies([{ policy: 'policy7' }, { cpus: [0, 1, 2] }, { policy: 3 }, { cpus: [9] }])), [595200, 307200, 499200, null], '按policy而非位置匹配，缺失保留null');
  const oldFixtureApi = new window.Tempo.Api({ exec: () => Promise.resolve({ code: 0, stdout: '0 307200\n1 499200\n2 595200\n' }) });
  assert.deepEqual(Array.from(await oldFixtureApi.queryFrequencies(window.Tempo.Mock.status.clusters)), [307200, null, null], '不允许使用簇序号fallback掩盖不匹配');

  ui.state.tab = 'overview';
  ui.state.fasDirty = false;
  ui.state.fasSaving = false;
  let fasReads = 0;
  let returnFas;
  ui.setApi({ loadFas() { fasReads += 1; return new Promise(resolve => { returnFas = resolve; }); }, queryPackages() { throw new Error('恢复可见不应扫描应用'); } });
  ui.visible();
  assert.equal(fasReads, 1, '已缓存FAS恢复前台仍需重读');
  returnFas({ enabled: false, packages: ['external.app'] });
  await Promise.resolve();
  assert.equal(ui.state.fas.packages[0], 'external.app');
  ui.state.fasDirty = true;
  ui.visible();
  assert.equal(fasReads, 1, '未保存的编辑不能被恢复读取覆盖');
  ui.state.fasDirty = false;
  ui.state.fasSaving = true;
  ui.visible();
  assert.equal(fasReads, 1, '正在保存时不能重读');
  ui.state.fasSaving = false;
  ui.visible();
  assert.equal(fasReads, 2);
  ui.state.fasEditRevision += 1;
  ui.state.fas.packages = ['local.edit'];
  returnFas({ enabled: true, packages: ['stale.app'] });
  await Promise.resolve();
  assert.equal(ui.state.fas.packages[0], 'local.edit', '读取飞行期间的新编辑必须保留');
  ui.stop();
  console.log('UI 行为回归：既有UI行为、demo与真实policy映射、缺失频率保留null、FAS恢复重读及编辑竞态保护均通过。');
}
uiRegression().catch(error => { console.error(error); process.exitCode = 1; });

if (failed > 0) {
  console.error('\n存在未通过用例，不能接入页面。');
  process.exit(1);
}
