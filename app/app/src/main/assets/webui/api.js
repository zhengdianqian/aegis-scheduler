/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (C) 2026 默默的阿文 */

/*
 * api.js —— 与守护/模块交互的唯一入口。
 * 所有命令、路径与解析规则来自 webui/CONTRACT.md。
 */
(function (global) {
  'use strict';

  var Tempo = (global.Tempo = global.Tempo || {});

  var PATHS = {
    config: '/data/adb/tempo/config.toml',
    configNew: '/data/adb/tempo/config.toml.new',
    configBak: '/data/adb/tempo/config.toml.bak',
    fas: '/data/adb/tempo/fas.json',
    chargePlan: '/data/adb/tempo/charge_plan.json',
    profile: '/data/adb/tempo/profile/babylon.toml',
    socket: '/data/adb/tempo/run/control.sock',
    logdir: '/data/adb/tempo/log',
    moduleProp: '/data/adb/modules/tempo_sched/module.prop',
    moduleDir: '/data/adb/modules/tempo_sched'
  };

  var BIN_CANDIDATES = [
    '/data/adb/modules/tempo_sched/bin/tempoctl',
    '/data/adb/modules/tempo_sched/tempoctl',
    '/data/adb/tempo/bin/tempoctl',
    '/data/adb/tempo/tempoctl'
  ];

  function text(value) {
    return String(value == null ? '' : value);
  }

  function firstLine(value) {
    return text(value).split(/\r?\n/)[0].trim();
  }

  function shorten(value, limit) {
    var flat = text(value).replace(/\s+/g, ' ').trim();
    return flat.length > limit ? flat.slice(0, limit) + '…' : flat;
  }

  function parseJsonObject(output) {
    var raw = text(output);
    var start = raw.indexOf('{');
    var end = raw.lastIndexOf('}');
    if (start === -1 || end === -1 || end <= start) {
      throw new Error('响应不是 JSON');
    }
    return JSON.parse(raw.slice(start, end + 1));
  }

  function TempoApi(shell) {
    this.shell = shell;
    this.binary = null;
    this.binaryError = null;
  }

  TempoApi.prototype.kind = function () {
    return this.shell ? this.shell.kind : 'unknown';
  };

  TempoApi.prototype._exec = function (command, timeoutMs) {
    if (!this.shell) {
      return Promise.reject(new Error('没有可用的 shell 通道'));
    }
    return this.shell.exec(command, { timeoutMs: timeoutMs || 10000 });
  };

  TempoApi.prototype._combined = function (result) {
    return [
      text(result.stderr).trim(),
      text(result.stdout).trim()
    ].filter(Boolean).join(' ');
  };

  /** 依次探测模块内与 PATH 中的 tempoctl，只执行一次。 */
  TempoApi.prototype.resolveBinary = function () {
    var self = this;
    if (self.binary) return Promise.resolve(self.binary);
    if (self.binaryError) return Promise.reject(self.binaryError);

    var probes = BIN_CANDIDATES.map(function (path) {
      return '[ -x "' + path + '" ] && echo "' + path + '"';
    }).join('; ');

    var command = probes + '; command -v tempoctl 2>/dev/null';
    return self._exec(command).then(function (result) {
      var found = firstLine(result.stdout);
      if (!found) {
        self.binaryError = new Error('控制服务未就绪');
        throw self.binaryError;
      }
      self.binary = found;
      return found;
    });
  };

  /** 读取守护状态。失败返回 null，由调用方汇总错误。 */
  TempoApi.prototype.queryStatus = function (binary) {
    var self = this;
    if (!binary) {
      return Promise.resolve(null);
    }
    var command = [
      '"' + binary + '"',
      '--config', PATHS.config,
      '--profile', PATHS.profile,
      'status',
      '--socket', PATHS.socket,
      '2>&1'
    ].join(' ');

    return self._exec(command).then(function (result) {
      var status = parseJsonObject(result.stdout);
      if (!status || status.ok !== true) {
        throw new Error('守护返回失败：' + shorten(result.stdout, 160));
      }
      return status;
    }).catch(function (error) {
      return { error: error };
    }).then(function (value) {
      if (value && value.error) {
        return { status: null, error: shorten(value.error.message, 200) };
      }
      return { status: value, error: null };
    });
  };

  /** 读取一次实时电源快照，仅在充电页可见时使用。 */
  TempoApi.prototype.queryPower = function (binary) {
    var self = this;
    if (!binary) {
      return Promise.resolve({ snapshot: null, error: '控制服务未就绪' });
    }
    var command = [
      '"' + binary + '"',
      '--config', PATHS.config,
      '--profile', PATHS.profile,
      'power',
      '--socket', PATHS.socket,
      '2>&1'
    ].join(' ');

    return self._exec(command, 6000).then(function (result) {
      var snapshot = parseJsonObject(result.stdout);
      if (!snapshot || snapshot.ok !== true) {
        throw new Error('电源快照返回失败：' + shorten(result.stdout, 160));
      }
      return { snapshot: snapshot, error: null };
    }).catch(function (error) {
      return { snapshot: null, error: shorten(error.message, 160) };
    });
  };

  /** 读取每簇实际频率（用户刷新时读一次，不做轮询）。 */
  TempoApi.prototype.queryFrequencies = function () {
    var command =
      'for d in /sys/devices/system/cpu/cpufreq/policy*; do ' +
      '[ -d "$d" ] || continue; ' +
      'n=${d##*policy}; ' +
      '[ -r "$d/scaling_cur_freq" ] && echo "$n $(cat "$d/scaling_cur_freq")"; ' +
      'done | sort -n';

    return this._exec(command, 6000).then(function (result) {
      var rows = text(result.stdout).split(/\r?\n/);
      var pairs = [];
      rows.forEach(function (row) {
        var line = row.trim();
        if (!line) return;
        var parts = line.split(/\s+/);
        if (parts.length >= 2) {
          var policy = Number(parts[0]);
          var freq = Number(parts[1]);
          if (isFinite(policy) && isFinite(freq) && freq > 0) {
            pairs.push({ policy: policy, khz: freq });
          }
        }
      });
      pairs.sort(function (a, b) { return a.policy - b.policy; });
      return pairs.map(function (pair) { return pair.khz; });
    }).catch(function () {
      return [];
    });
  };

  /** 读取模块元数据，并判断是否被停用。 */
  TempoApi.prototype.queryModule = function () {
    var command =
      'cat "' + PATHS.moduleProp + '" 2>/dev/null; ' +
      '[ -f "' + PATHS.moduleDir + '/disable" ] && echo "__DISABLED__"';

    return this._exec(command, 6000).then(function (result) {
      var lines = text(result.stdout).split(/\r?\n/);
      var info = { disabled: false };
      lines.forEach(function (row) {
        var line = row.trim();
        if (!line) return;
        if (line === '__DISABLED__') {
          info.disabled = true;
          return;
        }
        var eq = line.indexOf('=');
        if (eq <= 0) return;
        info[line.slice(0, eq).trim()] = line.slice(eq + 1).trim();
      });
      if (!info.id) return null;
      return info;
    }).catch(function () {
      return null;
    });
  };

  /**
   * 读取已安装应用（用户应用 + 系统应用）。
   *
   * APP 内优先走 PackageManager（有真实名称与图标）；
   * KernelSU WebUI 里退回到 pm 列表，并带 user/system 分类。
   */
  TempoApi.prototype.queryPackages = function () {
    var self = this;
    if (this.shell && typeof this.shell.apps === 'function') {
      return this.shell.apps().then(function (result) {
        return result || { apps: [], error: null };
      });
    }
    var readList = function (flag, tag) {
      return '(pm list packages ' + flag + ' 2>/dev/null || cmd package list packages ' + flag + ' 2>/dev/null) | ' +
        'while read line; do echo "' + tag + ' ${line#package:}"; done; ';
    };
    var command =
      'command -v pm >/dev/null 2>&1 || command -v cmd >/dev/null 2>&1 || echo __NO_PM__; ' +
      readList('-3', '__USER__') +
      readList('-s', '__SYSTEM__');

    return self._exec(command, 12000).then(function (result) {
      var apps = [];
      var seen = {};
      var noPm = false;
      text(result.stdout).split(/\r?\n/).forEach(function (row) {
        var line = row.trim();
        if (!line) return;
        if (line === '__NO_PM__') {
          noPm = true;
          return;
        }
        var type = 'user';
        var name = null;
        if (line.indexOf('__USER__ ') === 0) {
          line = line.slice(9).trim();
        } else if (line.indexOf('__SYSTEM__ ') === 0) {
          type = 'system';
          line = line.slice(11).trim();
        } else if (line.indexOf('__APP__ ') === 0) {
          line = line.slice(8).trim();
          var bar = line.indexOf('|');
          if (bar > 0) {
            name = line.slice(bar + 1).trim();
            line = line.slice(0, bar).trim();
          }
        } else if (line.indexOf('package:') === 0) {
          line = line.slice(8).trim();
        }
        if (!line || seen[line]) return;
        seen[line] = true;
        apps.push({ package: line, name: name, type: type });
      });
      if (noPm && !apps.length) {
        return { apps: [], error: '当前环境无法读取应用列表（缺少 pm 命令）' };
      }
      if (!apps.length && result.code !== 0) {
        return { apps: [], error: shorten(result.stderr || '读取应用列表失败', 160) };
      }
      apps.sort(function (a, b) {
        if (a.type !== b.type) return a.type === 'user' ? -1 : 1;
        return (a.name || a.package).localeCompare(b.name || b.package);
      });
      return { apps: apps.slice(0, 800), error: null };
    }).catch(function (error) {
      return { apps: [], error: shorten(error.message, 160) };
    });
  };

  /** 读取 FAS 侧车文件；不存在时返回关闭 + 空列表。 */
  TempoApi.prototype.loadFas = function () {
    var self = this;
    var command = 'cat "' + PATHS.fas + '" 2>/dev/null || echo __EMPTY__';

    return self._exec(command, 5000).then(function (result) {
      var output = text(result.stdout).trim();
      if (!output || output === '__EMPTY__') {
        return { enabled: false, packages: [], error: null };
      }
      var parsed = null;
      try {
        parsed = parseJsonObject(output);
      } catch (error) {
        parsed = null;
      }
      if (!parsed || typeof parsed !== 'object') {
        return { enabled: false, packages: [], error: '应用开关数据格式无效' };
      }
      var packages = Array.isArray(parsed.packages)
        ? parsed.packages.filter(function (item) { return typeof item === 'string' && item; })
        : [];
      return { enabled: parsed.enabled === true, packages: packages, error: null };
    }).catch(function (error) {
      return { enabled: false, packages: [], error: shorten(error.message, 160) };
    });
  };

  /** 写入 FAS 侧车文件；守护接入后可直接读取该文件生效。 */
  TempoApi.prototype.saveFas = function (fas) {
    var body = JSON.stringify({
      enabled: !!fas.enabled,
      packages: (fas.packages || []).slice().sort()
    }, null, 2) + '\n';
    if (/^AEGIS_FAS_EOF$/m.test(body)) {
      return Promise.resolve({ ok: false, error: '内容包含保留结束标记' });
    }
    var command =
      'cat > "' + PATHS.fas + '" <<\'AEGIS_FAS_EOF\'\n' +
      body +
      'AEGIS_FAS_EOF\n' +
      '[ -s "' + PATHS.fas + '" ] && echo __FAS_OK__ || echo __FAS_EMPTY__';

    return this._exec(command, 6000).then(function (result) {
      if (result.code !== 0 || text(result.stdout).indexOf('__FAS_OK__') === -1) {
        return { ok: false, error: shorten(result.stderr || result.stdout, 160) || '写入失败' };
      }
      return { ok: true, error: null };
    }).catch(function (error) {
      return { ok: false, error: shorten(error.message, 160) };
    });
  };

  /** 读取预约充电计划；不存在时返回空模型，交由页面使用默认时段。 */
  TempoApi.prototype.loadChargePlan = function () {
    var command = 'cat "' + PATHS.chargePlan + '" 2>/dev/null || echo __EMPTY__';

    return this._exec(command, 5000).then(function (result) {
      var output = text(result.stdout).trim();
      if (!output || output === '__EMPTY__') {
        return { model: null, error: null };
      }
      var parsed = null;
      try {
        parsed = parseJsonObject(output);
      } catch (error) {
        parsed = null;
      }
      if (!parsed || typeof parsed !== 'object') {
        return { model: null, error: '预约设置格式无效' };
      }
      return { model: parsed, error: null };
    }).catch(function (error) {
      return { model: null, error: shorten(error.message, 160) };
    });
  };

  /** 写入预约充电计划，记录每周可充电时段。 */
  TempoApi.prototype.saveChargePlan = function (plan) {
    var body = JSON.stringify({
      schema: 1,
      enabled: !!plan.enabled,
      mode: plan.mode,
      weekdays: (plan.weekdays || []).slice().sort(),
      windows: (plan.windows || []).map(function (win) {
        return { start: win.start, end: win.end };
      }),
      updated_ms: Date.now()
    }, null, 2) + '\n';
    if (/^AEGIS_PLAN_EOF$/m.test(body)) {
      return Promise.resolve({ ok: false, error: '内容包含保留结束标记' });
    }
    var command =
      'cat > "' + PATHS.chargePlan + '" <<\'AEGIS_PLAN_EOF\'\n' +
      body +
      'AEGIS_PLAN_EOF\n' +
      '[ -s "' + PATHS.chargePlan + '" ] && echo __PLAN_OK__ || echo __PLAN_EMPTY__';

    return this._exec(command, 6000).then(function (result) {
      if (result.code !== 0 || text(result.stdout).indexOf('__PLAN_OK__') === -1) {
        return { ok: false, error: shorten(result.stderr || result.stdout, 160) || '写入失败' };
      }
      return { ok: true, error: null };
    }).catch(function (error) {
      return { ok: false, error: shorten(error.message, 160) };
    });
  };

  /** 读取日志尾部。目录或文件缺失时返回空，不伪造内容。 */
  TempoApi.prototype.queryLogs = function (limit) {
    var max = limit || 200;
    var command =
      'd=/data/adb/tempo; f=$(ls -1t "$d"/*.log 2>/dev/null | head -1); ' +
      'if [ -n "$f" ]; then echo "__FILE__ $f"; tail -n ' + max + ' "$d/$f"; ' +
      'else echo "__EMPTY__"; fi';

    return this._exec(command, 6000).then(function (result) {
      var rows = text(result.stdout).split(/\r?\n/);
      var file = null;
      var lines = [];
      rows.forEach(function (row) {
        var line = row.replace(/\r$/, '');
        if (line.indexOf('__FILE__ ') === 0) {
          file = line.slice(9).trim();
          return;
        }
        if (line === '__EMPTY__' || !line.trim()) return;
        lines.push(line);
      });
      return { file: file, lines: lines.slice(-max), error: null };
    }).catch(function (error) {
      return { file: null, lines: [], error: shorten(error.message, 160) };
    });
  };

  /**
   * 读取当前配置。走 tempoctl 的 config 子命令，
   * 与守护 config.get 读到的是同一份文件，且会先经过同一套校验。
   */
  TempoApi.prototype.loadConfig = function (binary) {
    var self = this;
    if (!binary) {
      return Promise.resolve({ toml: null, model: null, error: '控制服务未就绪' });
    }
    var command = [
      '"' + binary + '"',
      '--config', '"' + PATHS.config + '"',
      '--profile', '"' + PATHS.profile + '"',
      'config 2>&1'
    ].join(' ');

    return self._exec(command, 8000).then(function (result) {
      var output = text(result.stdout);
      if (result.code !== 0 || !output.trim()) {
        throw new Error(firstLine(output) || ('读取配置失败（退出码 ' + result.code + '）'));
      }
      return {
        toml: output,
        model: Tempo.Toml.normalize(Tempo.Toml.parse(output)),
        error: null
      };
    }).catch(function (error) {
      return { toml: null, model: null, error: shorten(error.message, 240) };
    });
  };

  /** 校验指定配置文件；失败时返回 tempoctl 的具体原因。 */
  TempoApi.prototype.validateConfig = function (binary, path) {
    var self = this;
    var target = path || PATHS.configNew;
    var command = [
      '"' + binary + '"',
      '--config', '"' + target + '"',
      '--profile', '"' + PATHS.profile + '"',
      'validate 2>&1'
    ].join(' ');

    return self._exec(command, 8000).then(function (result) {
      var output = text(result.stdout);
      var parsed = null;
      try {
        parsed = parseJsonObject(output);
      } catch (ignored) {
        parsed = null;
      }
      if (result.code === 0 && parsed && parsed.ok === true) {
        return { ok: true, detail: null, info: parsed };
      }
      var detail = parsed && parsed.detail ? parsed.detail : (output.trim() || '校验失败');
      return { ok: false, detail: shorten(detail, 280), info: parsed };
    }).catch(function (error) {
      return { ok: false, detail: shorten(error.message, 280), info: null };
    });
  };

  /** 把完整 TOML 写到 config.toml.new（不触碰当前生效配置）。 */
  TempoApi.prototype.writeConfigDraft = function (toml) {
    var body = text(toml).replace(/\r\n/g, '\n');
    if (body.charAt(body.length - 1) !== '\n') body += '\n';
    if (/^AEGIS_CONFIG_EOF$/m.test(body)) {
      return Promise.reject(new Error('配置内容包含保留结束标记，已中止'));
    }
    var command =
      'cat > "' + PATHS.configNew + '" <<\'AEGIS_CONFIG_EOF\'\n' +
      body +
      'AEGIS_CONFIG_EOF\n' +
      '[ -s "' + PATHS.configNew + '" ] && echo __WRITE_OK__ || echo __WRITE_EMPTY__';

    return this._exec(command, 8000).then(function (result) {
      if (result.code !== 0 || text(result.stdout).indexOf('__WRITE_OK__') === -1) {
        throw new Error('写入草稿失败：' + shorten(result.stderr || result.stdout, 200));
      }
      return true;
    });
  };

  /** 备份当前配置并原子替换为草稿。 */
  TempoApi.prototype.commitConfig = function () {
    var command =
      '[ -f "' + PATHS.config + '" ] && cp -f "' + PATHS.config + '" "' + PATHS.configBak + '" 2>/dev/null; ' +
      'mv -f "' + PATHS.configNew + '" "' + PATHS.config + '" && echo __COMMIT_OK__ || echo __COMMIT_FAIL__';

    return this._exec(command, 8000).then(function (result) {
      if (text(result.stdout).indexOf('__COMMIT_OK__') === -1) {
        throw new Error('替换配置失败：' + shorten(result.stderr || result.stdout, 200));
      }
      return true;
    });
  };

  /** 恢复上一份备份配置。 */
  TempoApi.prototype.restoreConfig = function () {
    var command =
      'if [ ! -f "' + PATHS.configBak + '" ]; then echo __RESTORE_MISSING__; exit 0; fi; ' +
      'cp -f "' + PATHS.configBak + '" "' + PATHS.config + '" && echo __RESTORE_OK__ || echo __RESTORE_FAIL__';

    return this._exec(command, 8000).then(function (result) {
      var output = text(result.stdout);
      if (output.indexOf('__RESTORE_OK__') !== -1) return { ok: true, detail: null };
      if (output.indexOf('__RESTORE_MISSING__') !== -1) {
        return { ok: false, detail: '没有可恢复的上一份设置' };
      }
      return { ok: false, detail: shorten(result.stderr || output, 200) || '恢复失败' };
    }).catch(function (error) {
      return { ok: false, detail: shorten(error.message, 200) };
    });
  };

  /**
   * 完整保存流程：写草稿 → tempoctl validate → 备份 → 原子替换 → 回读。
   * onStep 依次收到 'write' / 'validate' / 'commit' / 'readback'。
   */
  TempoApi.prototype.applyConfig = function (binary, model, onStep) {
    var self = this;
    var notify = function (step) {
      if (typeof onStep === 'function') onStep(step);
    };

    return self.writeConfigDraft(Tempo.Toml.stringify(model)).then(function () {
      notify('validate');
      return self.validateConfig(binary, PATHS.configNew);
    }).then(function (checked) {
      if (!checked.ok) {
        return self._exec('rm -f "' + PATHS.configNew + '"').catch(function () {
          return null;
        }).then(function () {
          return { ok: false, step: 'validate', detail: checked.detail };
        });
      }
      notify('commit');
      return self.commitConfig().then(function () {
        notify('readback');
        return self.loadConfig(binary);
      }).then(function (readback) {
        return {
          ok: true,
          step: 'readback',
          model: readback.model,
          toml: readback.toml,
          error: readback.error || null
        };
      });
    }).catch(function (error) {
      return { ok: false, step: 'write', detail: shorten(error.message, 240) };
    });
  };

  /** 总览页一次刷新需要的全部数据。 */
  TempoApi.prototype.loadOverview = function () {
    var self = this;
    var errors = [];
    var binary = null;

    return self.resolveBinary().then(function (found) {
      binary = found;
      return self.queryStatus(found);
    }).catch(function (error) {
      errors.push(shorten(error.message, 200));
      return { status: null, error: null };
    }).then(function (statusResult) {
      if (statusResult.error) {
        errors.push(statusResult.error);
      }
      return Promise.all([
        self.queryFrequencies(),
        self.queryModule()
      ]).then(function (rest) {
        return {
          source: self.kind(),
          binary: binary,
          status: statusResult.status,
          freqs: rest[0],
          module: rest[1],
          errors: errors
        };
      });
    });
  };

  Tempo.Paths = PATHS;
  Tempo.Api = TempoApi;
  Tempo.createApi = function () {
    return new TempoApi(Tempo.Shell.create());
  };
})(window);
