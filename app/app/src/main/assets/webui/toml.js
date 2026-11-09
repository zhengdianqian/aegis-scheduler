/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * toml.js —— config.toml 的完整读写。
 *
 * 支持本项目及守护规范化输出会用到的 TOML 结构：
 *   - 表头 [a.b] 与数组表 [[a.b]]
 *   - 多行数组、尾随逗号、行内注释、嵌套数组
 *   - 数字下划线分隔、十六/八/二进制、inf/nan
 *   - 基本字符串转义（\n \t \r \b \f \" \\ \uXXXX \UXXXXXXXX）与字面量字符串
 *   - 点分键与内联表
 *
 * 不引入第三方解析器，保证 WebUI 在 file:// 下零依赖运行。
 * 输出按文档原有分组顺序重排，保留所有未在界面暴露的字段，避免回写时丢配置。
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
      demand_step: 0.08,
      screen_path: '',
      frame_stride: 2,
      quiet_decay_ms: 60
    },
    control: {
      quiet_ratio: 0.50,
      quiet_cap: 0.28,
      floor_enter_ratio: 0.05,
      floor_exit_ratio: 0.12,
      demand_smooth_ms: 200,
      down_step_ms: 45,
      touch_lift_tiers: 1,
      launch_tier_ratio: 0.62,
      tier_table_max: 8,
      up_confirm_ms: 120,
      max_up_step: 2,
      prime_cap_ratio: 0.70,
      ime_cap_ratio: 0.45,
      floor_settle_ms: 60,
      floor_residence_ms: 0,
      drop_ratio: 0.08,
      drop_factor: 2.0,
      touch_lift_ms: 150,
      touch_lift_level: 0.30,
      frame_slack_ms: 3.0,
      frame_burst_ms: 150,
      frame_burst_level: 0.70,
      reserve_ratio: 1.05,
      step_hysteresis: 0.03,
      role_cap: {
        efficiency: 1.00,
        balanced: 1.00,
        prime: 1.00
      }
    },
    charge: {
      stop_at_percent: 80,
      resume_at_percent: 70,
      temp_stop_c: 45.0,
      temp_resume_c: 40.0,
      latch_per_plug: false,
      full_current_ma: 100,
      full_hold_seconds: 9,
      full_voltage_mv: null
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

  /* ---------- 基础扫描 ---------- */

  function skipSpace(source, index) {
    var i = index;
    while (i < source.length) {
      var ch = source.charAt(i);
      if (ch === ' ' || ch === '\t' || ch === '\r' || ch === '\n') {
        i += 1;
        continue;
      }
      if (ch === '#') {
        while (i < source.length && source.charAt(i) !== '\n') i += 1;
        continue;
      }
      break;
    }
    return i;
  }

  /** 从 index 起跳过空白与注释，剩余内容只允许是空白/注释。 */
  function restIsBlank(source, index) {
    return skipSpace(source, index) >= source.length;
  }

  /** 扫描一段文本的引号与括号是否闭合（注释内不计）。 */
  function scanBalance(source) {
    var quote = null;
    var escaped = false;
    var depth = 0;
    for (var i = 0; i < source.length; i += 1) {
      var ch = source.charAt(i);
      if (quote) {
        if (quote === '"' && escaped) {
          escaped = false;
          continue;
        }
        if (quote === '"' && ch === '\\') {
          escaped = true;
          continue;
        }
        if (ch === quote) quote = null;
        continue;
      }
      if (ch === '#') {
        while (i < source.length && source.charAt(i) !== '\n') i += 1;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        continue;
      }
      if (ch === '[' || ch === '{') depth += 1;
      else if (ch === ']' || ch === '}') depth -= 1;
    }
    return { complete: depth === 0 && !quote, depth: depth };
  }

  /** 一行文本去掉行内注释（尊重引号）。仅用于单行表头。 */
  function stripLineComment(line) {
    var quote = null;
    var escaped = false;
    for (var i = 0; i < line.length; i += 1) {
      var ch = line.charAt(i);
      if (quote) {
        if (quote === '"' && escaped) {
          escaped = false;
          continue;
        }
        if (quote === '"' && ch === '\\') {
          escaped = true;
          continue;
        }
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

  /** 在语句里找顶层 "="（跳过引号与注释）。 */
  function findAssign(source) {
    var quote = null;
    var escaped = false;
    for (var i = 0; i < source.length; i += 1) {
      var ch = source.charAt(i);
      if (quote) {
        if (quote === '"' && escaped) {
          escaped = false;
          continue;
        }
        if (quote === '"' && ch === '\\') {
          escaped = true;
          continue;
        }
        if (ch === quote) quote = null;
        continue;
      }
      if (ch === '#') {
        while (i < source.length && source.charAt(i) !== '\n') i += 1;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        continue;
      }
      if (ch === '=') return i;
      if (ch === '\n') return -1;
    }
    return -1;
  }

  /* ---------- 值解析 ---------- */

  function decodeEscape(ch) {
    switch (ch) {
      case 'b': return '\b';
      case 't': return '\t';
      case 'n': return '\n';
      case 'f': return '\f';
      case 'r': return '\r';
      case '"': return '"';
      case '\\': return '\\';
      default: return ch;
    }
  }

  function readBasicString(source, index, lineNo) {
    var out = '';
    var i = index + 1;
    while (i < source.length) {
      var ch = source.charAt(i);
      if (ch === '"') return { value: out, index: i + 1 };
      if (ch === '\\') {
        var next = source.charAt(i + 1);
        if (next === 'u' || next === 'U') {
          var len = next === 'u' ? 4 : 8;
          var hex = source.slice(i + 2, i + 2 + len);
          if (!new RegExp('^[0-9a-fA-F]{' + len + '}$').test(hex)) {
            throw new Error('第 ' + lineNo + ' 行：无效的转义序列');
          }
          out += String.fromCharCode(parseInt(hex, 16));
          i += 2 + len;
          continue;
        }
        out += decodeEscape(next);
        i += 2;
        continue;
      }
      out += ch;
      i += 1;
    }
    throw new Error('第 ' + lineNo + ' 行：字符串未闭合');
  }

  function readLiteralString(source, index, lineNo) {
    var end = source.indexOf("'", index + 1);
    if (end === -1) throw new Error('第 ' + lineNo + ' 行：字符串未闭合');
    return { value: source.slice(index + 1, end), index: end + 1 };
  }

  function parseNumberToken(token, lineNo) {
    var raw = token.replace(/_/g, '');
    var lower = raw.toLowerCase();
    if (lower === 'inf' || lower === '+inf') return Infinity;
    if (lower === '-inf') return -Infinity;
    if (lower === 'nan' || lower === '+nan' || lower === '-nan') return NaN;
    var sign = 1;
    var body = raw;
    if (body.charAt(0) === '+') body = body.slice(1);
    else if (body.charAt(0) === '-') { sign = -1; body = body.slice(1); }
    if (/^0x[0-9a-f]+$/i.test(body)) return sign * parseInt(body.slice(2), 16);
    if (/^0o[0-7]+$/i.test(body)) return sign * parseInt(body.slice(2), 8);
    if (/^0b[01]+$/i.test(body)) return sign * parseInt(body.slice(2), 2);
    if (/^[+-]?(?:\d+(?:\.\d+)?|\.\d+)(?:[eE][+-]?\d+)?$/.test(raw)) return Number(raw);
    throw new Error('第 ' + lineNo + ' 行：无法识别的值 "' + token + '"');
  }

  function readBareToken(source, index) {
    var i = index;
    while (i < source.length) {
      var ch = source.charAt(i);
      if (ch === ',' || ch === ']' || ch === '}' || ch === '#' || ch === '\n' || ch === '\r') break;
      i += 1;
    }
    return { raw: source.slice(index, i).trim(), index: i };
  }

  function readValue(source, index, lineNo) {
    var i = skipSpace(source, index);
    var ch = source.charAt(i);
    if (ch === '"') return readBasicString(source, i, lineNo);
    if (ch === "'") return readLiteralString(source, i, lineNo);
    if (ch === '[') return readArray(source, i, lineNo);
    if (ch === '{') return readInlineTable(source, i, lineNo);
    var token = readBareToken(source, i);
    if (!token.raw) throw new Error('第 ' + lineNo + ' 行：缺少值');
    if (token.raw === 'true') return { value: true, index: token.index };
    if (token.raw === 'false') return { value: false, index: token.index };
    var value = parseNumberToken(token.raw, lineNo);
    return { value: value, index: token.index };
  }

  function readArray(source, index, lineNo) {
    var items = [];
    var i = index + 1;
    while (true) {
      i = skipSpace(source, i);
      if (i >= source.length) throw new Error('第 ' + lineNo + ' 行：数组未闭合');
      if (source.charAt(i) === ']') return { value: items, index: i + 1 };
      var parsed = readValue(source, i, lineNo);
      items.push(parsed.value);
      i = skipSpace(source, parsed.index);
      if (source.charAt(i) === ',') {
        i += 1;
        continue;
      }
      if (source.charAt(i) === ']') return { value: items, index: i + 1 };
      throw new Error('第 ' + lineNo + ' 行：数组缺少逗号或未闭合');
    }
  }

  function readInlineTable(source, index, lineNo) {
    var table = {};
    var i = index + 1;
    while (true) {
      i = skipSpace(source, i);
      if (i >= source.length) throw new Error('第 ' + lineNo + ' 行：内联表未闭合');
      if (source.charAt(i) === '}') return { value: table, index: i + 1 };
      var eq = findAssign(source.slice(i));
      if (eq < 0) throw new Error('第 ' + lineNo + ' 行：内联表缺少 "="');
      var keyParts = parseKeyPath(source.slice(i, i + eq), lineNo);
      var parsed = readValue(source, i + eq + 1, lineNo);
      assignPath(table, keyParts, parsed.value);
      i = skipSpace(source, parsed.index);
      if (source.charAt(i) === ',') {
        i += 1;
        continue;
      }
      if (source.charAt(i) === '}') return { value: table, index: i + 1 };
      throw new Error('第 ' + lineNo + ' 行：内联表缺少逗号或未闭合');
    }
  }

  function parseKeyPath(raw, lineNo) {
    var parts = [];
    var i = 0;
    var source = raw;
    while (i < source.length) {
      while (i < source.length && /[ \t\r\n]/.test(source.charAt(i))) i += 1;
      if (i >= source.length) break;
      var ch = source.charAt(i);
      var part;
      if (ch === '"') {
        var basic = readBasicString(source, i, lineNo);
        part = basic.value;
        i = basic.index;
      } else if (ch === "'") {
        var literal = readLiteralString(source, i, lineNo);
        part = literal.value;
        i = literal.index;
      } else {
        var start = i;
        while (i < source.length && source.charAt(i) !== '.') i += 1;
        part = source.slice(start, i).trim();
      }
      if (!part) throw new Error('第 ' + lineNo + ' 行：键名为空');
      parts.push(part);
      while (i < source.length && /[ \t\r\n]/.test(source.charAt(i))) i += 1;
      if (i < source.length) {
        if (source.charAt(i) !== '.') throw new Error('第 ' + lineNo + ' 行：键名格式无效');
        i += 1;
      }
    }
    return parts;
  }

  function assignPath(base, parts, value) {
    var node = base;
    for (var i = 0; i < parts.length - 1; i += 1) {
      if (!isObject(node[parts[i]])) node[parts[i]] = {};
      node = node[parts[i]];
    }
    node[parts[parts.length - 1]] = value;
  }

  /* ---------- 语句解析 ---------- */

  function looksLikeStatementStart(line) {
    var trimmed = stripLineComment(line).trim();
    if (!trimmed) return false;
    if (trimmed.charAt(0) === '[') return true;
    var eq = findAssign(trimmed);
    if (eq <= 0) return false;
    // 数组续行通常以引号/数字开头且带逗号；这里要求 "=" 前没有逗号。
    return trimmed.slice(0, eq).indexOf(',') === -1;
  }

  function parseInternal(source, options) {
    var body = text(source).replace(/^\uFEFF/, '').replace(/\r\n?/g, '\n');
    var lines = body.split('\n');
    var root = {};
    var current = root;
    var pending = null;
    var errors = options.errors || null;
    var lenient = !!options.lenient;

    function report(error, lineNo) {
      var message = error && error.message ? error.message : String(error);
      if (errors) {
        errors.push({ line: lineNo, message: message });
      }
      if (!lenient) throw error;
    }

    function descend(node, part) {
      var value = node[part];
      if (Array.isArray(value)) value = value[value.length - 1];
      if (!isObject(value)) {
        value = {};
        node[part] = value;
      }
      return value;
    }

    function applyStatement(stmt) {
      var lineNo = stmt.lineNo;
      var trimmed = stmt.text.replace(/^[ \t\r\n]+/, '');
      if (trimmed.charAt(0) === '[') {
        var header = stripLineComment(trimmed).trim();
        var isArray = header.slice(0, 2) === '[[';
        var close = isArray ? ']]' : ']';
        if (header.slice(-close.length) !== close) {
          throw new Error('第 ' + lineNo + ' 行：表头未闭合');
        }
        var inner = header.slice(isArray ? 2 : 1, -close.length).trim();
        if (!inner) throw new Error('第 ' + lineNo + ' 行：表头为空');
        var parts = parseKeyPath(inner, lineNo);
        var node = root;
        for (var i = 0; i < parts.length - 1; i += 1) node = descend(node, parts[i]);
        var last = parts[parts.length - 1];
        if (isArray) {
          if (!Array.isArray(node[last])) node[last] = [];
          var item = {};
          node[last].push(item);
          current = item;
        } else {
          current = descend(node, last);
        }
        return;
      }

      var eq = findAssign(trimmed);
      if (eq <= 0) throw new Error('第 ' + lineNo + ' 行：缺少 "="');
      var keyParts = parseKeyPath(trimmed.slice(0, eq), lineNo);
      var parsed = readValue(trimmed, eq + 1, lineNo);
      if (!restIsBlank(trimmed, parsed.index)) {
        throw new Error('第 ' + lineNo + ' 行：值后存在无法识别的内容');
      }
      assignPath(current, keyParts, parsed.value);
    }

    function flushPending() {
      var stmt = pending;
      pending = null;
      if (!stmt) return;
      try {
        applyStatement(stmt);
      } catch (error) {
        report(error, stmt.lineNo);
      }
    }

    for (var index = 0; index < lines.length; index += 1) {
      var line = lines[index];
      if (pending) {
        pending.text += '\n' + line;
        if (scanBalance(pending.text).complete) {
          flushPending();
        } else if (lenient && looksLikeStatementStart(line)) {
          report(new Error('第 ' + pending.lineNo + ' 行：语句未闭合，已跳过'), pending.lineNo);
          pending = null;
        }
        continue;
      }
      if (!stripLineComment(line).trim()) continue;
      pending = { text: line, lineNo: index + 1 };
      if (scanBalance(line).complete) flushPending();
    }

    if (pending) {
      var reason = scanBalance(pending.text).complete ? null : '语句未闭合';
      report(new Error('第 ' + pending.lineNo + ' 行：' + (reason || '语句解析失败')), pending.lineNo);
      if (!lenient) return root;
      pending = null;
    }

    return root;
  }

  function parse(source) {
    return parseInternal(source, { lenient: false });
  }

  /**
   * 容错解析：逐语句跳过无法解析的片段，返回能读到的部分。
   * 供参数页/充电页降级显示使用，不用于写回。
   */
  function parseLenient(source) {
    var errors = [];
    var doc = parseInternal(source, { lenient: true, errors: errors });
    return { doc: doc, errors: errors };
  }

  /* ---------- 规范化为内存模型 ---------- */

  function pickNumber(source, path, fallback) {
    var value = readPath(source, path);
    if (value === '' || value === null || value === undefined) return fallback;
    return isFinite(Number(value)) ? Number(value) : fallback;
  }

  function pickNumberOrNull(source, path) {
    var value = readPath(source, path);
    if (value === '' || value === null || value === undefined) return null;
    return isFinite(Number(value)) ? Number(value) : null;
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
    return {
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
        floor_exit_ratio: pickNumber(src, 'control.floor_exit_ratio', d.control.floor_exit_ratio),
        demand_smooth_ms: pickNumber(src, 'control.demand_smooth_ms', d.control.demand_smooth_ms),
        down_step_ms: pickNumber(src, 'control.down_step_ms', d.control.down_step_ms),
        touch_lift_tiers: pickNumber(src, 'control.touch_lift_tiers', d.control.touch_lift_tiers),
        launch_tier_ratio: pickNumber(src, 'control.launch_tier_ratio', d.control.launch_tier_ratio),
        tier_table_max: pickNumber(src, 'control.tier_table_max', d.control.tier_table_max),
        up_confirm_ms: pickNumber(src, 'control.up_confirm_ms', d.control.up_confirm_ms),
        max_up_step: pickNumber(src, 'control.max_up_step', d.control.max_up_step),
        prime_cap_ratio: pickNumber(src, 'control.prime_cap_ratio', d.control.prime_cap_ratio),
        ime_cap_ratio: pickNumber(src, 'control.ime_cap_ratio', d.control.ime_cap_ratio),
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
        latch_per_plug: pickBool(src, 'charge.latch_per_plug', d.charge.latch_per_plug),
        full_current_ma: pickNumber(src, 'charge.full_current_ma', d.charge.full_current_ma),
        full_hold_seconds: pickNumber(src, 'charge.full_hold_seconds', d.charge.full_hold_seconds),
        full_voltage_mv: pickNumberOrNull(src, 'charge.full_voltage_mv')
      },
      thermal: {
        guard_c: pickNumber(src, 'thermal.guard_c', d.thermal.guard_c),
        profile: pickString(src, 'thermal.profile', d.thermal.profile)
      },
      ui: {
        language: pickString(src, 'ui.language', d.ui.language)
      }
    };
  }

  /* ---------- 生成 ---------- */

  function isBareKey(key) {
    return /^[A-Za-z0-9_-]+$/.test(key);
  }

  function keyText(key) {
    return isBareKey(key) ? key : quote(key);
  }

  function pathText(parts) {
    return parts.map(keyText).join('.');
  }

  function numberText(value) {
    var number = Number(value);
    if (number !== number) return 'nan';
    if (number === Infinity) return 'inf';
    if (number === -Infinity) return '-inf';
    if (Object.is(number, -0)) return '-0.0';
    return String(number);
  }

  function quote(value) {
    var body = text(value)
      .replace(/\\/g, '\\\\')
      .replace(/"/g, '\\"')
      .replace(/\n/g, '\\n')
      .replace(/\r/g, '\\r')
      .replace(/\t/g, '\\t')
      .replace(/\f/g, '\\f')
      .replace(/[\u0000-\u001f\u007f]/g, function (ch) {
        var code = ch.charCodeAt(0).toString(16);
        while (code.length < 4) code = '0' + code;
        return '\\u' + code;
      });
    return '"' + body + '"';
  }

  function valueText(value) {
    if (typeof value === 'string') return quote(value);
    if (typeof value === 'boolean') return value ? 'true' : 'false';
    if (typeof value === 'number') return numberText(value);
    if (Array.isArray(value)) {
      return '[' + value.map(valueText).join(', ') + ']';
    }
    if (isObject(value)) {
      return '{ ' + Object.keys(value).map(function (key) {
        return keyText(key) + ' = ' + valueText(value[key]);
      }).join(', ') + ' }';
    }
    return '""';
  }

  function isTableArray(value) {
    return Array.isArray(value) && value.length > 0 && value.every(isObject);
  }

  function isSection(value) {
    return isObject(value) || isTableArray(value);
  }

  function emitBody(out, obj, path) {
    Object.keys(obj).forEach(function (key) {
      if (isSection(obj[key])) return;
      out.push(keyText(key) + ' = ' + valueText(obj[key]));
    });
    Object.keys(obj).forEach(function (key) {
      var value = obj[key];
      if (!isObject(value)) return;
      if (out.length) out.push('');
      out.push('[' + pathText(path.concat(key)) + ']');
      emitBody(out, value, path.concat(key));
    });
    Object.keys(obj).forEach(function (key) {
      var value = obj[key];
      if (!isTableArray(value)) return;
      value.forEach(function (item) {
        if (out.length) out.push('');
        out.push('[[' + pathText(path.concat(key)) + ']]');
        emitBody(out, item, path.concat(key));
      });
    });
  }

  /** 序列化整份文档：保留所有分组与数组表，输出合法 TOML。 */
  function stringify(doc) {
    var root = isObject(doc) ? doc : {};
    var out = [];
    emitBody(out, root, []);
    return out.join('\n') + '\n';
  }

  /**
   * 把界面模型里的字段合并回原始文档，未在界面暴露的字段原样保留。
   * paths 为点分路径数组；不在 paths 中的键不会被覆盖。
   */
  function applyModel(doc, model, paths) {
    var out = clone(isObject(doc) ? doc : {});
    (paths || []).forEach(function (path) {
      var value = readPath(model, path);
      if (value === undefined) return;
      writePath(out, path, value);
    });
    return out;
  }

  Tempo.Toml = {
    parse: parse,
    parseLenient: parseLenient,
    stringify: stringify,
    applyModel: applyModel,
    normalize: normalize,
    defaults: function () { return clone(DEFAULTS); },
    clone: clone,
    readPath: readPath,
    writePath: writePath
  };
})(window);
