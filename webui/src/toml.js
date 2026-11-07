/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * toml.js —— config.toml 的极简读写。
 *
 * 只覆盖本项目配置使用的 TOML 子集：表头、键值、字符串、数字、布尔、字符串数组。
 * 不引入第三方解析器，保证 WebUI 在 file:// 下零依赖运行。
 * 输出始终是完整配置（所有分组都写出），避免默认值歧义。
 */
(function (global) {
  'use strict';

  var Tempo = (global.Tempo = global.Tempo || {});

  var DEFAULTS = {
    schema: 1,
    daemon: {
      log_level: 'warn',
      profile_path: '/data/adb/tempo/device.toml'
    },
    events: {
      runtime_quantum_us: 50000,
      demand_step: 0.10,
      screen_path: '',
      frame_stride: 2,
      quiet_decay_ms: 60
    },
    control: {
      quiet_ratio: 0.50,
      quiet_cap: 0.28,
      floor_enter_ratio: 0.03,
      floor_settle_ms: 60,
      floor_residence_ms: 0,
      drop_ratio: 0.08,
      drop_factor: 4.0,
      touch_lift_ms: 150,
      touch_lift_level: 0.85,
      frame_slack_ms: 3.0,
      frame_burst_ms: 150,
      frame_burst_level: 0.95,
      reserve_ratio: 1.05,
      step_hysteresis: 0.03,
      role_cap: {
        efficiency: 1.00,
        balanced: 1.00,
        prime: 0.85
      }
    },
    charge: {
      stop_at_percent: 80,
      resume_at_percent: 70,
      temp_stop_c: 45.0,
      temp_resume_c: 40.0,
      latch_per_plug: false
    },
    thermal: {
      guard_c: 45.0,
      profile: 'auto'
    },
    ui: {
      language: 'zh'
    }
  };

  function clone(value) {
    return JSON.parse(JSON.stringify(value));
  }

  function text(value) {
    return String(value == null ? '' : value);
  }

  function isObject(value) {
    return !!value && typeof value === 'object' && !Array.isArray(value);
  }

  function readPath(root, path) {
    var node = root;
    var parts = path.split('.');
    for (var i = 0; i < parts.length; i += 1) {
      if (!isObject(node)) return undefined;
      node = node[parts[i]];
    }
    return node;
  }

  function writePath(root, path, value) {
    var node = root;
    var parts = path.split('.');
    for (var i = 0; i < parts.length - 1; i += 1) {
      if (!isObject(node[parts[i]])) node[parts[i]] = {};
      node = node[parts[i]];
    }
    node[parts[parts.length - 1]] = value;
  }

  /* ---------- 解析 ---------- */

  function stripComment(line) {
    var quote = null;
    var escaped = false;
    for (var i = 0; i < line.length; i += 1) {
      var ch = line.charAt(i);
      if (escaped) {
        escaped = false;
        continue;
      }
      if (ch === '\\' && quote === '"') {
        escaped = true;
        continue;
      }
      if (quote) {
        if (ch === quote) quote = null;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        continue;
      }
      if (ch === '#') return line.slice(0, i);
    }
    return line;
  }

  function splitTopLevel(body) {
    var items = [];
    var depth = 0;
    var quote = null;
    var escaped = false;
    var current = '';
    for (var i = 0; i < body.length; i += 1) {
      var ch = body.charAt(i);
      if (escaped) {
        current += ch;
        escaped = false;
        continue;
      }
      if (quote) {
        current += ch;
        if (ch === '\\' && quote === '"') escaped = true;
        else if (ch === quote) quote = null;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        current += ch;
        continue;
      }
      if (ch === '[') depth += 1;
      if (ch === ']') depth -= 1;
      if (ch === ',' && depth === 0) {
        items.push(current.trim());
        current = '';
        continue;
      }
      current += ch;
    }
    if (current.trim()) items.push(current.trim());
    return items;
  }

  function unquote(raw, lineNo) {
    var quote = raw.charAt(0);
    var inner = raw.slice(1, raw.length - 1);
    if (quote === "'") return inner;
    return inner.replace(/\\(u[0-9a-fA-F]{4}|.)/g, function (match, code) {
      if (code === 'n') return '\n';
      if (code === 't') return '\t';
      if (code === 'r') return '\r';
      if (code.charAt(0) === 'u') return '';
      return code;
    });
  }

  function parseValue(raw, lineNo) {
    var value = raw.trim();
    if (!value) throw new Error('第 ' + lineNo + ' 行：缺少值');

    if (value.charAt(0) === '"' || value.charAt(0) === "'") {
      if (value.charAt(value.length - 1) !== value.charAt(0)) {
        throw new Error('第 ' + lineNo + ' 行：字符串未闭合');
      }
      return unquote(value, lineNo);
    }
    if (value.charAt(0) === '[') {
      if (value.charAt(value.length - 1) !== ']') {
        throw new Error('第 ' + lineNo + ' 行：数组未闭合');
      }
      return splitTopLevel(value.slice(1, -1)).map(function (item) {
        return parseValue(item, lineNo);
      });
    }
    if (value === 'true') return true;
    if (value === 'false') return false;

    var number = Number(value.replace(/_/g, ''));
    if (isFinite(number)) return number;
    throw new Error('第 ' + lineNo + ' 行：无法识别的值 "' + value + '"');
  }

  function parse(source) {
    var root = {};
    var table = root;
    var lines = text(source).split(/\r?\n/);

    lines.forEach(function (rawLine, index) {
      var lineNo = index + 1;
      var line = stripComment(rawLine).trim();
      if (!line) return;

      if (line.charAt(0) === '[') {
        if (line.charAt(line.length - 1) !== ']') {
          throw new Error('第 ' + lineNo + ' 行：表头未闭合');
        }
        var name = line.slice(1, -1).trim();
        if (!name) throw new Error('第 ' + lineNo + ' 行：表头为空');
        var parts = name.split('.').map(function (part) { return part.trim(); });
        table = root;
        parts.forEach(function (part) {
          if (!isObject(table[part])) table[part] = {};
          table = table[part];
        });
        return;
      }

      var eq = -1;
      var quote = null;
      for (var i = 0; i < line.length; i += 1) {
        var ch = line.charAt(i);
        if (quote) {
          if (ch === quote) quote = null;
          continue;
        }
        if (ch === '"' || ch === "'") {
          quote = ch;
          continue;
        }
        if (ch === '=') {
          eq = i;
          break;
        }
      }
      if (eq <= 0) throw new Error('第 ' + lineNo + ' 行：缺少 "="');
      var key = line.slice(0, eq).trim().replace(/^["']|["']$/g, '');
      table[key] = parseValue(line.slice(eq + 1), lineNo);
    });

    return root;
  }

  /* ---------- 规范化为内存模型 ---------- */

  function pickNumber(source, path, fallback) {
    var value = readPath(source, path);
    return isFinite(Number(value)) && value !== '' && value !== null ? Number(value) : fallback;
  }

  function pickString(source, path, fallback) {
    var value = readPath(source, path);
    return typeof value === 'string' && value.length ? value : fallback;
  }

  function pickStringAllowEmpty(source, path, fallback) {
    var value = readPath(source, path);
    return typeof value === 'string' ? value : fallback;
  }

  function pickBool(source, path, fallback) {
    var value = readPath(source, path);
    return typeof value === 'boolean' ? value : fallback;
  }

  function normalize(source) {
    var src = isObject(source) ? source : {};
    var d = DEFAULTS;
    var model = {
      schema: 1,
      daemon: {
        log_level: pickString(src, 'daemon.log_level', d.daemon.log_level),
        profile_path: pickString(src, 'daemon.profile_path', d.daemon.profile_path)
      },
      events: {
        runtime_quantum_us: pickNumber(src, 'events.runtime_quantum_us', d.events.runtime_quantum_us),
        demand_step: pickNumber(src, 'events.demand_step', d.events.demand_step),
        screen_path: pickStringAllowEmpty(src, 'events.screen_path', d.events.screen_path),
        frame_stride: pickNumber(src, 'events.frame_stride', d.events.frame_stride),
        quiet_decay_ms: pickNumber(src, 'events.quiet_decay_ms', d.events.quiet_decay_ms)
      },
      control: {
        quiet_ratio: pickNumber(src, 'control.quiet_ratio', d.control.quiet_ratio),
        quiet_cap: pickNumber(src, 'control.quiet_cap', d.control.quiet_cap),
        floor_enter_ratio: pickNumber(src, 'control.floor_enter_ratio', d.control.floor_enter_ratio),
        floor_settle_ms: pickNumber(src, 'control.floor_settle_ms', d.control.floor_settle_ms),
        floor_residence_ms: pickNumber(src, 'control.floor_residence_ms', d.control.floor_residence_ms),
        drop_ratio: pickNumber(src, 'control.drop_ratio', d.control.drop_ratio),
        drop_factor: pickNumber(src, 'control.drop_factor', d.control.drop_factor),
        touch_lift_ms: pickNumber(src, 'control.touch_lift_ms', d.control.touch_lift_ms),
        touch_lift_level: pickNumber(src, 'control.touch_lift_level', d.control.touch_lift_level),
        frame_slack_ms: pickNumber(src, 'control.frame_slack_ms', d.control.frame_slack_ms),
        frame_burst_ms: pickNumber(src, 'control.frame_burst_ms', d.control.frame_burst_ms),
        frame_burst_level: pickNumber(src, 'control.frame_burst_level', d.control.frame_burst_level),
        reserve_ratio: pickNumber(src, 'control.reserve_ratio', d.control.reserve_ratio),
        step_hysteresis: pickNumber(src, 'control.step_hysteresis', d.control.step_hysteresis),
        role_cap: {
          efficiency: pickNumber(src, 'control.role_cap.efficiency', d.control.role_cap.efficiency),
          balanced: pickNumber(src, 'control.role_cap.balanced', d.control.role_cap.balanced),
          prime: pickNumber(src, 'control.role_cap.prime', d.control.role_cap.prime)
        }
      },
      charge: {
        stop_at_percent: pickNumber(src, 'charge.stop_at_percent', d.charge.stop_at_percent),
        resume_at_percent: pickNumber(src, 'charge.resume_at_percent', d.charge.resume_at_percent),
        temp_stop_c: pickNumber(src, 'charge.temp_stop_c', d.charge.temp_stop_c),
        temp_resume_c: pickNumber(src, 'charge.temp_resume_c', d.charge.temp_resume_c),
        latch_per_plug: pickBool(src, 'charge.latch_per_plug', d.charge.latch_per_plug)
      },
      thermal: {
        guard_c: pickNumber(src, 'thermal.guard_c', d.thermal.guard_c),
        profile: pickString(src, 'thermal.profile', d.thermal.profile)
      },
      ui: {
        language: pickString(src, 'ui.language', d.ui.language)
      }
    };
    return model;
  }

  /* ---------- 生成 ---------- */

  function intText(value) {
    return String(Math.round(Number(value)));
  }

  function floatText(value) {
    var number = Number(value);
    var out = number.toFixed(4).replace(/0+$/, '');
    if (out.charAt(out.length - 1) === '.') out += '0';
    return out;
  }

  function quote(value) {
    return '"' + text(value)
      .replace(/\\/g, '\\\\')
      .replace(/"/g, '\\"')
      .replace(/\n/g, '\\n')
      .replace(/\r/g, '\\r')
      .replace(/\t/g, '\\t') + '"';
  }

  function stringify(model) {
    var m = normalize(model);
    var lines = [
      '# Aegis 调度配置（控制台生成，schema = 1）',
      'schema = ' + intText(m.schema),
      '',
      '[daemon]',
      'log_level = ' + quote(m.daemon.log_level),
      'profile_path = ' + quote(m.daemon.profile_path),
      '',
      '[events]',
      'runtime_quantum_us = ' + intText(m.events.runtime_quantum_us),
      'demand_step = ' + floatText(m.events.demand_step),
      'screen_path = ' + quote(m.events.screen_path),
      'frame_stride = ' + intText(m.events.frame_stride),
      'quiet_decay_ms = ' + intText(m.events.quiet_decay_ms),
      '',
      '[control]',
      'quiet_ratio = ' + floatText(m.control.quiet_ratio),
      'quiet_cap = ' + floatText(m.control.quiet_cap),
      'floor_enter_ratio = ' + floatText(m.control.floor_enter_ratio),
      'floor_settle_ms = ' + intText(m.control.floor_settle_ms),
      'floor_residence_ms = ' + intText(m.control.floor_residence_ms),
      'drop_ratio = ' + floatText(m.control.drop_ratio),
      'drop_factor = ' + floatText(m.control.drop_factor),
      'touch_lift_ms = ' + intText(m.control.touch_lift_ms),
      'touch_lift_level = ' + floatText(m.control.touch_lift_level),
      'frame_slack_ms = ' + floatText(m.control.frame_slack_ms),
      'frame_burst_ms = ' + intText(m.control.frame_burst_ms),
      'frame_burst_level = ' + floatText(m.control.frame_burst_level),
      'reserve_ratio = ' + floatText(m.control.reserve_ratio),
      'step_hysteresis = ' + floatText(m.control.step_hysteresis),
      '',
      '[control.role_cap]',
      'efficiency = ' + floatText(m.control.role_cap.efficiency),
      'balanced = ' + floatText(m.control.role_cap.balanced),
      'prime = ' + floatText(m.control.role_cap.prime),
      '',
      '[charge]',
      'stop_at_percent = ' + intText(m.charge.stop_at_percent),
      'resume_at_percent = ' + intText(m.charge.resume_at_percent),
      'temp_stop_c = ' + floatText(m.charge.temp_stop_c),
      'temp_resume_c = ' + floatText(m.charge.temp_resume_c),
      'latch_per_plug = ' + (m.charge.latch_per_plug ? 'true' : 'false'),
      '',
      '[thermal]',
      'guard_c = ' + floatText(m.thermal.guard_c),
      'profile = ' + quote(m.thermal.profile),
      '',
      '[ui]',
      'language = ' + quote(m.ui.language)
    ];
    return lines.join('\n') + '\n';
  }

  Tempo.Toml = {
    parse: parse,
    stringify: stringify,
    normalize: normalize,
    defaults: function () { return clone(DEFAULTS); },
    clone: clone,
    readPath: readPath,
    writePath: writePath
  };
})(window);
