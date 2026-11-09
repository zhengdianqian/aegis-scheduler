/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/* 示例数据：目标机 Xiaomi MIX Fold 3 / babylon / kalama，三簇。 */
(function (global) {
  'use strict';

  var Tempo = (global.Tempo = global.Tempo || {});

  Tempo.Mock = {
    status: {
      ok: true,
      service: 'Aegis',
      version: '3.0',
      stage: 'active',
      mode: 'adaptive',
      scheduler_writes: true,
      schema: 1,
      platform: 'kalama',
      sdk: 36,
      cluster_count: 3,
      clusters: [
        {
          cpus: [0, 1, 2],
          role: 'efficiency',
          min_khz: 307200,
          max_khz: 2016000,
          capacity: 266,
          busy: 0.04,
          foreground: 0.0
        },
        {
          cpus: [3, 4, 5, 6],
          role: 'balanced',
          min_khz: 499200,
          max_khz: 2803200,
          capacity: 811,
          busy: 0.02,
          foreground: 0.0
        },
        {
          cpus: [7],
          role: 'prime',
          min_khz: 595200,
          max_khz: 3360000,
          capacity: 1024,
          busy: 0.01,
          foreground: 0.0
        }
      ],
      uptime_ms: 3612000,
      probe: {
        state: 'ready',
        cpu_count: 8,
        events: 48213,
        active_events: 914,
        wake_events: 1207,
        error: null
      },
      control: {
        quiet: true,
        decisions: [
          { cluster: 0, ceiling_khz: 307200, reason: 'quiet', changed: true },
          { cluster: 1, ceiling_khz: 499200, reason: 'quiet', changed: true },
          { cluster: 2, ceiling_khz: 595200, reason: 'quiet', changed: true }
        ]
      },
      charging: true,
      battery: { percent: 76, temp_c: 33.4, status: 'Charging', current_ua: 1200000 }
    },
    power: {
      ok: true,
      charging: true,
      battery: { percent: 76, temp_c: 33.4, status: 'Charging', current_ua: 1200000 },
      thermal_c: 37.5,
      at_ms: 0
    },
    chargePlanJson: JSON.stringify({
      schema: 1,
      enabled: false,
      mode: 'everyday',
      weekdays: [1, 2, 3, 4, 5, 6, 7],
      windows: [{ start: '23:00', end: '07:00' }],
      updated_ms: 0
    }, null, 2) + '\n',
    freqLines: ['0 307200', '3 499200', '7 595200'],
    /* 固定 /proc/stat 快照：间隔 0.5s，三簇占用分别约 20% / 35% / 6%，仅用于离线演示。 */
    cpuStatA: [
      'cpu  900000 0 0 8100000 0 0 0 0',
      'cpu0 100000 0 0 900000 0 0 0 0',
      'cpu1 100000 0 0 900000 0 0 0 0',
      'cpu2 100000 0 0 900000 0 0 0 0',
      'cpu3 100000 0 0 900000 0 0 0 0',
      'cpu4 100000 0 0 900000 0 0 0 0',
      'cpu5 100000 0 0 900000 0 0 0 0',
      'cpu6 100000 0 0 900000 0 0 0 0',
      'cpu7 100000 0 0 900000 0 0 0 0'
    ],
    cpuStatB: [
      'cpu  900750 0 0 8109885 0 0 0 0',
      'cpu0 100100 0 0 900400 0 0 0 0',
      'cpu1 100100 0 0 900400 0 0 0 0',
      'cpu2 100100 0 0 900400 0 0 0 0',
      'cpu3 100175 0 0 900325 0 0 0 0',
      'cpu4 100175 0 0 900325 0 0 0 0',
      'cpu5 100175 0 0 900325 0 0 0 0',
      'cpu6 100175 0 0 900325 0 0 0 0',
      'cpu7 100030 0 0 900470 0 0 0 0'
    ],
    apps: [
      { package: 'com.tencent.mm', name: '微信' },
      { package: 'com.tencent.mobileqq', name: 'QQ' },
      { package: 'com.taobao.taobao', name: '淘宝' },
      { package: 'com.jingdong.app.mall', name: '京东' },
      { package: 'com.xingin.xhs', name: '小红书' },
      { package: 'com.ss.android.ugc.aweme', name: '抖音' },
      { package: 'com.sina.weibo', name: '微博' },
      { package: 'com.netease.cloudmusic', name: '网易云音乐' },
      { package: 'com.zhihu.android', name: '知乎' },
      { package: 'com.bilibili.app.in', name: '哔哩哔哩' },
      { package: 'com.autonavi.minimap', name: '高德地图' },
      { package: 'mark.via', name: 'Via 浏览器' }
    ],
    logLines: [
      '2026-11-06 23:40:12 info  设备档案已载入：kalama，3 簇',
      '2026-11-06 23:40:12 info  控制通道就绪',
      '2026-11-06 23:40:13 info  配置已载入，日志级别 warn',
      '2026-11-06 23:41:02 info  频率上限写入：簇0 307200 kHz',
      '2026-11-06 23:41:02 info  频率上限写入：簇1 499200 kHz',
      '2026-11-06 23:41:03 info  频率上限写入：簇2 595200 kHz',
      '2026-11-06 23:41:03 warn  前台负载持续低于阈值，进入静默路径',
      '2026-11-06 23:42:18 info  触摸事件：一次性抬频窗口 150 ms',
      '2026-11-06 23:42:18 info  频率上限写入：簇0 1713600 kHz',
      '2026-11-06 23:42:19 info  抬频窗口结束，按当前负载重新决策',
      '2026-11-06 23:42:19 info  静默路径恢复：全部簇回到最低频',
      '2026-11-06 23:43:07 info  电池温度 38.3°C，温度保护未触发'
    ],
    moduleProp: [
      'id=aegis_scheduler',
      'name=Aegis',
      'version=3.0',
      'versionCode=13',
      'author=默默的阿文',
      'description=自适应 CPU 调度+FAS + 充电控制'
    ].join('\n'),
    configToml: [
      '# Aegis 调度配置（控制台生成）',
      'schema = 1',
      '',
      '[daemon]',
      'log_level = "warn"',
      'profile_path = "/data/adb/tempo/device.toml"',
      '',
      '[events]',
      'runtime_quantum_us = 50000',
      'demand_step = 0.10',
      'screen_path = ""',
      'frame_stride = 2',
      'quiet_decay_ms = 60',
      '',
      '[control]',
      'quiet_ratio = 0.50',
      'quiet_cap = 0.28',
      'floor_enter_ratio = 0.03',
      'floor_settle_ms = 60',
      'floor_residence_ms = 0',
      'drop_ratio = 0.08',
      'drop_factor = 4.0',
      'touch_lift_ms = 150',
      'touch_lift_level = 0.85',
      'frame_slack_ms = 3.0',
      'frame_burst_ms = 150',
      'frame_burst_level = 0.95',
      'reserve_ratio = 1.05',
      'step_hysteresis = 0.03',
      '',
      '[control.role_cap]',
      'efficiency = 1.00',
      'balanced = 1.00',
      'prime = 0.85',
      '',
      '[charge]',
      'stop_at_percent = 80',
      'resume_at_percent = 70',
      'temp_stop_c = 45.0',
      'temp_resume_c = 40.0',
      'latch_per_plug = true',
      '',
      '[thermal]',
      'guard_c = 45.0',
      'profile = "auto"',
      '',
      '[ui]',
      'language = "zh"'
    ].join('\n') + '\n',
    configDraft: null,
    configBak: null,
    fasJson: '{"enabled":false,"packages":[]}\n'
  };

  // 兼容 CONTRACT 中的命名
  Tempo.TempoMock = Tempo.TempoMock;
})(window);
