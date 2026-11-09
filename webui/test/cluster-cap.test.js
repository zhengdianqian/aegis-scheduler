/* SPDX-License-Identifier: GPL-3.0-or-later */
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

function sandbox() {
  const context = vm.createContext({
    window: {},
    document: { readyState: 'loading', addEventListener() {}, querySelector() { return null; }, getElementById() { return null; } }
  });
  vm.runInContext('window = globalThis;', context);
  for (const name of ['toml.js', 'charge-model.js', 'app.js']) {
    let source = fs.readFileSync(path.join(__dirname, '../src', name), 'utf8');
    if (name === 'app.js') {
      source = source.replace('  function render() {', '  function render() { return;');
      source = source.replace('  // 参数/FAS 写入成功后调用', `
      global.testConfig = { state: state, fields: clusterCapFields, value: cfgFieldValue,
        validate: validateConfigModel, render: renderClusterCaps, input: onCfgFieldInput,
        reset: resetCfgGroup, sync: syncConfigInputs, paths: CFG_WRITE_PATHS };
      // 参数/FAS 写入成功后调用`);
    }
    vm.runInContext(source, context, { filename: name });
  }
  const T = context.window.Tempo.Toml;
  const app = context.window.testConfig;
  app.state.cfg.loaded = true;
  app.state.cfg.draft = T.defaults();
  app.state.cfg.original = T.defaults();
  return { T, app };
}

function plain(value) { return JSON.parse(JSON.stringify(value)); }
function clusters(cpus) {
  return cpus.map((cpu) => ({ cpus: [cpu, cpu + 1], role: 'balanced', min_khz: 300000, max_khz: 2400000 }));
}

test('缺省为空且渲染不限不主动添加限制', () => {
  const { T, app } = sandbox();
  app.state.data = { status: { clusters: clusters([0, 3]) } };
  assert.deepEqual(plain(T.normalize({}).control.cluster_cap_ratio), {});
  const before = JSON.stringify(app.state.cfg.draft);
  const html = app.render({});
  assert.match(html, /选配/);
  assert.match(html, /保留系统温控，不锁最高频/);
  assert.match(html, /50%.*向下对齐 OPP.*夹到最低频率/);
  assert.match(html, /0.30–2.40 GHz/);
  assert.equal((html.match(/value="1"/g) || []).length, 4);
  assert.equal(JSON.stringify(app.state.cfg.draft), before);
});

test('按实际簇首 CPU 动态生成 2、4、任意簇，不按 role 或簇索引', () => {
  const { app } = sandbox();
  for (const cpus of [[3, 0], [0, 2, 5, 9], [0, 2, 4, 6, 8, 10]]) {
    app.state.data = { status: { clusters: clusters(cpus) } };
    assert.deepEqual(plain(app.fields().map((def) => def.path)), cpus.map((cpu) => 'control.cluster_cap_ratio.cpu' + cpu));
  }
  app.state.data = null;
  assert.equal(app.fields().length, 0);
  assert.match(app.render({}), /已有配置保留不变/);
});

test('TOML 模型与白名单往返保留隐藏字段、非显示簇和充电配置', () => {
  const { T, app } = sandbox();
  const doc = T.parse(`schema = 1
[control]
startup_guard_ms = 23456
[control.cluster_cap_ratio]
cpu0 = 0.5
cpu99 = 0.375
[control.role_cap]
prime = 0.8
[scene]
custom = [1, 2]
[charge]
full_current_ma = 123
full_hold_seconds = 15
full_voltage_mv = 4300
[fas]
enabled = true
apps = ["example.app"]
`);
  const model = T.normalize(doc);
  model.control.cluster_cap_ratio.cpu0 = 0.625;
  assert.equal(doc.control.cluster_cap_ratio.cpu0, 0.5);
  assert.ok(app.paths.includes('control.cluster_cap_ratio'));
  const saved = T.parse(T.stringify(T.applyModel(doc, model, app.paths)));
  assert.equal(saved.control.cluster_cap_ratio.cpu0, 0.625);
  assert.equal(saved.control.cluster_cap_ratio.cpu99, 0.375);
  assert.equal(saved.control.startup_guard_ms, 23456);
  assert.deepEqual(plain(saved.control.role_cap), plain(doc.control.role_cap));
  assert.deepEqual(plain(saved.scene), plain(doc.scene));
  assert.deepEqual(plain(saved.fas), plain(doc.fas));
  for (const key of ['full_current_ma', 'full_hold_seconds', 'full_voltage_mv']) assert.equal(saved.charge[key], doc.charge[key]);
  assert.deepEqual(plain(T.parse(T.stringify({ control: { cluster_cap_ratio: {} } }))), { control: { cluster_cap_ratio: {} } });
});

test('动态和未显示的键均校验数字、有限值及 (0,1]，不静默改正非法配置', () => {
  const { T, app } = sandbox();
  app.state.data = { status: { clusters: clusters([0, 3]) } };
  for (const value of [0.0001, 0.5, 1]) {
    const model = T.defaults();
    model.control.cluster_cap_ratio = { cpu0: value, cpu99: value };
    assert.deepEqual(plain(app.validate(model)), {});
  }
  for (const value of [0, -0.1, 1.01, NaN, Infinity, '', '0.5', true, null]) {
    const model = T.defaults();
    model.control.cluster_cap_ratio.cpu99 = value;
    assert.ok(app.validate(model)['control.cluster_cap_ratio.cpu99']);
  }
  for (const caps of [null, [], 1, { prime: 0.5 }, { 'cpu0.extra': 0.5 }]) {
    const model = T.normalize({ control: { cluster_cap_ratio: caps } });
    assert.ok(app.validate(model)['control.cluster_cap_ratio']);
  }
});

test('动态输入和恢复不限只影响可见簇，回读保持比例精度', () => {
  const { app } = sandbox();
  app.state.data = { status: { clusters: clusters([3, 7]) } };
  app.state.cfg.draft.control.cluster_cap_ratio = { cpu99: 0.4 };
  const field = { dataset: { field: 'control.cluster_cap_ratio.cpu3' }, value: '0.375', type: 'range' };
  app.input({ closest() { return field; } });
  assert.equal(app.state.cfg.draft.control.cluster_cap_ratio.cpu3, 0.375);
  assert.match(app.render({}), /value="0.375"/);
  field.value = '';
  field.type = 'number';
  app.input({ closest() { return field; } });
  assert.ok(app.validate(app.state.cfg.draft)['control.cluster_cap_ratio.cpu3']);
  app.reset('cluster-caps');
  assert.deepEqual(plain(app.state.cfg.draft.control.cluster_cap_ratio), { cpu99: 0.4 });
  assert.equal(app.value(app.state.cfg.draft, app.fields()[0]), 1);
  app.sync();
});
