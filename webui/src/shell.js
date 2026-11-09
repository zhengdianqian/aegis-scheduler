/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * shell.js —— Tempo UI 的统一 shell 适配层。
 *
 * 页面只依赖 Tempo.Shell.create() 返回的适配器：
 *   adapter.kind        'ksu' | 'demo'
 *   adapter.exec(cmd, {timeoutMs}) -> Promise<{code, stdout, stderr}>
 *
 * 当前实现：KernelSU 的 window.ksu.exec 注入、以及可直接打开 index.html 的演示 shell。
 * 之后要加 APatch：新增一个实现并在 create() 里分支即可，页面与 api.js 不需要改。
 *
 * 自定义通道：在加载本文件前设置 window.__TEMPO_SHELL_FACTORY__ = () => adapter。
 */
(function (global) {
  'use strict';

  var Tempo = (global.Tempo = global.Tempo || {});

  function ShellError(message, detail) {
    var error = new Error(message);
    error.name = 'ShellError';
    error.detail = detail || null;
    return error;
  }

  function timeoutText(timeoutMs) {
    if (timeoutMs < 1000) return '命令执行超时（' + timeoutMs + ' 毫秒）';
    return '命令执行超时（' + Math.round(timeoutMs / 1000) + ' 秒）';
  }

  /**
   * KernelSU WebUI 注入的 ksu.exec。
   *
   * KernelSU 的 Java 桥签名为：
   *   exec(cmd: String, options: String?, callbackFunc: String)
   * options 必须是 JSON 字符串，callbackFunc 必须是“全局函数名字符串”，
   * 由内核侧通过 javascript: <name>(code, stdout, stderr) 回调。
   * 不存在可 await 的 Promise 返回，传对象或函数都会触发 Java 异常。
   *
   * 所有命令串行执行，避免页面与另一实例同时调用时把桥打满。
   */
  function KsuShell(ksu) {
    this.kind = 'ksu';
    this.ksu = ksu;
    this.seq = 0;
    this.queue = Promise.resolve();
  }

  KsuShell.prototype.exec = function (command, options) {
    var self = this;
    if (!command || typeof command !== 'string') {
      return Promise.reject(ShellError('命令为空', ''));
    }
    if (!self.ksu || typeof self.ksu.exec !== 'function') {
      return Promise.reject(ShellError('控制通道不可用，请确认已授予 root 权限', command));
    }
    var task = self.queue.then(function () {
      return self._execOne(command, options);
    }, function () {
      return self._execOne(command, options);
    });
    self.queue = task.then(function () { return null; }, function () { return null; });
    return task;
  };

  KsuShell.prototype._execOne = function (command, options) {
    var self = this;
    var timeoutMs = (options && options.timeoutMs) || 8000;
    var cwd = (options && typeof options.cwd === 'string' && options.cwd) ? options.cwd : '/';
    var payload = global.JSON ? global.JSON.stringify({ cwd: cwd }) : '{"cwd":"/"}';
    var callbackName = '__aegis_exec_' + (self.seq += 1) + '_' + Date.now();

    return new Promise(function (resolve, reject) {
      var settled = false;

      function cleanup() {
        if (settled) return true;
        settled = true;
        global.clearTimeout(timer);
        try {
          delete global[callbackName];
        } catch (ignored) {
          global[callbackName] = undefined;
        }
        return false;
      }

      var timer = global.setTimeout(function () {
        if (cleanup()) return;
        reject(ShellError(timeoutText(timeoutMs), command));
      }, timeoutMs);

      global[callbackName] = function (code, stdout, stderr) {
        if (cleanup()) return;
        resolve({
          code: Number(code) || 0,
          stdout: String(stdout == null ? '' : stdout),
          stderr: String(stderr == null ? '' : stderr)
        });
      };

      try {
        self.ksu.exec(String(command), payload, callbackName);
      } catch (error) {
        // 兼容只提供两参数形式的 KernelSU 版本：exec(cmd, callbackFunc)
        try {
          self.ksu.exec(String(command), callbackName);
        } catch (second) {
          var reason = String((second && second.message) || second || error || '未知错误');
          if (cleanup()) return;
          reject(ShellError('控制通道调用失败：' + reason, command));
        }
      }
    });
  };

  /**
   * Android APP 注入的原生桥：window.AegisShell.exec(command, id)。
   * 与 KernelSU 通道语义一致，只是换成了 APP 的 su 执行器。
   */
  function NativeShell(bridge) {
    this.kind = 'app';
    this.bridge = bridge;
    this.seq = 0;
    this.pending = {};
    this.queue = Promise.resolve();
    var self = this;
    global.AegisShellResolve = function (id, code, stdout, stderr) {
      var entry = self.pending[id];
      if (!entry) return;
      delete self.pending[id];
      global.clearTimeout(entry.timer);
      entry.resolve({
        code: Number(code) || 0,
        stdout: String(stdout == null ? '' : stdout),
        stderr: String(stderr == null ? '' : stderr)
      });
    };
  }

  NativeShell.prototype.exec = function (command, options) {
    var self = this;
    if (!command || typeof command !== 'string') {
      return Promise.reject(ShellError('命令为空', ''));
    }
    if (!self.bridge || typeof self.bridge.exec !== 'function') {
      return Promise.reject(ShellError('原生通道不可用', command));
    }
    var task = self.queue.then(function () {
      return self._execOne(command, options);
    }, function () {
      return self._execOne(command, options);
    });
    self.queue = task.then(function () { return null; }, function () { return null; });
    return task;
  };

  NativeShell.prototype._execOne = function (command, options) {
    var self = this;
    var timeoutMs = (options && options.timeoutMs) || 10000;
    return new Promise(function (resolve, reject) {
      var id = 'c' + (self.seq += 1) + '_' + Date.now();
      var timer = global.setTimeout(function () {
        if (!self.pending[id]) return;
        delete self.pending[id];
        reject(ShellError(timeoutText(timeoutMs), command));
      }, timeoutMs);
      self.pending[id] = { resolve: resolve, timer: timer };
      try {
        self.bridge.exec(String(command), id);
      } catch (error) {
        delete self.pending[id];
        global.clearTimeout(timer);
        reject(ShellError('原生通道调用失败：' + String((error && error.message) || error), command));
      }
    });
  };

  /** 应用元数据走异步回调，不阻塞 WebView 的桥接线程。 */
  NativeShell.prototype.apps = function (force) {
    var self = this;
    if (!self.bridge || typeof self.bridge.apps !== 'function') return Promise.resolve(null);
    return new Promise(function (resolve) {
      var id = 'apps' + (self.seq += 1) + '_' + Date.now();
      var timer = global.setTimeout(function () {
        delete self.pending[id];
        resolve({ apps: [], error: '读取应用列表超时，请重试' });
      }, 30000);
      self.pending[id] = { timer: timer, resolve: function (result) {
        try {
          var parsed = global.JSON.parse(result.stdout);
          resolve({ apps: Array.isArray(parsed.apps) ? parsed.apps : [], error: parsed.error || null });
        } catch (error) {
          resolve({ apps: [], error: '应用列表返回格式无效' });
        }
      } };
      try {
        self.bridge.apps(id, !!force);
      } catch (error) {
        delete self.pending[id];
        global.clearTimeout(timer);
        resolve({ apps: [], error: '读取应用列表失败：' + String(error.message || error) });
      }
    });
  };

  /**
   * 演示 shell：不访问设备，按命令特征返回内置 babylon 样例。
   * 走的是和真机完全相同的解析路径，便于离线评审。
   */
  function DemoShell(mock) {
    this.kind = 'demo';
    this.mock = mock;
  }

  DemoShell.prototype.exec = function (command) {
    var mock = this.mock;
    return new Promise(function (resolve) {
      global.setTimeout(function () {
        if (command.indexOf('__AEGIS_SPLIT__') !== -1) {
          resolve({
            code: 0,
            stdout: mock.cpuStatA.join('\n') + '\n__AEGIS_SPLIT__\n' +
              mock.cpuStatB.join('\n') + '\n__AEGIS_WINDOW__ 100.000 100.500\n',
            stderr: ''
          });
          return;
        }
        if (command.indexOf("<<'AEGIS_PLAN_EOF'") !== -1) {
          var planStart = command.indexOf('\n') + 1;
          var planEnd = command.indexOf('AEGIS_PLAN_EOF', planStart);
          mock.chargePlanJson = command.slice(planStart, planEnd);
          resolve({ code: 0, stdout: '__PLAN_OK__\n', stderr: '' });
          return;
        }
        if (command.indexOf('charge_plan.json') !== -1) {
          resolve({ code: 0, stdout: mock.chargePlanJson || '__EMPTY__', stderr: '' });
          return;
        }
        if (command.indexOf("<<'AEGIS_FAS_EOF'") !== -1) {
          var fasStart = command.indexOf('\n') + 1;
          var fasEnd = command.indexOf('AEGIS_FAS_EOF', fasStart);
          mock.fasJson = command.slice(fasStart, fasEnd);
          resolve({ code: 0, stdout: mock.fasJson, stderr: '' });
          return;
        }
        if (command.indexOf('fas.json') !== -1) {
          resolve({ code: 0, stdout: mock.fasJson || '__EMPTY__', stderr: '' });
          return;
        }
        if (command.indexOf("<<'AEGIS_CONFIG_EOF'") !== -1) {
          var start = command.indexOf('\n') + 1;
          var end = command.indexOf('AEGIS_CONFIG_EOF', start);
          mock.configDraft = command.slice(start, end);
          resolve({ code: 0, stdout: '__WRITE_OK__\n', stderr: '' });
          return;
        }
        if (command.indexOf('__COMMIT_OK__') !== -1) {
          mock.configBak = mock.configToml;
          if (mock.configDraft) mock.configToml = mock.configDraft;
          resolve({ code: 0, stdout: '__COMMIT_OK__\n', stderr: '' });
          return;
        }
        if (command.indexOf('__RESTORE_OK__') !== -1) {
          if (mock.configBak) {
            mock.configToml = mock.configBak;
            resolve({ code: 0, stdout: '__RESTORE_OK__\n', stderr: '' });
          } else {
            resolve({ code: 0, stdout: '__RESTORE_MISSING__\n', stderr: '' });
          }
          return;
        }
        if (/validate 2>&1$/.test(command)) {
          resolve({
            code: 0,
            stdout: JSON.stringify({ ok: true, config: 1, profile: 1, clusters: 3, platform: 'kalama' }),
            stderr: ''
          });
          return;
        }
        if (/ config 2>&1$/.test(command)) {
          resolve({ code: 0, stdout: mock.configToml, stderr: '' });
          return;
        }
        if (command.indexOf('module.prop') !== -1) {
          resolve({ code: 0, stdout: mock.moduleProp, stderr: '' });
          return;
        }
        if (command.indexOf('scaling_cur_freq') !== -1) {
          resolve({ code: 0, stdout: mock.freqLines.join('\n') + '\n', stderr: '' });
          return;
        }
        if (command.indexOf('.log') !== -1) {
          resolve({
            code: 0,
            stdout: '__FILE__ /data/adb/tempo/Aegis.log\n' + mock.logLines.join('\n') + '\n',
            stderr: ''
          });
          return;
        }
        if (command.indexOf('list packages') !== -1) {
          resolve({
            code: 0,
            stdout: mock.apps.map(function (app) {
              return '__APP__ ' + app.package + '|' + app.name;
            }).join('\n') + '\n',
            stderr: ''
          });
          return;
        }
        if (command.indexOf('power --socket') !== -1) {
          resolve({ code: 0, stdout: JSON.stringify(mock.power), stderr: '' });
          return;
        }
        if (command.indexOf('status --socket') !== -1) {
          resolve({ code: 0, stdout: JSON.stringify(mock.status), stderr: '' });
          return;
        }
        if (command.indexOf('command -v tempoctl') !== -1) {
          resolve({ code: 0, stdout: '/data/adb/modules/aegis_scheduler/bin/tempoctl\n', stderr: '' });
          return;
        }
        resolve({ code: 0, stdout: '', stderr: '' });
      }, 90);
    });
  };

  Tempo.Shell = {
    create: function () {
      // 显式演示：?demo=1
      var forcedDemo = /(?:^|[?&])demo=1(?:&|$)/.test(global.location.search || '');
      if (!forcedDemo) {
        if (global.AegisShell && typeof global.AegisShell.exec === 'function') {
          return new NativeShell(global.AegisShell);
        }
        if (global.ksu) {
          // 即使 exec 缺失也返回 KsuShell，让页面显示“控制通道不可用”，
          // 而不是静默回退到演示数据。
          return new KsuShell(global.ksu);
        }
        if (typeof global.__TEMPO_SHELL_FACTORY__ === 'function') {
          return global.__TEMPO_SHELL_FACTORY__();
        }
      }
      if (forcedDemo) return new DemoShell(Tempo.Mock);
      return {
        kind: 'unavailable',
        exec: function () { return Promise.reject(new Error('控制通道不可用，请在已授权的 Aegis 或 KernelSU 中打开')); }
      };
    },
    KsuShell: KsuShell,
    NativeShell: NativeShell,
    DemoShell: DemoShell
  };
})(window);
