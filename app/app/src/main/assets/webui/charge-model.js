/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * charge-model.js —— 充电控制的纯函数模型。
 *
 * 浏览器中挂到 Tempo.Charge；Node 中可直接 require，用于沙盒仿真。
 * 不读文件、不访问设备、不依赖守护进程，只把一次采样映射成“允许/停止充电”。
 */
(function (root, factory) {
  'use strict';

  var api = factory();
  if (typeof module === 'object' && module.exports) {
    module.exports = api;
    return;
  }

  root.Tempo = root.Tempo || {};
  root.Tempo.Charge = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  var PLAN_MODES = [
    { id: 'everyday', label: '每天' },
    { id: 'workdays', label: '工作日' },
    { id: 'custom', label: '自定义' }
  ];

  var WEEKDAY_LABELS = ['一', '二', '三', '四', '五', '六', '日'];

  var REASON_LABELS = {
    temp_high: '电池温度达到停充线',
    full_latch: '已充满，保持停充',
    upper_bound: '已达到充电上限',
    plan_window: '处于预约停充时段'
  };

  var MAX_WINDOWS = 6;

  function numberOrNull(value) {
    if (value == null || value === '') return null;
    var number = Number(value);
    return isFinite(number) ? number : null;
  }

  function defaultPlan() {
    return {
      enabled: false,
      mode: 'everyday',
      weekdays: [1, 2, 3, 4, 5, 6, 7],
      windows: [{ start: '23:00', end: '07:00' }]
    };
  }

  function parseClock(value) {
    if (typeof value !== 'string' || !/^\d{2}:\d{2}$/.test(value)) return null;
    var hours = Number(value.slice(0, 2));
    var minutes = Number(value.slice(3, 5));
    if (hours > 23 || minutes > 59) return null;
    return hours * 60 + minutes;
  }

  function pad2(value) {
    return (value < 10 ? '0' : '') + value;
  }

  function clockText(total) {
    var minutes = ((total % 1440) + 1440) % 1440;
    return pad2(Math.floor(minutes / 60)) + ':' + pad2(minutes % 60);
  }

  function isValidWindow(win) {
    if (!win) return false;
    var start = parseClock(win.start);
    var end = parseClock(win.end);
    return start != null && end != null && start !== end;
  }

  function normalizePlan(raw) {
    var plan = defaultPlan();
    if (!raw || typeof raw !== 'object') return plan;

    plan.enabled = raw.enabled === true;
    if (PLAN_MODES.some(function (mode) { return mode.id === raw.mode; })) plan.mode = raw.mode;

    if (Array.isArray(raw.weekdays)) {
      var days = raw.weekdays.map(Number).filter(function (day) {
        return day >= 1 && day <= 7;
      });
      days = days.filter(function (day, index) {
        return days.indexOf(day) === index;
      }).sort(function (a, b) { return a - b; });
      if (days.length) plan.weekdays = days;
    }

    if (Array.isArray(raw.windows)) {
      var windows = raw.windows.filter(isValidWindow).map(function (win) {
        return { start: String(win.start), end: String(win.end) };
      });
      plan.windows = windows.slice(0, MAX_WINDOWS);
    }

    return plan;
  }

  function currentDayIndex(date) {
    var day = date.getDay();
    return day === 0 ? 7 : day;
  }

  function planDays(plan) {
    if (plan.mode === 'everyday') return [1, 2, 3, 4, 5, 6, 7];
    if (plan.mode === 'workdays') return [1, 2, 3, 4, 5];
    return plan.weekdays;
  }

  function dayIn(days, day) {
    return days.indexOf(day) !== -1;
  }

  function formatSpan(minutes) {
    var total = Math.max(0, Math.round(minutes));
    var days = Math.floor(total / 1440);
    var hours = Math.floor((total % 1440) / 60);
    var rest = total % 60;
    if (days) {
      if (hours && rest) return days + ' 天 ' + hours + ' 小时 ' + rest + ' 分';
      if (hours) return days + ' 天 ' + hours + ' 小时';
      if (rest) return days + ' 天 ' + rest + ' 分';
      return days + ' 天';
    }
    if (hours && rest) return hours + ' 小时 ' + rest + ' 分';
    if (hours) return hours + ' 小时';
    return rest + ' 分钟';
  }

  function dayLabel(offset, date) {
    if (offset === 0) return '今天';
    if (offset === 1) return '明天';
    return '周' + WEEKDAY_LABELS[currentDayIndex(date) - 1];
  }

  /*
   * 判断当前时刻是否处于允许充电的时段。
   * 跨零点时段归属“开始日”：周一 23:00–周二 07:00 属于周一，
   * 因此自定义只选周一时，周二凌晨仍应允许充电。
   */
  function planState(plan, now) {
    plan = normalizePlan(plan);
    var at = now instanceof Date ? now : new Date();

    if (!plan.enabled) {
      return { allowed: true, tone: 'idle', text: '预约未启用', detail: '当前不限制充电时段', next: null, remain: null };
    }

    var windows = plan.windows.filter(isValidWindow);
    if (!windows.length) {
      return { allowed: true, tone: 'idle', text: '预约已开启，但没有有效时段', detail: '没有时段时保持正常充电', next: null, remain: null };
    }

    var days = planDays(plan);
    if (!days.length) {
      return { allowed: true, tone: 'idle', text: '未选择任何星期', detail: '没有可用星期时保持正常充电', next: null, remain: null };
    }

    var minutes = at.getHours() * 60 + at.getMinutes();
    var today = currentDayIndex(at);
    var yesterday = today === 1 ? 7 : today - 1;

    var active = null;
    windows.forEach(function (win) {
      var start = parseClock(win.start);
      var end = parseClock(win.end);
      if (start == null || end == null) return;

      var remain = null;
      if (end > start) {
        if (dayIn(days, today) && minutes >= start && minutes < end) remain = end - minutes;
      } else if (minutes >= start && dayIn(days, today)) {
        remain = 1440 - minutes + end;
      } else if (minutes < end && dayIn(days, yesterday)) {
        remain = end - minutes;
      }
      if (remain == null) return;
      if (!active || remain < active.remain) active = { remain: remain, win: win };
    });

    if (active) {
      return {
        allowed: true,
        tone: 'live',
        text: '当前允许充电',
        detail: '本段还可充 ' + formatSpan(active.remain) + '（' + clockText(minutes + active.remain) + ' 结束）',
        next: null,
        remain: active.remain
      };
    }

    var next = null;
    for (var offset = 0; offset <= 8 && !next; offset += 1) {
      var dayDate = new Date(at.getTime());
      dayDate.setDate(dayDate.getDate() + offset);
      if (!dayIn(days, currentDayIndex(dayDate))) continue;

      for (var index = 0; index < windows.length; index += 1) {
        var startMin = parseClock(windows[index].start);
        if (startMin == null) continue;
        var delta = offset * 1440 + startMin - minutes;
        if (delta <= 0) continue;
        if (!next || delta < next.delta) {
          next = { delta: delta, start: startMin, offset: offset, dayDate: dayDate };
        }
      }
    }

    return {
      allowed: false,
      tone: 'warn',
      text: '当前停充时段',
      detail: next
        ? dayLabel(next.offset, next.dayDate) + ' ' + clockText(next.start) + ' 恢复充电（' + formatSpan(next.delta) + '后）'
        : '没有可用的后续时段',
      next: next,
      remain: null
    };
  }

  /* 100% 是“充满即停”的唯一入口，其余阈值都不保持。 */
  function deriveLatch(stopAt) {
    return numberOrNull(stopAt) === 100;
  }

  function reasonLabel(code) {
    return REASON_LABELS[code] || String(code || '');
  }

  function createState(initial) {
    var init = initial || {};
    var percent = numberOrNull(init.percent);
    var stopAt = numberOrNull(init.stopAt);

    return {
      tempBlocked: !!init.tempBlocked,
      upperLatched: !!init.upperLatched,
      upperHold: !!init.upperHold,
      armed: typeof init.armed === 'boolean'
        ? init.armed
        : (percent == null || stopAt == null ? true : percent < stopAt),
      stopped: !!init.stopped,
      lastPercent: Object.prototype.hasOwnProperty.call(init, 'lastPercent')
        ? numberOrNull(init.lastPercent)
        : percent,
      lastPlugged: typeof init.lastPlugged === 'boolean'
        ? init.lastPlugged
        : (typeof init.plugged === 'boolean' ? init.plugged : null),
      stopAt: stopAt,
      resumeAt: numberOrNull(init.resumeAt),
      tempStop: numberOrNull(init.tempStop),
      tempResume: numberOrNull(init.tempResume),
      latchPerPlug: typeof init.latchPerPlug === 'boolean' ? init.latchPerPlug : null
    };
  }

  /*
   * 推进一次状态机。
   *
   * 输入：now / percent / tempC / plugged / stopAt / resumeAt /
   *       tempStop / tempResume / latchPerPlug / plan
   * 输出：是否允许充电、动作、停充原因（按优先级排序）与新的内部状态。
   *
   * 停充条件取“或”：温度保护、100% 锁存、普通上限保持、预约时段外。
   * 恢复条件取“与”：以上条件必须全部解除。
   * 原因优先级：温度 > 100% 锁存 > 普通上限 > 预约。
   */
  function step(prev, input) {
    var state = createState(prev || {});
    var data = input || {};
    var flags = {
      booted: false,
      replugged: false,
      unplugged: false,
      configChanged: false
    };

    var percent = numberOrNull(data.percent);
    var temp = numberOrNull(data.tempC);
    var stopAt = numberOrNull(data.stopAt);
    var resumeAt = numberOrNull(data.resumeAt);
    var tempStop = numberOrNull(data.tempStop);
    var tempResume = numberOrNull(data.tempResume);
    var latchPerPlug = typeof data.latchPerPlug === 'boolean'
      ? data.latchPerPlug
      : deriveLatch(stopAt);
    var plugged = typeof data.plugged === 'boolean'
      ? data.plugged
      : (state.lastPlugged == null ? true : state.lastPlugged);

    var firstRun = state.stopAt == null && state.tempStop == null && state.lastPercent == null;
    var configChanged = !firstRun && (
      state.stopAt !== stopAt ||
      state.resumeAt !== resumeAt ||
      state.tempStop !== tempStop ||
      state.tempResume !== tempResume ||
      state.latchPerPlug !== latchPerPlug
    );

    if (firstRun) {
      flags.booted = true;
      state.tempBlocked = false;
      state.upperLatched = false;
      state.upperHold = false;
    } else if (configChanged) {
      flags.configChanged = true;
      state.tempBlocked = false;
      state.upperLatched = false;
      state.upperHold = false;
    }

    if (typeof state.lastPlugged === 'boolean' && state.lastPlugged !== plugged) {
      if (!plugged) {
        flags.unplugged = true;
        state.tempBlocked = false;
        state.upperLatched = false;
        state.upperHold = false;
      } else {
        flags.replugged = true;
        state.tempBlocked = false;
        state.upperLatched = false;
        state.upperHold = false;
      }
    }

    if (!plugged) {
      state.armed = true;
      state.stopped = false;
      state.lastPercent = percent;
      state.lastPlugged = false;
      state.stopAt = stopAt;
      state.resumeAt = resumeAt;
      state.tempStop = tempStop;
      state.tempResume = tempResume;
      state.latchPerPlug = latchPerPlug;

      return {
        state: state,
        allowCharge: true,
        action: 'none',
        primary: null,
        reasons: [],
        plan: planState(data.plan, data.now),
        flags: flags,
        plugged: false
      };
    }

    if (flags.replugged || flags.booted || flags.configChanged) {
      state.armed = percent == null || stopAt == null ? true : percent < stopAt;
    }

    if (temp != null && tempStop != null) {
      if (temp >= tempStop) state.tempBlocked = true;
      else if (tempResume != null && temp <= tempResume) state.tempBlocked = false;
    }

    if (latchPerPlug) {
      state.upperHold = false;
      if (percent != null && stopAt != null) {
        if (percent < stopAt) {
          state.armed = true;
        } else if (state.armed && percent >= stopAt) {
          state.upperLatched = true;
          state.armed = false;
        }
      }
    } else {
      state.upperLatched = false;
      state.armed = true;
      if (percent != null && stopAt != null) {
        if (percent >= stopAt) state.upperHold = true;
        else if (resumeAt != null && percent <= resumeAt) state.upperHold = false;
      }
    }

    var planLive = planState(data.plan, data.now);
    var reasons = [];
    if (state.tempBlocked) reasons.push('temp_high');
    if (state.upperLatched) reasons.push('full_latch');
    if (state.upperHold) reasons.push('upper_bound');
    if (!planLive.allowed) reasons.push('plan_window');

    var allowCharge = reasons.length === 0;
    var wasStopped = state.stopped;
    var action = allowCharge ? (wasStopped ? 'resume' : 'none') : (wasStopped ? 'hold' : 'stop');

    state.stopped = !allowCharge;
    state.lastPercent = percent;
    state.lastPlugged = true;
    state.stopAt = stopAt;
    state.resumeAt = resumeAt;
    state.tempStop = tempStop;
    state.tempResume = tempResume;
    state.latchPerPlug = latchPerPlug;

    return {
      state: state,
      allowCharge: allowCharge,
      action: action,
      primary: reasons.length ? reasons[0] : null,
      reasons: reasons.map(function (code) {
        return { code: code, label: reasonLabel(code) };
      }),
      plan: planLive,
      flags: flags,
      plugged: true
    };
  }

  function boot(input) {
    return step(createState({}), input);
  }

  /* 页面只能看到电源快照；用电池状态推断是否插电，与守护侧 uevent 判断一致。 */
  function inferPlugged(snapshot) {
    if (!snapshot) return null;
    if (typeof snapshot.plugged === 'boolean') return snapshot.plugged;
    var status = String((snapshot.battery && snapshot.battery.status) || '').toLowerCase();
    if (status === 'discharging') return false;
    if (status) return true;
    var current = numberOrNull(snapshot.battery && snapshot.battery.current_ua);
    if (current == null) return true;
    return current >= 0;
  }

  return {
    PLAN_MODES: PLAN_MODES,
    WEEKDAY_LABELS: WEEKDAY_LABELS,
    REASON_LABELS: REASON_LABELS,
    MAX_WINDOWS: MAX_WINDOWS,
    defaultPlan: defaultPlan,
    normalizePlan: normalizePlan,
    parseClock: parseClock,
    pad2: pad2,
    clockText: clockText,
    isValidWindow: isValidWindow,
    currentDayIndex: currentDayIndex,
    planDays: planDays,
    formatSpan: formatSpan,
    dayLabel: dayLabel,
    planState: planState,
    deriveLatch: deriveLatch,
    reasonLabel: reasonLabel,
    createState: createState,
    step: step,
    boot: boot,
    inferPlugged: inferPlugged
  };
});
