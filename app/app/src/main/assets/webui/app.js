/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * app.js —— 页面装配与渲染（总览 / 参数占位 / FAS / 充电占位 + 日志抽屉）。
 * 数据全部来自 Tempo.Api，页面不直接拼接 shell 命令。
 */
(function (global) {
  'use strict';

  var Tempo = (global.Tempo = global.Tempo || {});

  var MAX_LOG_LINES = 300;
  var APP_VERSION = '2.3';

  var ROLE = {
    efficiency: { label: '能效核', color: '#63a7ff' },
    balanced: { label: '均衡核', color: '#5fd6a8' },
    prime: { label: '超大核', color: '#ffb454' }
  };

  var REASON = {
    quiet: '静默',
    screen_off: '息屏',
    touch_lift: '触摸抬频',
    frame_burst: '帧突发',
    foreground: '前台',
    load: '负载',
    thermal: '热限',
    unchanged: '保持'
  };

  var ICONS = {
    logo:
      '<svg viewBox="0 0 32 32" aria-hidden="true">' +
      '<rect x="4" y="13" width="4" height="14" rx="2"/>' +
      '<rect x="12" y="6" width="4" height="21" rx="2"/>' +
      '<rect x="20" y="10" width="4" height="17" rx="2"/>' +
      '<rect x="28" y="3" width="3" height="24" rx="1.5"/>' +
      '</svg>',
    refresh:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M20 12a8 8 0 1 1-2.34-5.66"/>' +
      '<path d="M20 4v4h-4"/>' +
      '</svg>',
    gauge:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M4 18a8 8 0 1 1 16 0"/>' +
      '<path d="M12 18l4-6"/>' +
      '<circle cx="12" cy="18" r="1.6"/>' +
      '</svg>',
    sliders:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M5 6h14M5 12h14M5 18h14"/>' +
      '<circle cx="9" cy="6" r="2"/><circle cx="15" cy="12" r="2"/><circle cx="11" cy="18" r="2"/>' +
      '</svg>',
    flash:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M13 3L5 14h6l-1 7 8-11h-6z"/>' +
      '</svg>',
    battery:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<rect x="3" y="8" width="15" height="9" rx="2"/>' +
      '<path d="M21 11v3"/><path d="M11 10l-2 4h3l-1 3 3-5h-3l1-2z"/>' +
      '</svg>',
    logs:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<rect x="4" y="4" width="16" height="16" rx="3"/>' +
      '<path d="M8 9h8M8 13h8M8 17h5"/>' +
      '</svg>',
    close:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M6 6l12 12M18 6L6 18"/>' +
      '</svg>',
    search:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<circle cx="11" cy="11" r="6.5"/><path d="M16 16l4 4"/>' +
      '</svg>',
    check:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M5 12.5l4.5 4.5L19 7.5"/>' +
      '</svg>',
    alert:
      '<svg viewBox="0 0 24 24" aria-hidden="true">' +
      '<path d="M12 4l9 16H3z"/><path d="M12 10v4"/><circle cx="12" cy="17" r=".8"/>' +
      '</svg>'
  };

  var LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

  /*
   * 参数分组。区间与 config.rs 的 validate() 一一对应，
   * 客户端只做即时提示，最终以服务端检查结果为准。
   */
  var CFG_GROUPS = [
    {
      id: 'quiet',
      title: '静默与压缩',
      desc: '前台空闲时能压到多低、落到最低频有多快。',
      fields: [
        { path: 'control.quiet_ratio', label: '负载压缩', min: 0.05, max: 0.95, step: 0.01, decimals: 2, tip: '把负载映射为目标频率的压缩系数，越小越省电。' },
        { path: 'control.quiet_cap', label: '空闲上限', min: 0.05, max: 0.90, step: 0.01, decimals: 2, tip: '前台整体空闲时，允许的最高频率占可调区间的比例。' },
        { path: 'control.floor_enter_ratio', label: '静默进入阈值', min: 0.005, max: 0.20, step: 0.005, decimals: 3, tip: '前台阶梯负载持续低于该比例，视为没有有效前台工作。' },
        { path: 'control.drop_ratio', label: '吸附最低频阈值', min: 0.01, max: 0.30, step: 0.01, decimals: 2, tip: '目标比例低于该值时直接吸附到该簇最低频率。' },
        { path: 'control.drop_factor', label: '下落倍率', min: 1.0, max: 16.0, step: 0.5, decimals: 1, tip: '向下换档的滞回阈值按该倍率收紧，做到快下慢上。' }
      ]
    },
    {
      id: 'touch',
      title: '触摸与帧',
      desc: '触摸、前台切换与掉帧时的一次性抬频，用来保住交互手感。',
      fields: [
        { path: 'control.touch_lift_ms', label: '触摸抬频窗口', min: 20, max: 600, step: 10, integer: true, unit: 'ms', tip: '一次触摸后维持抬频的时间。' },
        { path: 'control.touch_lift_level', label: '触摸抬频目标', min: 0.10, max: 1.0, step: 0.01, decimals: 2, tip: '抬频窗口内允许达到的频率比例。' },
        { path: 'control.frame_slack_ms', label: '帧余量', min: 0.5, max: 20.0, step: 0.5, decimals: 1, unit: 'ms', tip: '渲染时间允许超出帧预算的余量。' },
        { path: 'control.frame_burst_ms', label: '掉帧抬频窗口', min: 40, max: 1000, step: 10, integer: true, unit: 'ms', tip: '检测到帧超时后的一次性抬频时间。' },
        { path: 'control.frame_burst_level', label: '掉帧抬频目标', min: 0.10, max: 1.0, step: 0.01, decimals: 2, tip: '掉帧抬频窗口内允许达到的频率比例。' }
      ]
    },
    {
      id: 'shift',
      title: '换档与角色上限',
      desc: '向上换档的滞回与按核心角色的硬上限，跨簇数通用。',
      fields: [
        { path: 'control.step_hysteresis', label: '向上换档滞回', min: 0.002, max: 0.20, step: 0.002, decimals: 3, tip: '目标比例相对上次发布值至少变化多少才向上换档。' },
        { path: 'control.reserve_ratio', label: '高负载余量', min: 1.0, max: 1.25, step: 0.05, decimals: 2, tip: '高负载段允许超过线性满档的顶部余量。' },
        { path: 'control.role_cap.efficiency', label: '能效核上限', min: 0.20, max: 1.0, step: 0.01, decimals: 2, tip: '能效簇允许达到的最高频率比例。' },
        { path: 'control.role_cap.balanced', label: '均衡核上限', min: 0.20, max: 1.0, step: 0.01, decimals: 2, tip: '均衡簇允许达到的最高频率比例。' },
        { path: 'control.role_cap.prime', label: '超大核上限', min: 0.20, max: 1.0, step: 0.01, decimals: 2, tip: '超大簇允许达到的最高频率比例。' }
      ]
    },
    {
      id: 'sample',
      title: '采样与确认',
      desc: '事件采样密度与静默确认时间；采样越稀，自身唤醒越少。',
      fields: [
        { path: 'events.runtime_quantum_us', label: '需求结算窗口', min: 500, max: 50000, step: 250, integer: true, unit: 'µs', tip: '每簇累计运行量达到该值才结算一次需求，不是定时器周期。' },
        { path: 'events.demand_step', label: '需求上报步长', min: 0.01, max: 0.20, step: 0.01, decimals: 2, tip: '需求变化达到该步长才上报，越大越省唤醒。' },
        { path: 'events.frame_stride', label: '帧事件合并', min: 1, max: 16, step: 1, integer: true, unit: '帧', tip: '每 N 帧最多上报一次，1 表示不合并。' },
        { path: 'events.quiet_decay_ms', label: '静默延迟', min: 20, max: 2000, step: 10, integer: true, unit: 'ms', tip: '最后一次触摸后至少经过该时间才允许进入静默。' },
        { path: 'control.floor_settle_ms', label: '静默确认时间', min: 10, max: 1000, step: 10, integer: true, unit: 'ms', tip: '前台负载持续低于进入阈值多久才确认静默。' },
        { path: 'control.floor_residence_ms', label: '最低频驻留', min: 0, max: 2000, step: 10, integer: true, unit: 'ms', tip: '非事件驱动的抬升需要的最短驻留；触摸与帧事件不受限。' },
        { path: 'daemon.log_level', label: '日志级别', kind: 'select', options: LOG_LEVELS, tip: '日志级别越高写入越少；排查问题时可临时调低。' }
      ]
    }
  ];

  var POWER_GROUPS = [
    {
      id: 'charge',
      title: '充电上限',
      desc: '设置停止充电的电量；100% 时按充满即停处理。',
      fields: [
        { path: 'charge.stop_at_percent', label: '停充阈值', min: 50, max: 100, step: 1, integer: true, unit: '%', presets: [80, 90, 100], tip: '电量达到该值后停止充电；100% 即充满即停。' },
        { path: 'charge.resume_at_percent', label: '恢复阈值', min: 20, max: 99, step: 1, integer: true, unit: '%', tip: '普通上限时，电量降到该值以下才继续充电；100% 时改为重新插电恢复。' }
      ]
    },
    {
      id: 'thermal',
      title: '温度保护',
      desc: '电池到温即停充，温度回落再恢复。',
      fields: [
        { path: 'charge.temp_stop_c', label: '停充温度', min: 20, max: 70, step: 0.5, decimals: 1, unit: '°C', tip: '电池达到该温度时停止充电。' },
        { path: 'charge.temp_resume_c', label: '恢复温度', min: 20, max: 69, step: 0.5, decimals: 1, unit: '°C', tip: '电池回落到该温度后恢复充电。' }
      ]
    }
  ];

  var CFG_FIELDS = [];
  var CFG_FIELD_MAP = {};
  CFG_GROUPS.concat(POWER_GROUPS).forEach(function (group) {
    group.fields.forEach(function (def) {
      CFG_FIELDS.push(def);
      CFG_FIELD_MAP[def.path] = def;
    });
  });

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function clamp01(value) {
    var number = Number(value);
    if (!isFinite(number)) return 0;
    return number < 0 ? 0 : (number > 1 ? 1 : number);
  }

  function formatGhz(khz) {
    var value = Number(khz);
    if (!isFinite(value) || value <= 0) return '--';
    return (value / 1000000).toFixed(2);
  }

  function formatPercent(value) {
    return Math.round(clamp01(value) * 100) + '%';
  }

  function formatDuration(ms) {
    var total = Math.max(0, Math.floor(Number(ms) / 1000));
    var days = Math.floor(total / 86400);
    var hours = Math.floor((total % 86400) / 3600);
    var minutes = Math.floor((total % 3600) / 60);
    var seconds = total % 60;
    if (days > 0) return days + '天 ' + hours + '小时';
    if (hours > 0) return hours + '小时 ' + minutes + '分';
    if (minutes > 0) return minutes + '分 ' + seconds + '秒';
    return seconds + '秒';
  }

  function cpuRange(cpus) {
    if (!Array.isArray(cpus) || !cpus.length) return '—';
    var sorted = cpus.slice().sort(function (a, b) { return a - b; });
    if (sorted.length === 1) return 'CPU ' + sorted[0];
    return 'CPU ' + sorted[0] + '–' + sorted[sorted.length - 1];
  }

  function roleMeta(role) {
    return ROLE[role] || { label: String(role || '未知'), color: '#93a3b8' };
  }

  function sourceLabel(kind) {
    if (kind === 'ksu') return { text: '已连接', tone: 'live' };
    if (kind === 'app') return { text: '已连接', tone: 'live' };
    return { text: '', tone: 'demo' };
  }

  function appLabel(app) {
    if (app.name) return app.name;
    return app.package || '';
  }

  function appHue(pkg) {
    var hash = 0;
    for (var i = 0; i < pkg.length; i++) hash = (hash * 31 + pkg.charCodeAt(i)) % 360;
    return hash;
  }

  function appInitial(app, label) {
    var first = String(label || '').trim().charAt(0);
    return first || 'A';
  }

  function collectNotices(data) {
    var notices = (data.errors || []).map(function (message) {
      return { level: 'error', text: message };
    });
    if (!data.status) {
      if (!notices.length) {
        notices.push({ level: 'error', text: '未能获取守护状态，请确认服务已启动。' });
      }
    }
    if (data.module && data.module.disabled) {
      notices.push({ level: 'warn', text: '模块在管理器中标记为停用。' });
    }
    return notices;
  }

  function renderHeader() {
    var chip = sourceLabel(state.source);
    return '' +
      '<header class="topbar">' +
        '<div class="brand">' +
          '<div class="brand__mark">' + ICONS.logo + '</div>' +
          '<div class="brand__text">' +
            '<h1>Aegis</h1>' +
            '<p>单模式自适应调度</p>' +
          '</div>' +
        '</div>' +
        '<div class="topbar__side">' +
          (chip.text ? '<span class="chip chip--' + chip.tone + '">' + escapeHtml(chip.text) + '</span>' : '') +
          '<button class="icon-btn' + (state.loading ? ' is-spinning' : '') + '" data-action="refresh" aria-label="刷新状态" title="刷新状态"' + (state.loading ? ' disabled' : '') + '>' +
            ICONS.refresh +
          '</button>' +
        '</div>' +
      '</header>';
  }

  function renderNotices(notices) {
    if (!notices.length) return '';
    return notices.map(function (notice) {
      return '' +
        '<div class="notice notice--' + notice.level + '">' +
          '<span class="notice__icon">' + ICONS.alert + '</span>' +
          '<span>' + escapeHtml(notice.text) + '</span>' +
        '</div>';
    }).join('');
  }

  function renderHero(data) {
    var status = data.status;
    var running = !!status;
    var mode = running ? '自适应调度' : '等待连接';
    var quiet = running && status.control && status.control.quiet;
    var uptime = running ? formatDuration(status.uptime_ms) : '--';
    var version = APP_VERSION;

    return '' +
      '<section class="card hero">' +
        '<div class="hero__head">' +
          '<div class="state">' +
            '<span class="state__dot' + (running ? '' : ' state__dot--off') + '"></span>' +
            '<div class="state__text">' +
              '<strong>' + (running ? '运行中' : '未连接') + '</strong>' +
              '<small>' + escapeHtml(mode) + '</small>' +
            '</div>' +
          '</div>' +
          (running
            ? '<span class="pill pill--' + (quiet ? 'quiet' : 'active') + '">' + (quiet ? '静默中' : '活跃') + '</span>'
            : '') +
        '</div>' +
        '<div class="meta">' +
          '<div class="meta__cell"><span>版本</span><b>' + escapeHtml(version) + '</b></div>' +
          '<div class="meta__cell"><span>平台</span><b>' + escapeHtml(running ? (status.platform || '--') : '--') + '</b></div>' +
          '<div class="meta__cell"><span>运行时长</span><b>' + escapeHtml(uptime) + '</b></div>' +
        '</div>' +
      '</section>';
  }

  function renderCluster(cluster, index) {
    var role = roleMeta(cluster.role);
    var decision = null;
    if (state.data && state.data.status && state.data.status.control &&
        Array.isArray(state.data.status.control.decisions)) {
      decision = state.data.status.control.decisions.filter(function (item) {
        return item && Number(item.cluster) === index;
      })[0] || null;
    }
    var ceiling = decision && decision.ceiling_khz ? decision.ceiling_khz : cluster.min_khz;
    var current = state.data.freqs[index];
    var hasCurrent = isFinite(Number(current)) && Number(current) > 0;
    var span = Math.max(1, Number(cluster.max_khz) - Number(cluster.min_khz));
    var ratio = hasCurrent
      ? clamp01((Number(current) - Number(cluster.min_khz)) / span)
      : 0;
    var pct = Math.max(ratio * 100, hasCurrent ? 1.5 : 0);
    var reasonLabel = decision ? (REASON[decision.reason] || decision.reason) : '—';

    return '' +
      '<article class="card cluster" style="--role:' + role.color + '">' +
        '<header class="cluster__head">' +
          '<span class="role-dot"></span>' +
          '<h2>' + escapeHtml(role.label) + '</h2>' +
          '<span class="cluster__cpus">' + escapeHtml(cpuRange(cluster.cpus)) + '</span>' +
          '<span class="tag' + (decision && decision.reason === 'quiet' ? ' tag--quiet' : '') + '">' + escapeHtml(reasonLabel) + '</span>' +
        '</header>' +
        '<div class="cluster__body">' +
          '<div class="ring" style="--pct:' + pct.toFixed(1) + '%">' +
            '<div class="ring__inner">' +
              '<b>' + (hasCurrent ? formatGhz(current) : '--') + '</b>' +
              '<small>GHz</small>' +
            '</div>' +
          '</div>' +
          '<div class="stats">' +
            '<div class="stats__row"><span>上限</span><b>' + formatGhz(ceiling) + ' GHz</b></div>' +
            '<div class="stats__row"><span>区间</span><b>' + formatGhz(cluster.min_khz) + ' – ' + formatGhz(cluster.max_khz) + '</b></div>' +
            '<div class="stats__row"><span>容量</span><b>' + escapeHtml(cluster.capacity) + '</b></div>' +
            '<div class="bars">' +
              '<div class="bar"><span>负载</span><i style="width:' + formatPercent(cluster.busy) + '"></i><em>' + formatPercent(cluster.busy) + '</em></div>' +
              '<div class="bar bar--fg"><span>前台</span><i style="width:' + formatPercent(cluster.foreground) + '"></i><em>' + formatPercent(cluster.foreground) + '</em></div>' +
            '</div>' +
          '</div>' +
        '</div>' +
      '</article>';
  }

  function vendorSuppressed(status) {
    if (!status) return false;
    if (status.vendor_scheduler_suppressed === true) return true;
    if (status.vendor_scheduling_suppressed === true) return true;
    var vendor = status.vendor_scheduler || status.vendor_scheduling;
    return !!(vendor && vendor.suppressed === true);
  }

  function renderOverview(data) {
    var notices = collectNotices(data);
    var clusters = data.status && Array.isArray(data.status.clusters)
      ? data.status.clusters
      : [];
    var clusterHtml = clusters.length
      ? clusters.map(function (cluster, index) {
          return renderCluster(cluster, index);
        }).join('')
      : '<section class="card empty"><p>未能读取设备簇信息，暂不展示频率与上限。</p></section>';

    return '' +
      renderNotices(notices) +
      renderHero(data) +
      (vendorSuppressed(data.status) ? '<div class="vendor-line">厂商调度已屏蔽</div>' : '') +
      (clusters.length
        ? '<div class="section-title"><h2>簇状态</h2><span>' + escapeHtml(clusters.length) + ' 簇 · 实时</span></div>' + clusterHtml
        : clusterHtml) +
      renderFooter(data);
  }

  function renderAppRows() {
    if (state.fasAppsLoading) {
      return '<div class="applist__state"><span class="live-dot"></span>读取应用列表…</div>';
    }
    if (state.fasAppsError) {
      return '' +
        '<div class="applist__state applist__state--error">' +
          '<span>' + escapeHtml(humanizeError(state.fasAppsError)) + '</span>' +
          '<button class="ghost-btn ghost-btn--sm" data-action="fas-apps-retry">重试</button>' +
        '</div>';
    }
    if (!state.fasApps.length) {
      return '' +
        '<div class="applist__state applist__state--error">' +
          '<span>没有读取到任何已安装应用，请确认已授予应用列表权限。</span>' +
          '<button class="ghost-btn ghost-btn--sm" data-action="fas-apps-retry">重新读取</button>' +
        '</div>';
    }

    var query = state.fasQuery.trim().toLowerCase();
    var apps = state.fasApps.filter(function (app) {
      if (!query) return true;
      return app.package.toLowerCase().indexOf(query) !== -1 ||
        appLabel(app).toLowerCase().indexOf(query) !== -1;
    });
    if (!apps.length) {
      return '<div class="applist__state">没有匹配的应用</div>';
    }

    return apps.map(function (app) {
      var label = appLabel(app);
      var selected = state.fas.packages.indexOf(app.package) !== -1;
      var kind = app.type === 'system' ? '系统应用' : '用户应用';
      return '' +
        '<button class="approw' + (selected ? ' is-selected' : '') + '" data-pkg="' + escapeHtml(app.package) + '" aria-pressed="' + selected + '"' + (state.fas.enabled ? '' : ' disabled') + '>' +
          (app.icon
            ? '<span class="appicon appicon--img"><img src="data:image/png;base64,' + escapeHtml(app.icon) + '" alt=""></span>'
            : '<span class="appicon" style="--h:' + appHue(app.package) + '">' + escapeHtml(appInitial(app, label)) + '</span>') +
          '<span class="appmeta">' +
            '<b>' + escapeHtml(label) + '</b>' +
            '<small>' + escapeHtml(app.package) + ' · ' + kind + '</small>' +
          '</span>' +
          '<span class="mini-switch" aria-hidden="true"><span></span></span>' +
        '</button>';
    }).join('');
  }

  function renderFas() {
    var fas = state.fas;
    var count = fas.packages.length;
    var statusLine;
    if (!fas.enabled) {
      statusLine = 'FAS 已关闭，勾选的应用不会生效。';
    } else if (!count) {
      statusLine = '未勾选任何应用，FAS 完全不工作。';
    } else {
      statusLine = '已对 ' + count + ' 个应用开启帧感知。';
    }

    return '' +
      '<section class="card fas-hero">' +
        '<div class="fas-hero__row">' +
          '<div class="fas-hero__text">' +
            '<h2>FAS 帧感知</h2>' +
            '<p>帧感知会略微增加功耗，只对你选中的应用开启</p>' +
          '</div>' +
          '<button class="switch' + (fas.enabled ? ' is-on' : '') + '" data-action="fas-toggle" role="switch" aria-checked="' + fas.enabled + '" aria-label="FAS 总开关"><span></span></button>' +
        '</div>' +
        '<div class="fas-hint' + (fas.enabled && !count ? ' fas-hint--warn' : '') + '">' + escapeHtml(statusLine) + '</div>' +
      '</section>' +
      '<section class="card fas-apps' + (fas.enabled ? '' : ' is-off') + '">' +
        '<div class="fas-apps__head">' +
          '<h2>应用选择</h2>' +
          '<span id="fas-count">已选 ' + count + '</span>' +
        '</div>' +
        (state.fasLoadError
          ? '<div class="applist__state applist__state--error">' +
              '<span>读取 FAS 设置失败：' + escapeHtml(humanizeError(state.fasLoadError)) + '</span>' +
              '<button class="ghost-btn ghost-btn--sm" data-action="fas-reload">重试</button>' +
            '</div>'
          : '') +
        '<label class="searchbox">' +
          '<span class="searchbox__icon">' + ICONS.search + '</span>' +
          '<input id="fas-search" type="search" placeholder="搜索应用或包名" autocomplete="off" spellcheck="false" value="' + escapeHtml(state.fasQuery) + '"' + (fas.enabled ? '' : ' disabled') + '>' +
        '</label>' +
        '<div class="applist" id="fas-list">' + renderAppRows() + '</div>' +
        '<div class="fas-note">没有勾选任何应用时，FAS 不产生任何帧感知开销。</div>' +
        '<div class="fas-spec">' + escapeHtml(
          state.fasError
            ? '保存失败：' + humanizeError(state.fasError)
            : state.fasSaving
              ? '正在保存…'
              : state.fasNote || '修改会立即保存并生效。'
        ) + '</div>' +
      '</section>';
  }

  function renderLogLines() {
    if (state.logLoading && !state.logLines.length) {
      return '<div class="logempty">读取中…</div>';
    }
    if (state.logError) {
      return '<div class="logempty logempty--error">' + escapeHtml(state.logError) + '</div>';
    }
    if (!state.logLines.length) {
      return '<div class="logempty">暂无日志</div>';
    }
    return state.logLines.map(function (line) {
      var cls = 'logline';
      if (/error|fail|拒绝|失败/i.test(line)) cls += ' logline--error';
      else if (/warn|警告/i.test(line)) cls += ' logline--warn';
      return '<div class="' + cls + '">' + escapeHtml(line) + '</div>';
    }).join('');
  }

  function renderLogSheet() {
    if (!state.logOpen) return '';
    var fileName = state.logFile ? state.logFile.split('/').pop() : '未找到日志文件';
    return '' +
      '<section class="logsheet" role="dialog" aria-label="运行日志">' +
        '<header class="logsheet__head">' +
          '<div class="logsheet__title">' +
            '<span class="live-dot"></span>' +
            '<strong>运行日志</strong>' +
            '<small>' + escapeHtml(fileName) + '</small>' +
          '</div>' +
          '<div class="logsheet__actions">' +
            '<button class="ghost-btn" data-action="logs-refresh"' + (state.logLoading ? ' disabled' : '') + '>刷新</button>' +
            '<button class="icon-btn icon-btn--sm" data-action="logs-close" aria-label="关闭日志">' + ICONS.close + '</button>' +
          '</div>' +
        '</header>' +
        '<div class="logbody">' + renderLogLines() + '</div>' +
        '<footer class="logsheet__foot">' +
          '<span>' + escapeHtml(state.logLines.length) + ' 行 · 上限 ' + MAX_LOG_LINES + ' 行</span>' +
          '<span>' + (state.logLoading ? '读取中…' : '点击刷新获取最新日志') + '</span>' +
        '</footer>' +
      '</section>';
  }

  function renderLogToggle() {
    if (state.logOpen) return '';
    return '' +
      '<button class="log-toggle" data-action="logs-open" aria-label="打开运行日志">' +
        '<span class="log-toggle__icon">' + ICONS.logs + '</span>' +
        '<span>日志</span>' +
      '</button>';
  }

  function renderFooter() {
    return '' +
      '<footer class="foot">' +
        '<div>Aegis · v' + APP_VERSION + '</div>' +
      '</footer>';
  }

  function renderPlaceholder(tab) {
    var map = {
      power: {
        title: '充电与温控',
        desc: '设置充电上限、恢复阈值与温度保护；页面同时显示当前电量和充电状态。'
      }
    };
    var item = map[tab] || map.power;
    return '' +
      '<section class="card placeholder">' +
        '<div class="placeholder__mark">' + (tab === 'power' ? ICONS.battery : ICONS.sliders) + '</div>' +
        '<h2>' + escapeHtml(item.title) + '</h2>' +
        '<p>' + escapeHtml(item.desc) + '</p>' +
      '</section>';
  }

  /* ---------- 参数页 ---------- */

  function cfgNumberText(def, value) {
    if (def.kind === 'select') return String(value == null ? '' : value);
    var number = Number(value);
    if (!isFinite(number)) return '--';
    if (def.integer) return String(Math.round(number));
    return number.toFixed(def.decimals == null ? 2 : def.decimals);
  }

  function cfgRangeText(def) {
    if (def.kind === 'select') return def.options.join(' / ');
    return cfgNumberText(def, def.min) + '–' + cfgNumberText(def, def.max);
  }

  function renderCfgField(def, model, errors) {
    var value = Tempo.Toml.readPath(model, def.path);
    var error = errors[def.path];
    var inputId = 'cfg-' + def.path.replace(/\./g, '-');
    var head =
      '<div class="cfgrow__head">' +
        '<label for="' + inputId + '">' + escapeHtml(def.label) + '</label>' +
        '<span class="cfg-value">' +
          '<b data-value-for="' + def.path + '">' + escapeHtml(cfgNumberText(def, value)) + '</b>' +
          (def.unit ? '<small>' + escapeHtml(def.unit) + '</small>' : '') +
        '</span>' +
      '</div>';

    var control;
    if (def.kind === 'select') {
      control =
        '<select id="' + inputId + '" class="cfg-select" data-field="' + def.path + '">' +
          def.options.map(function (option) {
            return '<option value="' + option + '"' + (String(value) === option ? ' selected' : '') + '>' + escapeHtml(option) + '</option>';
          }).join('') +
        '</select>';
    } else {
      var shown = cfgNumberText(def, value);
      control =
        '<div class="cfg-control">' +
          '<input id="' + inputId + '" class="cfg-range" type="range" data-field="' + def.path + '"' +
            ' min="' + def.min + '" max="' + def.max + '" step="' + def.step + '" value="' + shown + '">' +
          '<input class="cfg-number" type="number" data-field="' + def.path + '" inputmode="decimal"' +
            ' min="' + def.min + '" max="' + def.max + '" step="' + def.step + '" value="' + shown + '"' +
            ' aria-label="' + escapeHtml(def.label) + '">' +
        '</div>';
    }

    return '' +
      '<div class="cfgrow' + (error ? ' is-invalid' : '') + '" data-key="' + def.path + '">' +
        head + control +
        (def.presets
          ? '<div class="cfg-presets">' + def.presets.map(function (preset) {
              return '<button class="preset-btn' + (Number(value) === preset ? ' is-active' : '') + '" data-action="cfg-preset" data-path="' + def.path + '" data-value="' + preset + '">' + (preset === 100 ? '充满 100%' : preset + '%') + '</button>';
            }).join('') + '</div>'
          : '') +
        '<div class="cfgrow__foot"><span>' + escapeHtml(def.tip) + '</span><em>' + escapeHtml(cfgRangeText(def)) + '</em></div>' +
        '<p class="cfg-error"' + (error ? '' : ' hidden') + '>' + escapeHtml(error || '') + '</p>' +
      '</div>';
  }

  function validateConfigModel(model) {
    var errors = {};
    CFG_FIELDS.forEach(function (def) {
      var value = Tempo.Toml.readPath(model, def.path);
      if (def.kind === 'select') {
        if (def.options.indexOf(String(value)) === -1) errors[def.path] = '取值不在允许范围';
        return;
      }
      var number = Number(value);
      if (!isFinite(number)) {
        errors[def.path] = '必须是数字';
        return;
      }
      if (number < def.min || number > def.max) {
        errors[def.path] = '需在 ' + cfgRangeText(def) + ' 范围内';
      }
    });

    var control = model.control;
    var events = model.events;
    var charge = model.charge;

    if (charge.stop_at_percent < 50 || charge.stop_at_percent > 100) {
      errors['charge.stop_at_percent'] = '停充电量需在 50–100% 之间';
    }
    if (charge.resume_at_percent < 20) {
      errors['charge.resume_at_percent'] = '恢复电量不得低于 20%';
    }
    if (charge.resume_at_percent >= charge.stop_at_percent) {
      errors['charge.resume_at_percent'] = '恢复电量必须低于停充电量';
    }
    if (charge.temp_stop_c < 20 || charge.temp_stop_c > 70) {
      errors['charge.temp_stop_c'] = '停充温度需在 20–70°C 之间';
    }
    if (charge.temp_resume_c >= charge.temp_stop_c) {
      errors['charge.temp_resume_c'] = '恢复温度必须低于停充温度';
    }

    if (control.floor_enter_ratio > control.drop_ratio) {
      errors['control.floor_enter_ratio'] = '静默进入阈值不能高于吸附最低频阈值';
    }
    if (events.quiet_decay_ms + control.floor_settle_ms > 300) {
      var budget = '静默进入预算需 ≤ 300ms（当前 ' + Math.round(events.quiet_decay_ms + control.floor_settle_ms) + 'ms）';
      errors['events.quiet_decay_ms'] = budget;
      errors['control.floor_settle_ms'] = budget;
    }
    return errors;
  }

  function firstErrorText(errors) {
    var keys = Object.keys(errors);
    return keys.length ? errors[keys[0]] : '';
  }

  function configDirty() {
    if (!state.cfg.original || !state.cfg.draft) return false;
    return JSON.stringify(state.cfg.original) !== JSON.stringify(state.cfg.draft);
  }

  function renderSavebar(errors) {
    var count = Object.keys(errors).length;
    var dirty = configDirty();
    var summary;
    if (state.cfg.saving) summary = state.cfg.saveMessage || '保存中…';
    else if (count) summary = count + ' 项参数需要调整';
    else if (dirty) summary = '有未保存的更改';
    else summary = '与当前配置一致';

    var message = state.cfg.saveMessage ||
      (count ? firstErrorText(errors) : '保存后立即生效，可随时恢复上一份设置');
    var tone = state.cfg.saveTone ? ' savebar__msg--' + state.cfg.saveTone : '';

    return '' +
      '<section class="card savebar">' +
        '<div class="savebar__info">' +
          '<b id="cfg-summary">' + escapeHtml(summary) + '</b>' +
          '<small id="cfg-message" class="savebar__msg' + tone + '">' + escapeHtml(message) + '</small>' +
        '</div>' +
        '<div class="savebar__actions">' +
          '<button class="ghost-btn" data-action="cfg-rollback"' + (state.cfg.saving ? ' disabled' : '') + '>恢复上一份</button>' +
          '<button class="primary-btn" data-action="cfg-save"' + (state.cfg.saving || count > 0 || !dirty ? ' disabled' : '') + '>' +
            (state.cfg.saving ? '保存中…' : '保存并校验') +
          '</button>' +
        '</div>' +
      '</section>';
  }

  function renderConfigPage() {
    if (state.cfg.error) {
      return '' +
        '<section class="card empty">' +
          '<h2>无法读取设置</h2>' +
          '<p>' + escapeHtml(humanizeError(state.cfg.error)) + '</p>' +
          '<button class="ghost-btn empty__action" data-action="cfg-reload">重试</button>' +
        '</section>';
    }
    if (!state.cfg.loaded) {
      return '' +
        '<section class="card cfg-loading">' +
          '<span class="live-dot"></span>' +
          '<div><b>正在读取设置</b><small>读取后即可调整各项参数。</small></div>' +
        '</section>';
    }

    var errors = validateConfigModel(state.cfg.draft);
    return '' +
      '<div class="section-title"><h2>参数</h2><span>修改后保存生效</span></div>' +
      CFG_GROUPS.map(function (group) {
        return '' +
          '<section class="card cfg-group">' +
            '<header class="cfg-group__head">' +
              '<div><h2>' + escapeHtml(group.title) + '</h2><p>' + escapeHtml(group.desc) + '</p></div>' +
              '<button class="ghost-btn ghost-btn--sm" data-action="cfg-reset" data-group="' + group.id + '">恢复默认</button>' +
            '</header>' +
            group.fields.map(function (def) {
              return renderCfgField(def, state.cfg.draft, errors);
            }).join('') +
          '</section>';
      }).join('') +
      renderSavebar(errors);
  }

  /* ---------- 充电与温控 ---------- */

  /* 预约与充电状态机只在 charge-model.js 中实现，页面与沙盒共用同一套逻辑。 */
  var ChargeModel = Tempo.Charge;
  var PLAN_MODES = ChargeModel.PLAN_MODES;
  var WEEKDAY_LABELS = ChargeModel.WEEKDAY_LABELS;
  var defaultPlan = ChargeModel.defaultPlan;
  var normalizePlan = ChargeModel.normalizePlan;
  var isValidWindow = ChargeModel.isValidWindow;
  var planState = ChargeModel.planState;

  function chargeLatchMode(model) {
    return numberOrNull(model && model.charge && model.charge.stop_at_percent) === 100;
  }

  /* 100% 是充满即停的唯一入口，其余阈值一律不保持。 */
  function deriveChargeLatch(model) {
    if (!model || !model.charge) return;
    model.charge.latch_per_plug = chargeLatchMode(model);
  }

  function resetChargeMachine() {
    state.charge.machine = ChargeModel.createState();
    state.charge.decision = null;
    state.charge.at = null;
  }

  function refreshChargeDecision() {
    if (!state.cfg.draft) return state.charge.decision;

    var snapshot = powerSnapshot();
    var charge = state.cfg.draft.charge || {};
    var input = {
      now: new Date(),
      percent: snapshot ? numberOrNull((snapshot.battery || {}).percent) : null,
      tempC: snapshot ? numberOrNull((snapshot.battery || {}).temp_c) : null,
      plugged: snapshot ? ChargeModel.inferPlugged(snapshot) : null,
      stopAt: numberOrNull(charge.stop_at_percent),
      resumeAt: numberOrNull(charge.resume_at_percent),
      tempStop: numberOrNull(charge.temp_stop_c),
      tempResume: numberOrNull(charge.temp_resume_c),
      latchPerPlug: chargeLatchMode(state.cfg.draft),
      plan: state.plan.model
    };

    var result = ChargeModel.step(state.charge.machine, input);
    state.charge.machine = result.state;
    state.charge.decision = result;
    state.charge.at = new Date();
    return result;
  }

  function numberOrNull(value) {
    if (value == null || value === '') return null;
    var number = Number(value);
    return isFinite(number) ? number : null;
  }

  function humanizeError(value) {
    return String(value == null ? '' : value)
      .replace(/控制通道调用失败：Error invoking exec[^\n]*/gi, '控制通道调用失败，请确认已授予 root 权限')
      .replace(/Error invoking exec[^\n]*/gi, '控制通道调用失败，请确认已授予 root 权限')
      .replace(/Java exception was raised during method invocation/gi, '控制通道调用失败')
      .replace(/^(?:java\.lang\.)?RuntimeException:\s*/i, '')
      .replace(/tempoctl/gi, '控制服务')
      .replace(/config\.toml\.new|config\.toml|device\.toml|fas\.json|charge_plan\.json/gi, '设置数据')
      .replace(/schema\s*=\s*\d+/gi, '')
      .replace(/schema\s*\d+/gi, '');
  }

  function powerSnapshot() {
    if (state.power.snapshot) return state.power.snapshot;
    var status = state.data && state.data.status;
    if (!status) return null;
    return {
      ok: true,
      charging: !!status.charging,
      battery: status.battery || {},
      thermal_c: status.thermal_c
    };
  }

  function renderPowerLive() {
    var snapshot = powerSnapshot();
    var model = state.cfg.draft || {};
    var charge = model.charge || {};
    var stop = numberOrNull(charge.stop_at_percent);
    var resume = numberOrNull(charge.resume_at_percent);
    var tempStop = numberOrNull(charge.temp_stop_c);
    var latchMode = chargeLatchMode(model);

    if (!snapshot) {
      return '' +
        '<div class="power-live__head">' +
          '<div class="power-state"><b>暂无实时数据</b><small>' +
            escapeHtml(state.power.error ? humanizeError(state.power.error) : '等待读取当前电量与温度') +
          '</small></div>' +
        '</div>' +
        '<button class="ghost-btn ghost-btn--sm power-live__refresh" data-action="power-refresh">刷新状态</button>';
    }

    var decision = refreshChargeDecision() || { allowCharge: true, reasons: [], primary: null, state: {}, plan: null };
    var planLive = decision.plan || planState(state.plan.model, new Date());

    var battery = snapshot.battery || {};
    var percent = numberOrNull(battery.percent);
    var temp = numberOrNull(battery.temp_c);
    var rawStatus = String(battery.status || '').toLowerCase();
    var charging = !!snapshot.charging;
    var plugged = ChargeModel.inferPlugged(snapshot);
    var reasonText = (decision.reasons || []).map(function (reason) { return reason.label; }).join(' · ');

    var paused = !charging && !decision.allowCharge && plugged !== false && rawStatus !== 'discharging';

    var headline;
    var detail;
    if (charging) {
      headline = percent != null && percent >= 100 ? '已充满' : '正在充电';
      detail = decision.allowCharge ? '电量与温度均在设定范围内' : '条件已满足，停充即将生效';
    } else if (paused) {
      headline = '充电已暂停';
      if (decision.primary === 'full_latch') {
        detail = '已达到充电上限，保持停充；重新插电恢复';
      } else {
        detail = reasonText || '已达到停充条件';
      }
    } else {
      headline = '未在充电';
      detail = rawStatus === 'discharging'
        ? (decision.allowCharge ? '设备未插电或正在放电' : '设备可能已拔出；满足：' + reasonText)
        : '当前没有进行充电';
    }

    var remaining = percent != null && stop != null ? Math.max(0, stop - percent) : null;
    var remainingText;
    if (remaining == null) remainingText = '--';
    else if (latchMode && percent >= stop) remainingText = '已充满';
    else if (remaining <= 0) remainingText = '已到上限';
    else remainingText = Math.round(remaining) + '%';

    var pct = percent == null ? 0 : Math.max(0, Math.min(100, percent));
    var dash = (pct * 2.512).toFixed(1);

    var thermalLabel;
    var thermalTone;
    if (temp == null || tempStop == null) {
      thermalLabel = '温度未知';
      thermalTone = 'idle';
    } else if (decision.state && decision.state.tempBlocked) {
      thermalLabel = temp >= tempStop ? '已达停充温度，温度保护已触发' : '温度已回落，等待降到恢复温度';
      thermalTone = 'warn';
    } else if (temp >= tempStop) {
      thermalLabel = '已达停充温度，温度保护已触发';
      thermalTone = 'warn';
    } else if (temp >= tempStop - 2) {
      thermalLabel = '接近停充温度，注意散热';
      thermalTone = 'warn';
    } else {
      thermalLabel = '温度正常';
      thermalTone = 'ok';
    }

    var statusChip = charging
      ? '<span class="chip chip--live">充电中</span>'
      : paused
        ? '<span class="chip chip--warn">本模块已停充</span>'
        : '<span class="chip chip--idle">未停充</span>';

    return '' +
      '<div class="power-live__head">' +
        '<div class="power-ring-wrap">' +
          '<svg class="power-ring" viewBox="0 0 96 96" aria-hidden="true">' +
            '<circle cx="48" cy="48" r="40" fill="none" stroke="rgba(255,255,255,0.10)" stroke-width="9"></circle>' +
            '<circle class="power-ring__arc' + (charging ? ' is-charging' : '') + '" cx="48" cy="48" r="40" fill="none" stroke-width="9" stroke-linecap="round"' +
              ' stroke-dasharray="' + dash + ' 251.2" transform="rotate(-90 48 48)"></circle>' +
          '</svg>' +
          '<div class="power-ring__label"><b>' + (percent == null ? '--' : Math.round(percent)) + '</b><small>%</small></div>' +
        '</div>' +
        '<div class="power-state">' +
          statusChip +
          '<b>' + escapeHtml(headline) + '</b>' +
          '<small>' + escapeHtml(detail) + '</small>' +
        '</div>' +
        '<button class="ghost-btn ghost-btn--sm power-live__refresh" data-action="power-refresh">刷新</button>' +
      '</div>' +
      '<div class="power-metrics">' +
        '<div class="power-metric"><span>当前电量</span><b>' + (percent == null ? '--' : Math.round(percent) + '%') + '</b></div>' +
        '<div class="power-metric"><span>停充阈值</span><b>' + (stop == null ? '--' : stop + '%') + '</b></div>' +
        '<div class="power-metric"><span>恢复阈值</span><b>' + (latchMode ? '重新插电' : (resume == null ? '--' : resume + '%')) + '</b></div>' +
        '<div class="power-metric"><span>距离上限</span><b>' + remainingText + '</b></div>' +
      '</div>' +
      '<div class="power-strip">' +
        '<span class="chip chip--' + planLive.tone + '">' + escapeHtml(planLive.text) + '</span>' +
        '<span class="power-strip__text">' + escapeHtml(planLive.detail) + '</span>' +
      '</div>' +
      '<div class="power-strip">' +
        '<span class="chip chip--' + thermalTone + '">' + (temp == null ? '电池温度' : '电池 ' + temp.toFixed(1) + '°C') + '</span>' +
        '<span class="power-strip__text">' + escapeHtml(thermalLabel) + '</span>' +
      '</div>' +
      (state.power.error ? '<p class="power-live__error">' + escapeHtml(humanizeError(state.power.error)) + '</p>' : '') +
      '<p class="power-live__foot">更新于 ' + escapeHtml(state.power.fetchedAt || '--') + '</p>';
  }

  function renderPlanNow() {
    var live = planState(state.plan.model, new Date());
    return '<span class="chip chip--' + live.tone + '">' + escapeHtml(live.text) + '</span>' +
      '<span class="plan-now__text">' + escapeHtml(live.detail) + '</span>';
  }

  function renderPlanNote() {
    if (state.plan.error) return '保存失败：' + humanizeError(state.plan.error);
    if (state.plan.saving) return '正在保存…';
    if (state.plan.note) return state.plan.note;
    return '修改会立即保存，充电控制按此时段执行。';
  }

  function renderChargePlan() {
    var plan = state.plan.model;
    var off = plan.enabled ? '' : ' disabled';

    var dayChips = plan.mode === 'custom'
      ? '<div class="daychips">' + WEEKDAY_LABELS.map(function (label, index) {
          var day = index + 1;
          var on = plan.weekdays.indexOf(day) !== -1;
          return '<button class="daychip' + (on ? ' is-on' : '') + '" data-action="plan-day" data-day="' + day + '"' + off + '>' + label + '</button>';
        }).join('') + '</div>'
      : '';

    var rows = plan.windows.map(function (win, index) {
      return '' +
        '<div class="plan-row">' +
          '<input class="time-field" type="time" value="' + escapeHtml(win.start) + '" data-plan-role="start" data-index="' + index + '" aria-label="开始时间"' + off + '>' +
          '<span class="plan-row__sep">至</span>' +
          '<input class="time-field" type="time" value="' + escapeHtml(win.end) + '" data-plan-role="end" data-index="' + index + '" aria-label="结束时间"' + off + '>' +
          '<button class="plan-del" data-action="plan-remove" data-index="' + index + '" aria-label="删除该时段"' + off + '>×</button>' +
        '</div>';
    }).join('');

    return '' +
      '<section class="card plan-card' + (plan.enabled ? '' : ' is-off') + '">' +
        '<header class="plan-head">' +
          '<div>' +
            '<h2>预约充电</h2>' +
            '<p>只在设定时段内充电，时段外自动停充，进入时段自动恢复。</p>' +
          '</div>' +
          '<button class="switch' + (plan.enabled ? ' is-on' : '') + '" data-action="plan-toggle" role="switch" aria-checked="' + plan.enabled + '" aria-label="预约充电开关"><span></span></button>' +
        '</header>' +
        '<div class="plan-body">' +
          '<div class="segmented">' + PLAN_MODES.map(function (mode) {
            return '<button class="seg' + (plan.mode === mode.id ? ' is-active' : '') + '" data-action="plan-mode" data-mode="' + mode.id + '"' + off + '>' + mode.label + '</button>';
          }).join('') + '</div>' +
          dayChips +
          '<div class="plan-windows">' + (rows || '<div class="plan-empty">还没有时段，添加后才会限制充电。</div>') + '</div>' +
          '<button class="ghost-btn ghost-btn--sm plan-add" data-action="plan-add"' + off + '>+ 添加时段</button>' +
          '<div class="plan-now" id="plan-now">' + renderPlanNow() + '</div>' +
          '<div class="plan-note" id="plan-note">' + escapeHtml(renderPlanNote()) + '</div>' +
        '</div>' +
      '</section>';
  }

  function renderChargePolicyHint() {
    return '<p class="cfg-policy">选择 100% 时，充满后自动停止并保持，需重新插电才恢复充电；选择较低上限时，达到上限后停充，电量回落到恢复阈值后自动继续充电。</p>';
  }

  function renderPowerPage() {
    if (state.cfg.error) {
      return '' +
        '<section class="card empty">' +
          '<h2>无法读取设置</h2>' +
          '<p>' + escapeHtml(humanizeError(state.cfg.error)) + '</p>' +
          '<button class="ghost-btn empty__action" data-action="cfg-reload">重试</button>' +
        '</section>';
    }
    if (!state.cfg.loaded) {
      return '' +
        '<section class="card cfg-loading">' +
          '<span class="live-dot"></span>' +
          '<div><b>正在读取当前设置</b><small>读取完成后即可查看实时状态并调整上限。</small></div>' +
        '</section>';
    }

    var errors = validateConfigModel(state.cfg.draft);
    return '' +
      '<div class="section-title"><h2>充电与温控</h2><span>实时状态与上限设置</span></div>' +
      '<section class="card power-live" id="power-live">' + renderPowerLive() + '</section>' +
      renderChargePlan() +
      POWER_GROUPS.map(function (group) {
        return '' +
          '<section class="card cfg-group">' +
            '<header class="cfg-group__head">' +
              '<div><h2>' + escapeHtml(group.title) + '</h2><p>' + escapeHtml(group.desc) + '</p></div>' +
              '<button class="ghost-btn ghost-btn--sm" data-action="cfg-reset" data-group="' + group.id + '">恢复默认</button>' +
            '</header>' +
            group.fields.map(function (def) {
              return renderCfgField(def, state.cfg.draft, errors);
            }).join('') +
            (group.id === 'charge' ? renderChargePolicyHint() : '') +
          '</section>';
      }).join('') +
      renderSavebar(errors);
  }

  function updatePowerLive() {
    var box = document.getElementById('power-live');
    if (box) box.innerHTML = renderPowerLive();
  }

  function updatePlanNow() {
    var box = document.getElementById('plan-now');
    if (box) box.innerHTML = renderPlanNow();
    var note = document.getElementById('plan-note');
    if (note) note.textContent = renderPlanNote();
  }

  /** 局部更新校验提示，避免拖动滑杆时整页重绘。 */
  function applyValidationUi() {
    if (!state.cfg.loaded) return;
    var errors = validateConfigModel(state.cfg.draft);

    CFG_FIELDS.forEach(function (def) {
      var row = document.querySelector('.cfgrow[data-key="' + def.path + '"]');
      if (!row) return;
      var error = errors[def.path];
      row.classList.toggle('is-invalid', !!error);
      var box = row.querySelector('.cfg-error');
      if (box) {
        box.textContent = error || '';
        box.hidden = !error;
      }
      var label = row.querySelector('[data-value-for="' + def.path + '"]');
      if (label) {
        label.textContent = cfgNumberText(def, Tempo.Toml.readPath(state.cfg.draft, def.path));
      }
    });

    var count = Object.keys(errors).length;
    var dirty = configDirty();
    var summary = document.getElementById('cfg-summary');
    if (summary) {
      summary.textContent = state.cfg.saving
        ? (state.cfg.saveMessage || '保存中…')
        : count
          ? (count + ' 项参数需要调整')
          : dirty
            ? '有未保存的更改'
            : '与当前配置一致';
    }

    var message = document.getElementById('cfg-message');
    if (message) {
      message.textContent = state.cfg.saveMessage ||
        (count ? firstErrorText(errors) : '保存后立即生效，可随时恢复上一份设置');
      message.className = 'savebar__msg' + (state.cfg.saveTone ? ' savebar__msg--' + state.cfg.saveTone : '');
    }

    var button = document.querySelector('[data-action="cfg-save"]');
    if (button) {
      button.disabled = state.cfg.saving || count > 0 || !dirty;
      button.textContent = state.cfg.saving ? '保存中…' : '保存并校验';
    }
  }

  function currentBinary() {
    return (state.data && state.data.binary) || null;
  }

  function loadConfigIntoState(binary) {
    return api.loadConfig(binary).then(function (result) {
      state.cfg.loading = false;
      if (result.error) {
        state.cfg.loaded = false;
        state.cfg.error = result.error;
        render();
        return false;
      }
      state.cfg.model = result.model;
      state.cfg.original = Tempo.Toml.clone(result.model);
      state.cfg.draft = Tempo.Toml.clone(result.model);
      deriveChargeLatch(state.cfg.draft);
      if (state.cfg.original.charge && state.cfg.original.charge.latch_per_plug !== state.cfg.draft.charge.latch_per_plug) {
        state.cfg.saveMessage = '停充行为会随上限自动调整，保存后写入设备';
        state.cfg.saveTone = 'info';
      }
      resetChargeMachine();
      state.cfg.loaded = true;
      state.cfg.error = null;
      render();
      return true;
    });
  }

  function ensureConfig() {
    if (state.cfg.loaded || state.cfg.loading) return;
    state.cfg.loading = true;
    state.cfg.error = null;
    render();

    var binary = currentBinary();
    if (binary) {
      loadConfigIntoState(binary);
      return;
    }
    api.resolveBinary().then(loadConfigIntoState).catch(function (error) {
      state.cfg.loading = false;
      state.cfg.loaded = false;
      state.cfg.error = '无法连接控制服务：' + String((error && error.message) || error);
      render();
    });
  }

  /** 保存成功后把回读值同步回控件，避免整页重绘导致整屏闪动。 */
  function syncConfigInputs() {
    if (!state.cfg.loaded) return;
    CFG_FIELDS.forEach(function (def) {
      var row = document.querySelector('.cfgrow[data-key="' + def.path + '"]');
      if (!row) return;
      var value = Tempo.Toml.readPath(state.cfg.draft, def.path);
      var text = cfgNumberText(def, value);
      var range = row.querySelector('input[type="range"]');
      if (range) range.value = String(Math.min(def.max, Math.max(def.min, Number(value))));
      var number = row.querySelector('input[type="number"]');
      if (number) number.value = text;
      var select = row.querySelector('select');
      if (select) select.value = text;
    });
  }

  function saveConfig() {
    if (!state.cfg.loaded || state.cfg.saving) return;
    deriveChargeLatch(state.cfg.draft);
    var errors = validateConfigModel(state.cfg.draft);
    if (Object.keys(errors).length) {
      state.cfg.saveMessage = '请先修正标红的参数';
      state.cfg.saveTone = 'error';
      applyValidationUi();
      return;
    }

    state.cfg.saving = true;
    state.cfg.saveMessage = '正在保存设置…';
    state.cfg.saveTone = 'info';
    applyValidationUi();

    var stepText = {
      write: '正在保存设置…',
      validate: '正在检查取值…',
      commit: '正在应用设置…',
      readback: '正在确认结果…'
    };

    var run = function (binary) {
      api.applyConfig(binary, state.cfg.draft, function (step) {
        state.cfg.saveMessage = stepText[step] || '保存中…';
        state.cfg.saveTone = 'info';
        applyValidationUi();
      }).then(function (result) {
        state.cfg.saving = false;
        if (!result.ok) {
          state.cfg.saveMessage = (result.step === 'validate' ? '取值未通过检查：' : '保存失败：') + humanizeError(result.detail);
          state.cfg.saveTone = 'error';
          applyValidationUi();
          return;
        }
        if (result.model) {
          state.cfg.model = result.model;
          state.cfg.original = Tempo.Toml.clone(result.model);
          state.cfg.draft = Tempo.Toml.clone(result.model);
          deriveChargeLatch(state.cfg.draft);
        }
        state.cfg.saveMessage = '已保存并生效';
        state.cfg.saveTone = 'ok';
        syncConfigInputs();
        applyValidationUi();
        updatePowerLive();
        updatePlanNow();
        Tempo.App.refreshLogs();
      }).catch(function (error) {
        state.cfg.saving = false;
        state.cfg.saveMessage = '保存失败：' + humanizeError(String((error && error.message) || error));
        state.cfg.saveTone = 'error';
        applyValidationUi();
      });
    };

    var binary = currentBinary();
    if (binary) {
      run(binary);
      return;
    }
    api.resolveBinary().then(run).catch(function (error) {
      state.cfg.saving = false;
      state.cfg.saveMessage = '无法连接控制服务：' + String((error && error.message) || error);
      state.cfg.saveTone = 'error';
      applyValidationUi();
    });
  }

  function restoreConfigFile() {
    if (state.cfg.saving) return;
    state.cfg.saving = true;
    state.cfg.saveMessage = '正在恢复上一份配置…';
    state.cfg.saveTone = 'info';
    applyValidationUi();

    api.restoreConfig().then(function (result) {
      if (!result.ok) {
        state.cfg.saving = false;
        state.cfg.saveMessage = result.detail;
        state.cfg.saveTone = 'error';
        render();
        return;
      }
      var finish = function (ok) {
        state.cfg.saving = false;
        if (ok) {
          state.cfg.saveMessage = '已恢复上一份配置';
          state.cfg.saveTone = 'ok';
        }
        render();
      };
      var binary = currentBinary();
      if (binary) {
        loadConfigIntoState(binary).then(finish);
        return;
      }
      api.resolveBinary().then(loadConfigIntoState).then(finish).catch(function (error) {
        state.cfg.saving = false;
        state.cfg.saveMessage = '恢复后回读失败：' + String((error && error.message) || error);
        state.cfg.saveTone = 'error';
        render();
      });
    });
  }

  function resetCfgGroup(groupId) {
    var group = CFG_GROUPS.concat(POWER_GROUPS).filter(function (item) { return item.id === groupId; })[0];
    if (!group || !state.cfg.loaded) return;
    var defaults = Tempo.Toml.defaults();
    group.fields.forEach(function (def) {
      Tempo.Toml.writePath(state.cfg.draft, def.path, Tempo.Toml.readPath(defaults, def.path));
    });
    if (groupId === 'charge') deriveChargeLatch(state.cfg.draft);
    state.cfg.saveMessage = '已恢复「' + group.title + '」默认值，尚未保存';
    state.cfg.saveTone = 'info';
    render();
  }

  function onCfgFieldInput(target) {
    var field = target.closest('[data-field]');
    if (!field) return;
    var def = CFG_FIELD_MAP[field.dataset.field];
    if (!def) return;

    var raw = field.value;
    var value;
    if (def.kind === 'select') {
      value = raw;
    } else {
      if (raw === '' || raw === '-' || raw === '.') return;
      value = Number(raw);
      if (!isFinite(value)) return;
      if (def.integer) value = Math.round(value);
      if (field.type === 'range') {
        value = Math.min(def.max, Math.max(def.min, value));
      }
    }

    Tempo.Toml.writePath(state.cfg.draft, def.path, value);
    if (def.path === 'charge.stop_at_percent') deriveChargeLatch(state.cfg.draft);

    if (def.kind !== 'select' && field.type === 'number') {
      var slider = field.closest('.cfgrow').querySelector('input[type="range"]');
      if (slider) slider.value = String(Math.min(def.max, Math.max(def.min, value)));
    }

    state.cfg.saveMessage = null;
    state.cfg.saveTone = null;
    applyValidationUi();
    updatePowerLive();
  }

  function renderSkeleton() {
    return '' +
      '<div class="skeleton">' +
        '<div class="skeleton__hero"></div>' +
        '<div class="skeleton__card"></div>' +
        '<div class="skeleton__card"></div>' +
        '<div class="skeleton__card"></div>' +
      '</div>';
  }

  function renderNav() {
    var tabs = [
      { id: 'overview', label: '总览', icon: ICONS.gauge },
      { id: 'config', label: '参数', icon: ICONS.sliders },
      { id: 'fas', label: 'FAS', icon: ICONS.flash },
      { id: 'power', label: '充电', icon: ICONS.battery }
    ];
    return '' +
      '<nav class="tabbar" role="tablist">' +
        '<span class="tabbar__pill" aria-hidden="true"></span>' +
        tabs.map(function (tab) {
          var active = state.tab === tab.id;
          return '' +
            '<button class="tab' + (active ? ' is-active' : '') + '" data-tab="' + tab.id + '" role="tab" aria-selected="' + active + '">' +
              '<span class="tab__icon">' + tab.icon + '</span>' +
              '<span class="tab__label">' + tab.label + '</span>' +
            '</button>';
        }).join('') +
      '</nav>';
  }

  var state = {
    tab: 'overview',
    source: 'unknown',
    loading: false,
    data: null,
    updatedAt: null,
    pill: null,
    logOpen: false,
    logLoading: false,
    logLines: [],
    logFile: null,
    logError: null,
    fas: { enabled: false, packages: [] },
    fasApps: [],
    fasAppsLoaded: false,
    fasAppsLoading: false,
    fasAppsError: null,
    fasLoaded: false,
    fasLoading: false,
    fasLoadError: null,
    fasSaving: false,
    fasNote: null,
    fasError: null,
    fasQuery: '',
    power: {
      snapshot: null,
      error: null,
      fetchedAt: null,
      inFlight: false
    },
    plan: {
      loaded: false,
      loading: false,
      saving: false,
      error: null,
      note: null,
      model: defaultPlan()
    },
    charge: {
      machine: ChargeModel.createState(),
      decision: null,
      at: null
    },
    cfg: {
      loaded: false,
      loading: false,
      error: null,
      model: null,
      original: null,
      draft: null,
      saving: false,
      saveMessage: null,
      saveTone: null
    }
  };

  var api = null;

  function layoutTabPill(animate) {
    var bar = document.querySelector('.tabbar');
    if (!bar) return;
    var pill = bar.querySelector('.tabbar__pill');
    var active = bar.querySelector('.tab.is-active');
    if (!pill || !active) return;

    var barRect = bar.getBoundingClientRect();
    var tabRect = active.getBoundingClientRect();
    var target = {
      x: tabRect.left - barRect.left + 6,
      w: Math.max(40, tabRect.width - 12)
    };

    function apply(pos) {
      pill.style.width = pos.w + 'px';
      pill.style.transform = 'translateX(' + pos.x + 'px)';
    }

    if (animate && state.pill) {
      pill.style.transition = 'none';
      apply(state.pill);
      void pill.offsetWidth;
      pill.style.transition = '';
      global.requestAnimationFrame(function () { apply(target); });
    } else {
      apply(target);
    }
    state.pill = target;
  }

  function render() {
    var app = document.getElementById('app');
    var body = '';
    if (state.tab === 'overview') {
      if (state.data) {
        body = renderOverview(state.data);
      } else if (state.loading) {
        body = renderSkeleton();
      } else {
        body = '' +
          '<section class="card empty">' +
            '<h2>尚无数据</h2>' +
            '<p>点击右上角刷新，读取守护状态与当前频率。</p>' +
          '</section>' +
          renderFooter({ module: null });
      }
    } else if (state.tab === 'fas') {
      body = renderFas();
    } else if (state.tab === 'config') {
      body = renderConfigPage();
    } else if (state.tab === 'power') {
      body = renderPowerPage();
    } else {
      body = renderPlaceholder(state.tab);
    }

    app.innerHTML = '' +
      '<div class="shell">' +
        renderHeader() +
        '<main class="main">' + body + '</main>' +
      '</div>' +
      renderLogToggle() +
      renderLogSheet() +
      renderNav();

    layoutTabPill(!!state.pill);
  }

  function updateFasList() {
    var list = document.getElementById('fas-list');
    if (list) list.innerHTML = renderAppRows();
    var count = document.getElementById('fas-count');
    if (count) count.textContent = '已选 ' + state.fas.packages.length;
  }

  function ensureApps(force) {
    if (state.fasAppsLoading) return;
    if (state.fasAppsLoaded && !force) return;
    state.fasAppsLoading = true;
    state.fasAppsError = null;
    if (state.tab === 'fas') updateFasList();

    var finish = function (error, apps) {
      state.fasAppsLoading = false;
      state.fasApps = apps || [];
      state.fasAppsError = error || null;
      state.fasAppsLoaded = !error;
      if (state.tab === 'fas') updateFasList();
    };

    api.queryPackages().then(function (result) {
      var apps = result.apps || [];
      if (result.error || !apps.length) {
        finish(result.error, apps);
        return;
      }
      var needLabels = apps.some(function (app) { return !app.name; });
      if (!needLabels) {
        finish(null, apps);
        return;
      }
      return api.queryLabels().then(function (labels) {
        // 优先级：APP 导出的映射 > 系统查询到的名称 > 包名回退。
        apps = apps.map(function (app) {
          return {
            package: app.package,
            name: labels[app.package] || app.name || null,
            icon: app.icon || null,
            type: app.type || 'user'
          };
        });
        apps.sort(function (a, b) {
          if (a.type !== b.type) return a.type === 'user' ? -1 : 1;
          return (a.name || a.package).localeCompare(b.name || b.package);
        });
        finish(null, apps);
      }).catch(function () {
        finish(null, apps);
      });
    }).catch(function (error) {
      finish('读取应用列表失败：' + String((error && error.message) || error), []);
    });
  }

  function toggleApp(pkg) {
    if (!state.fas.enabled) return;
    var index = state.fas.packages.indexOf(pkg);
    if (index === -1) state.fas.packages.push(pkg);
    else state.fas.packages.splice(index, 1);
    updateFasList();
    persistFas();
  }

  function ensureFasLoaded(force) {
    if (state.fasLoading) return;
    if (state.fasLoaded && !force) return;
    state.fasLoading = true;
    state.fasLoadError = null;
    if (state.tab === 'fas') render();
    api.loadFas().then(function (result) {
      state.fasLoading = false;
      if (!result.error) {
        state.fas.enabled = !!result.enabled;
        state.fas.packages = result.packages || [];
        state.fasLoaded = true;
      } else {
        state.fasLoaded = false;
      }
      state.fasLoadError = result.error || null;
      if (state.tab === 'fas') render();
    }).catch(function (error) {
      state.fasLoading = false;
      state.fasLoaded = false;
      state.fasLoadError = String((error && error.message) || error);
      if (state.tab === 'fas') render();
    });
  }

  var fasSaveTimer = null;

  /** FAS 写入侧车文件，做了 350ms 合并，避免连续勾选反复写盘。 */
  function persistFas() {
    if (!api) return;
    global.clearTimeout(fasSaveTimer);
    state.fasSaving = true;
    state.fasNote = '正在保存…';
    fasSaveTimer = global.setTimeout(function () {
      api.saveFas(state.fas).then(function (result) {
        state.fasSaving = false;
        state.fasError = result.error;
        state.fasNote = result.error ? '保存失败：' + humanizeError(result.error) : '已保存';
        if (state.tab === 'fas') render();
      });
    }, 350);
  }

  var planSaveTimer = null;

  function ensurePlanLoaded() {
    if (state.plan.loaded || state.plan.loading) return;
    state.plan.loading = true;
    api.loadChargePlan().then(function (result) {
      state.plan.loading = false;
      state.plan.loaded = true;
      if (result.error) {
        state.plan.error = null;
        state.plan.note = '读取失败，当前显示默认时段';
      } else {
        state.plan.model = normalizePlan(result.model);
      }
      if (state.tab === 'power') render();
    });
  }

  function planProblem() {
    var plan = state.plan.model;
    for (var index = 0; index < plan.windows.length; index += 1) {
      if (!isValidWindow(plan.windows[index])) return '开始时间与结束时间不能相同';
    }
    return null;
  }

  function persistPlan() {
    var problem = planProblem();
    if (problem) {
      state.plan.error = problem;
      state.plan.note = null;
      updatePlanNow();
      return;
    }
    global.clearTimeout(planSaveTimer);
    state.plan.saving = true;
    state.plan.error = null;
    state.plan.note = null;
    updatePlanNow();
    planSaveTimer = global.setTimeout(function () {
      api.saveChargePlan(state.plan.model).then(function (result) {
        state.plan.saving = false;
        state.plan.error = result.error;
        state.plan.note = result.error ? null : '已保存';
        updatePlanNow();
      });
    }, 450);
  }

  function onPlanTimeChange(target) {
    var index = Number(target.dataset.index);
    var role = target.dataset.planRole;
    if (!isFinite(index) || role !== 'start' && role !== 'end') return;
    var win = state.plan.model.windows[index];
    if (!win) return;
    win[role] = target.value;
    persistPlan();
    updatePlanNow();
    updatePowerLive();
  }

  /** 只在充电页可见时读取实时电源快照，避免后台轮询。 */
  function fetchPower() {
    if (!api || state.power.inFlight) return;
    state.power.inFlight = true;
    var finish = function () {
      state.power.inFlight = false;
    };
    var read = function (binary) {
      return api.queryPower(binary).then(function (result) {
        if (result.snapshot) {
          state.power.snapshot = result.snapshot;
          state.power.fetchedAt = new Date().toLocaleTimeString('zh-CN', { hour12: false });
        }
        state.power.error = result.error;
        if (state.tab === 'power') {
          updatePowerLive();
          updatePlanNow();
        }
      });
    };
    var binary = currentBinary();
    var task = binary
      ? read(binary)
      : api.resolveBinary().then(read).catch(function (error) {
          state.power.error = String((error && error.message) || error);
          updatePowerLive();
        });
    task.then(finish, finish);
  }

  var powerTimer = null;

  function startPowerPolling() {
    stopPowerPolling();
    fetchPower();
    powerTimer = global.setInterval(function () {
      if (state.tab === 'power' && !document.hidden) fetchPower();
    }, 12000);
  }

  function stopPowerPolling() {
    if (powerTimer) {
      global.clearInterval(powerTimer);
      powerTimer = null;
    }
  }

  function fetchLogs() {
    if (!api || state.logLoading) return;
    state.logLoading = true;
    render();
    api.queryLogs(200).then(function (result) {
      state.logLoading = false;
      state.logFile = result.file;
      state.logError = result.error;
      state.logLines = (result.lines || []).slice(-MAX_LOG_LINES);
      render();
      var body = document.querySelector('.logbody');
      if (body) body.scrollTop = body.scrollHeight;
    });
  }

  function refresh() {
    if (state.loading) return;
    state.loading = true;
    render();
    api.loadOverview().then(function (data) {
      state.data = data;
      state.source = data.source;
      state.loading = false;
      state.updatedAt = new Date();
      render();
      if (state.tab === 'power') fetchPower();
      if (state.logOpen) fetchLogs();
    }).catch(function (error) {
      state.loading = false;
      state.data = {
        source: api.kind(),
        binary: null,
        status: null,
        freqs: [],
        module: null,
        errors: [String((error && error.message) || error)]
      };
      state.updatedAt = new Date();
      render();
    });
  }

  function onClick(event) {
    var target = event.target.closest('[data-action], [data-tab], [data-pkg]');
    if (!target) return;
    var action = target.dataset.action;

    if (action === 'refresh') {
      refresh();
      return;
    }
    if (action === 'logs-open') {
      state.logOpen = true;
      fetchLogs();
      return;
    }
    if (action === 'logs-close') {
      state.logOpen = false;
      render();
      return;
    }
    if (action === 'logs-refresh') {
      fetchLogs();
      return;
    }
    if (action === 'fas-toggle') {
      state.fas.enabled = !state.fas.enabled;
      render();
      persistFas();
      return;
    }
    if (action === 'fas-apps-retry') {
      ensureApps(true);
      return;
    }
    if (action === 'fas-reload') {
      ensureFasLoaded(true);
      return;
    }
    if (action === 'power-refresh') {
      fetchPower();
      return;
    }
    if (action === 'cfg-preset' && state.cfg.loaded) {
      Tempo.Toml.writePath(state.cfg.draft, target.dataset.path, Number(target.dataset.value));
      if (target.dataset.path === 'charge.stop_at_percent') deriveChargeLatch(state.cfg.draft);
      state.cfg.saveMessage = null;
      state.cfg.saveTone = null;
      syncConfigInputs();
      applyValidationUi();
      updatePowerLive();
      var presetRow = target.closest('.cfgrow');
      if (presetRow) {
        var presets = presetRow.querySelectorAll('.preset-btn');
        for (var i = 0; i < presets.length; i += 1) {
          presets[i].classList.toggle('is-active', presets[i].dataset.value === target.dataset.value);
        }
      }
      return;
    }
    if (action === 'plan-toggle') {
      state.plan.model.enabled = !state.plan.model.enabled;
      if (state.plan.model.enabled && !state.plan.model.windows.length) {
        state.plan.model.windows.push({ start: '23:00', end: '07:00' });
      }
      persistPlan();
      render();
      updatePowerLive();
      return;
    }
    if (action === 'plan-mode') {
      state.plan.model.mode = target.dataset.mode;
      if (state.plan.model.mode === 'custom' && !state.plan.model.weekdays.length) {
        state.plan.model.weekdays = [1, 2, 3, 4, 5];
      }
      persistPlan();
      render();
      updatePowerLive();
      return;
    }
    if (action === 'plan-day') {
      var day = Number(target.dataset.day);
      var dayIndex = state.plan.model.weekdays.indexOf(day);
      if (dayIndex === -1) {
        state.plan.model.weekdays.push(day);
        state.plan.model.weekdays.sort();
      } else if (state.plan.model.weekdays.length > 1) {
        state.plan.model.weekdays.splice(dayIndex, 1);
      } else {
        state.plan.note = '至少保留一天';
        updatePlanNow();
        return;
      }
      persistPlan();
      render();
      updatePowerLive();
      return;
    }
    if (action === 'plan-add') {
      if (state.plan.model.windows.length >= 6) {
        state.plan.note = '最多添加 6 个时段';
        updatePlanNow();
        return;
      }
      state.plan.model.windows.push({ start: '23:00', end: '07:00' });
      persistPlan();
      render();
      updatePowerLive();
      return;
    }
    if (action === 'plan-remove') {
      var removeIndex = Number(target.dataset.index);
      if (isFinite(removeIndex)) state.plan.model.windows.splice(removeIndex, 1);
      persistPlan();
      render();
      updatePowerLive();
      return;
    }
    if (action === 'cfg-save') {
      saveConfig();
      return;
    }
    if (action === 'cfg-rollback') {
      restoreConfigFile();
      return;
    }
    if (action === 'cfg-reset') {
      resetCfgGroup(target.dataset.group);
      return;
    }
    if (action === 'cfg-reload') {
      state.cfg.error = null;
      state.cfg.loaded = false;
      ensureConfig();
      return;
    }
    if (target.dataset.pkg) {
      toggleApp(target.dataset.pkg);
      return;
    }
    if (target.dataset.tab && target.dataset.tab !== state.tab) {
      state.tab = target.dataset.tab;
      if (state.tab === 'fas') {
        ensureApps();
        ensureFasLoaded();
      }
      if (state.tab === 'config') ensureConfig();
      if (state.tab === 'power') {
        ensureConfig();
        ensurePlanLoaded();
        startPowerPolling();
      } else {
        stopPowerPolling();
      }
      render();
      global.scrollTo(0, 0);
    }
  }

  function onInput(event) {
    var target = event.target;
    if (!target) return;
    if (target.id === 'fas-search') {
      state.fasQuery = target.value;
      updateFasList();
      return;
    }
    if (state.cfg.loaded && target.closest && target.closest('[data-field]')) {
      onCfgFieldInput(target);
    }
  }

  function init() {
    api = Tempo.createApi();
    state.source = api.kind();
    document.getElementById('app').addEventListener('click', onClick);
    document.getElementById('app').addEventListener('input', onInput);
    document.getElementById('app').addEventListener('change', function (event) {
      var element = event.target;
      if (!element) return;
      if (element.classList && element.classList.contains('time-field')) {
        onPlanTimeChange(element);
        return;
      }
      if (element.tagName === 'SELECT') onInput(event);
    });

    var resizeTimer = null;
    global.addEventListener('resize', function () {
      global.clearTimeout(resizeTimer);
      resizeTimer = global.setTimeout(function () {
        layoutTabPill(false);
      }, 120);
    });

    // 页面不可见时停止轮询，避免 APP 与 KernelSU 两个实例同时刷桥。
    document.addEventListener('visibilitychange', function () {
      if (document.hidden) {
        stopPowerPolling();
      } else if (state.tab === 'power') {
        startPowerPolling();
      }
    });

    refresh();
  }

  // 参数/FAS 写入成功后调用 Tempo.App.refreshLogs()，确保日志不是旧内容。
  Tempo.App = {
    refresh: refresh,
    refreshLogs: function () {
      if (state.logOpen) fetchLogs();
    }
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})(window);
