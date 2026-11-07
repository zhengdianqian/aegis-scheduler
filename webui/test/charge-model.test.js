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
  return input(Object.assign({ stopAt: 100, resumeAt: 70, latchPerPlug: true }, overrides || {}));
}

function describeResult(result) {
  return (result.allowCharge ? '允许' : '停充') + ' | ' + result.action + ' | ' + (result.primary || '-');
}

function describeExpected(expected) {
  return (expected.allow ? '允许' : '停充') + ' | ' + expected.action + ' | ' + (expected.primary || '-');
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
    const actualText = describeResult(result);
    const pass = result.allowCharge === item.expect.allow &&
      result.action === item.expect.action &&
      (result.primary || '-') === (item.expect.primary || '-');
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

/* ---------- 3. 100% 充满即停：锁存与重新插电 ---------- */
simulate('100% 充满即停 / 重新插电', [
  { label: '99% 充电中', input: full({ percent: 99 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '到达 100%', input: full({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } },
  { label: '保持 100%', input: full({ percent: 100 }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '保持 95%（不回落复充）', input: full({ percent: 95 }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '拔出充电器', input: full({ percent: 95, plugged: false }), expect: { allow: true, action: 'none', primary: null } },
  { label: '重新插电（100%）', input: full({ percent: 100 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '重新插电后 99%', input: full({ percent: 99 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '再次到达 100%', input: full({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } },
  { label: '保持 99%（仍锁存）', input: full({ percent: 99 }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '再次拔出', input: full({ percent: 99, plugged: false }), expect: { allow: true, action: 'none', primary: null } },
  { label: '再次插电（99%）', input: full({ percent: 99 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '再次到达 100%', input: full({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
]);

/* ---------- 4. 上限配置变更 ---------- */
simulate('上限配置变更', [
  { label: '100% 策略下 85%', input: full({ percent: 85 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '100% 策略下充满', input: full({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } },
  { label: '改为 80% 上限', input: input({ percent: 100 }), expect: { allow: false, action: 'hold', primary: 'upper_bound' } },
  { label: '回落到 70%', input: input({ percent: 70 }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '改回 100% 上限', input: full({ percent: 70 }), expect: { allow: true, action: 'none', primary: null } },
  { label: '改回后充满', input: full({ percent: 100 }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
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

/* ---------- 7. 预约时段与 100% 锁存的优先级 ---------- */
simulate('预约 vs 100% 锁存', [
  { label: '周一 23:30 时段内 99%', input: full({ now: new Date(2026, 10, 2, 23, 30), percent: 99, plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段内充满', input: full({ now: new Date(2026, 10, 2, 23, 30), percent: 100, plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'full_latch' } },
  { label: '跨到周二 12:00 时段外', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, plan: planOf() }), expect: { allow: false, action: 'hold', primary: 'full_latch' } },
  { label: '时段外拔出', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, plugged: false, plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段外重新插电', input: full({ now: new Date(2026, 10, 3, 12, 0), percent: 100, plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'plan_window' } },
  { label: '等到 23:30 进入时段', input: full({ now: new Date(2026, 10, 3, 23, 30), percent: 100, plan: planOf() }), expect: { allow: true, action: 'resume', primary: null } },
  { label: '时段内回落到 99%', input: full({ now: new Date(2026, 10, 3, 23, 30), percent: 99, plan: planOf() }), expect: { allow: true, action: 'none', primary: null } },
  { label: '时段内再次充满', input: full({ now: new Date(2026, 10, 3, 23, 30), percent: 100, plan: planOf() }), expect: { allow: false, action: 'stop', primary: 'full_latch' } }
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

if (failed > 0) {
  console.error('\n存在未通过用例，不能接入页面。');
  process.exit(1);
}
