// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Event loop.
//!
//! Every source registers in one epoll set: the BPF probe ring, the backlight
//! inotify descriptor, the evdev touch nodes and the FAS trace pipe. There is a
//! one-shot `timerfd` for controller deadlines, plus bounded safety readbacks
//! and charging reservation/retry timers. Safety polling backs off while stable.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use tempo_core::controller::{Controller, Decision, DemandSample, FrameSample, Scene};
use tempo_core::{Config, DeviceProfile};

use crate::charge::ChargeWatcher;
use crate::fas::FasCollector;
use crate::foreground::ForegroundResolver;
use crate::ipc::{self, ControlContext, Outcome, RuntimeStatus};
use crate::launch::{self, WakePipe};
use crate::power;
use crate::probe::{self, ProbeSession};
use crate::screen::ScreenWatcher;
use crate::touch::TouchWatcher;
use crate::writer::{CpufreqWriter, ReconcileBackoff};

const LISTENER: Token = Token(0);
const DEADLINE: Token = Token(1);
const PROBE: Token = Token(2);
const SCREEN: Token = Token(3);
const FRAME: Token = Token(4);
const TOUCH_BASE: usize = 5;
const CHARGE: Token = Token(11);
const CHARGE_TIMER: Token = Token(12);
const CONFIG_WATCH: Token = Token(13);
const VERIFY: Token = Token(14);
const CEILING_TIMER: Token = Token(15);
const WAKE: Token = Token(100);
const SIGNAL: Token = Token(101);
const FIRST_CONNECTION: usize = 16;

fn write_clean_exit_marker(run_dir: &Path) -> io::Result<()> {
    let temporary = run_dir.join(".clean-exit.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(b"aegis-clean-exit-v1\nwriter_restored=1\n")?;
    file.sync_all()?;
    std::fs::rename(temporary, run_dir.join("clean-exit"))?;
    std::fs::File::open(run_dir)?.sync_all()
}

struct ShutdownSignals {
    file: std::fs::File,
    previous: libc::sigset_t,
}

impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            libc::sigaddset(&mut mask, libc::SIGINT);
            let code = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous);
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
            let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                let error = io::Error::last_os_error();
                libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                file: std::fs::File::from_raw_fd(fd),
                previous,
            })
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        // 此对象最早创建、最后析构，先让 writer 完成频率恢复。
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    config_path: &Path,
    config: Config,
    profile: DeviceProfile,
    run_dir: &Path,
    started: Instant,
    log_path: &Path,
    apply: bool,
) -> io::Result<()> {
    // 在创建工作线程之前屏蔽退出信号，由主循环接收并正常析构 writer。
    let mut signals = ShutdownSignals::new()?;
    std::fs::create_dir_all(run_dir).map_err(|error| context("create run dir", error))?;

    std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| context("chmod run dir", error))?;

    // Exactly one daemon may own the cpufreq policies. A second instance exits
    // instead of fighting over min/max (the reported "frequency anomaly").
    let instance_lock = acquire_instance_lock(run_dir)?;
    if instance_lock.is_none() {
        tracing::warn!("another Aegis instance already owns the run directory; exiting");
        return Ok(());
    }

    // 获取实例锁后才清除旧标记，第二实例不能破坏当前实例的回执。
    match std::fs::remove_file(run_dir.join("clean-exit")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(context("remove stale clean exit marker", error)),
    }
    let mut writer_restored = false;
    let socket_path = run_dir.join("control.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).map_err(|error| context("remove old socket", error))?;
    }
    let mut listener =
        UnixListener::bind(&socket_path).map_err(|error| context("bind socket", error))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| context("chmod socket", error))?;

    let mut poll = Poll::new().map_err(|error| context("create poll", error))?;
    let mut events = Events::with_capacity(64);
    use std::os::unix::io::AsRawFd;
    poll.registry().register(
        &mut mio::unix::SourceFd(&signals.file.as_raw_fd()),
        SIGNAL,
        Interest::READABLE,
    )?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .map_err(|error| context("register listener", error))?;

    let mut deadline_source =
        OneShotTimer::new().map_err(|error| context("create deadline timer", error))?;
    poll.registry()
        .register(&mut deadline_source, DEADLINE, Interest::READABLE)
        .map_err(|error| context("register deadline timer", error))?;

    // Dedicated one-shot timer for the post-start verification. Kept off the
    // scheduling deadline source so an unrelated event cannot disarm it.
    let mut verify_timer =
        OneShotTimer::new().map_err(|error| context("create verify timer", error))?;
    poll.registry()
        .register(&mut verify_timer, VERIFY, Interest::READABLE)
        .map_err(|error| context("register verify timer", error))?;

    // Slow periodic reconcile of the soft cpufreq ceiling. A vendor perf
    // service moves its own floors without generating a probe event, so the
    // static state still needs a cheap low-frequency check.
    let mut ceiling_timer =
        OneShotTimer::new().map_err(|error| context("create ceiling timer", error))?;
    poll.registry()
        .register(&mut ceiling_timer, CEILING_TIMER, Interest::READABLE)
        .map_err(|error| context("register ceiling timer", error))?;

    let status: Arc<Mutex<RuntimeStatus>> = Arc::new(Mutex::new(RuntimeStatus {
        config_generation: 1,
        ..RuntimeStatus::default()
    }));

    let mut config = config;
    let startup_config = config.clone();
    let mut controller = Controller::new(profile.clone(), config.clone());

    // 探针成功附着之前不发现、不写入 cpufreq，避免降级后遗留已写频率。
    let mut writer = None;
    let discover_writer = |status: &Arc<Mutex<RuntimeStatus>>| {
        if apply {
            match CpufreqWriter::discover(&profile) {
                Ok(writer) => {
                    let backup = writer.backup_json();
                    let _ = std::fs::write(run_dir.join("cpufreq-backup.json"), backup);
                    if let Ok(mut state) = status.lock() {
                        state.write_enabled = !writer.is_empty();
                    }
                    Some(writer)
                }
                Err(error) => {
                    if let Ok(mut state) = status.lock() {
                        state.write_enabled = false;
                        state.write_error = Some(error.clone());
                    }
                    tracing::error!(%error, "cpufreq writer unavailable; scheduling stays off");
                    None
                }
            }
        } else {
            None
        }
    };

    let mut wake = WakePipe::new().map_err(|error| context("create wake pipe", error))?;
    poll.registry()
        .register(&mut wake, WAKE, Interest::READABLE)
        .map_err(|error| context("register wake pipe", error))?;
    let wake = Arc::new(wake);
    let launch_events = launch::spawn(Arc::clone(&wake));

    let mut screen = match ScreenWatcher::open(&config.events.screen_path) {
        Ok(mut watcher) => {
            poll.registry()
                .register(&mut watcher, SCREEN, Interest::READABLE)
                .map_err(|error| context("register screen watcher", error))?;
            tracing::info!(
                device = %watcher.device().display(),
                on = watcher.is_on(),
                "display state source attached"
            );
            Some(watcher)
        }
        Err(error) => {
            tracing::warn!(%error, "display state source unavailable; assuming screen on");
            None
        }
    };
    let mut screen_on = screen.as_ref().map(ScreenWatcher::is_on).unwrap_or(true);
    let screen_decisions = controller.on_screen(now_us(), screen_on);
    commit(&screen_decisions, &mut writer, &status);
    let mut ceiling_backoff = ReconcileBackoff::new();
    ceiling_timer.arm(now_us() + ceiling_backoff.reset());
    if let Ok(mut state) = status.lock() {
        state.screen_on = screen_on;
        state.screen_source = match screen.as_ref() {
            Some(watcher) => watcher.device().display().to_string(),
            None => "unavailable".to_owned(),
        };
    }

    let mut probe = match ProbeSession::open(&profile, &config) {
        Ok(mut session) => {
            poll.registry()
                .register(&mut session, PROBE, Interest::READABLE)
                .map_err(|error| context("register probe ring", error))?;
            if let Err(error) = session.set_screen(screen_on) {
                tracing::warn!(%error, "screen gate could not be applied");
            }
            if let Ok(mut state) = status.lock() {
                state.probe = "ready".to_owned();
                state.cpu_count = session.cpu_count();
                state.foreground_pids = session.foreground_count();
                state.foreground_error = session.foreground_error().map(str::to_owned);
            }
            tracing::info!(cpu_count = session.cpu_count(), "runtime probe attached");
            Some(session)
        }
        Err(error) => {
            tracing::error!(%error, "runtime probe disabled; scheduling stays off");
            if let Ok(mut state) = status.lock() {
                state.probe = "disabled".to_owned();
                state.probe_error = Some(error);
            }
            None
        }
    };

    if probe.is_some() {
        writer = discover_writer(&status);
        if let Some(writer) = writer.as_mut() {
            writer.arm_guard(Duration::from_millis(config.control.startup_guard_ms));
            verify_timer.arm(now_us() + config.control.startup_guard_ms.saturating_mul(1000));
        }
    }

    let mut touches: Vec<TouchWatcher> = TouchWatcher::open_all();
    for (index, watcher) in touches.iter_mut().enumerate() {
        poll.registry()
            .register(watcher, Token(TOUCH_BASE + index), Interest::READABLE)
            .map_err(|error| context("register touch node", error))?;
        tracing::info!(device = watcher.name(), "touch source attached");
    }
    if let Ok(mut state) = status.lock() {
        state.touch_sources = touches.len();
    }

    let mut fas = match FasCollector::new(poll.registry(), FRAME) {
        Ok(collector) => {
            if let Ok(mut state) = status.lock() {
                state.fas_available = true;
                state.fas_active = false;
            }
            tracing::info!("fas frame probe ready");
            Some(collector)
        }
        Err(error) => {
            tracing::warn!(%error, "fas frame probe unavailable");
            if let Ok(mut state) = status.lock() {
                state.fas_available = false;
                state.fas_active = false;
            }
            None
        }
    };

    let fas_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("fas.json");
    let mut fas_stamp = None;
    refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path);

    // Charging control: netlink power-supply uevents drive the state machine;
    // the only timer is a one-shot reservation boundary. When no switch is
    // writable the capability is reported as false instead of silently
    // pretending to control charging.
    let data_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("/data/adb/tempo"));
    // 真机隔离调度测试显式禁用充电控制，不打开 watcher，也不触碰充电节点。
    let charge_disabled =
        std::env::var_os("AEGIS_NO_CHARGE").as_deref() == Some(std::ffi::OsStr::new("1"));
    let mut charge = if charge_disabled {
        tracing::info!("charge control explicitly disabled by AEGIS_NO_CHARGE=1");
        None
    } else {
        match ChargeWatcher::open(data_dir.join("charge_plan.json")) {
            Ok(mut watcher) => {
                poll.registry()
                    .register(&mut watcher, CHARGE, Interest::READABLE)
                    .map_err(|error| context("register charge uevents", error))?;
                let decision = watcher.update(&config, now_us());
                tracing::info!(
                    node = watcher.node().as_str(),
                    reasons = decision.reasons_joined().as_str(),
                    "charge control attached"
                );
                publish_charge(&watcher, &status);
                Some(watcher)
            }
            Err(error) => {
                tracing::warn!(%error, "charge control unavailable");
                None
            }
        }
    };
    let mut charge_timer =
        OneShotTimer::new().map_err(|error| context("create charge timer", error))?;
    poll.registry()
        .register(&mut charge_timer, CHARGE_TIMER, Interest::READABLE)
        .map_err(|error| context("register charge timer", error))?;
    arm_charge_timer(charge.as_ref(), &charge_timer);
    let mut last_charge_latched = charge
        .as_ref()
        .map(ChargeWatcher::is_latched)
        .unwrap_or(false);

    // Atomic config replacement (WebUI `mv`) is picked up without polling.
    let mut config_watch = match ConfigWatcher::open(data_dir) {
        Ok(mut watcher) => {
            poll.registry()
                .register(&mut watcher, CONFIG_WATCH, Interest::READABLE)
                .map_err(|error| context("register config watcher", error))?;
            Some(watcher)
        }
        Err(error) => {
            tracing::warn!(%error, "config watcher unavailable; config.apply still works");
            None
        }
    };

    if let Ok(mut state) = status.lock() {
        state.charge_node = charge.as_ref().map(ChargeWatcher::node);
        state.charge_stopped = charge
            .as_ref()
            .map(ChargeWatcher::is_stopped)
            .unwrap_or(false);
        state.charge_reasons = charge
            .as_ref()
            .map(ChargeWatcher::reasons)
            .unwrap_or_default();
        state.charge_plan_allowed = charge
            .as_ref()
            .map(ChargeWatcher::plan_allowed)
            .unwrap_or(true);
        state.charge_plan_enabled = charge
            .as_ref()
            .map(ChargeWatcher::plan_enabled)
            .unwrap_or(false);
        state.thermal_available = power::thermal_c().is_some();
    }

    let mut foreground = ForegroundResolver::new();
    if let Some(session) = probe.as_ref() {
        let ids = session.foreground_ids();
        foreground.resolve(&ids);
        if let Ok(mut state) = status.lock() {
            state.foreground_package = foreground.package().map(str::to_owned);
        }
    }
    refresh_fas(&config, &foreground, &mut fas, &status, screen_on);

    tracing::info!(
        path = %socket_path.display(),
        clusters = profile.clusters.len(),
        touches = touches.len(),
        fas = fas.is_some(),
        apply,
        "Aegis event loop ready"
    );

    let mut connections: HashMap<Token, Connection> = HashMap::new();
    let mut next_token = FIRST_CONNECTION;
    let mut last_thermal_us: u64 = 0;
    // 热区读取会走 QMI，静止页面下没有必要 1.5 s 采一次；安静且不热时放宽到
    // 6 s，一旦发热或出现交互立即回到 1.5 s。
    let mut thermal_interval_us: u64 = 1_500_000;
    // The probe publishes one sample per runtime quantum (200 ms by default).
    // Every sample is evaluated once; this is an event gate, not a timer.
    // Resolving the foreground package spawns a dumpsys-backed fallback and is
    // far more expensive than the probe itself. Coalesce real foreground
    // changes to at most one resolution per 2 s.
    let mut last_foreground_us: u64 = 0;
    // Last activity start consumed by the launch logic, so one launch does not
    // reopen the same window on every wake.
    let mut last_start_us: u64 = 0;
    let mut pending_start_us: Option<u64> = None;
    let mut launch_invalidated_us: u64 = 0;

    loop {
        if writer
            .as_ref()
            .is_some_and(|writer| writer.last_error.is_some())
            && ceiling_backoff.expedite()
        {
            ceiling_timer.arm(now_us() + 1_000_000);
        }
        // A signal (SIGCHLD, SIGTERM races, ptrace attach) makes epoll_wait
        // return EINTR. That is not a failure: retry instead of tearing the
        // whole scheduler down and handing cpufreq back to the vendor.
        match poll.poll(&mut events, None) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }

        for event in events.iter() {
            let token = event.token();
            if token == SIGNAL {
                let mut buffer = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
                signals.file.read_exact(&mut buffer)?;
                if let Some(writer) = writer.as_mut() {
                    if !writer_restored {
                        writer
                            .restore()
                            .map_err(|problem| io::Error::new(io::ErrorKind::Other, problem))?;
                    }
                }
                write_clean_exit_marker(run_dir)?;
                return Ok(());
            }

            if token == LISTENER {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if next_token == WAKE.0 {
                                next_token = SIGNAL.0 + 1;
                            }
                            let connection_token = Token(next_token);
                            next_token += 1;
                            poll.registry().register(
                                &mut stream,
                                connection_token,
                                Interest::READABLE,
                            )?;
                            connections.insert(
                                connection_token,
                                Connection {
                                    stream,
                                    buffer: Vec::new(),
                                },
                            );
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
                continue;
            }

            if token == PROBE {
                if let Some(session) = probe.as_mut() {
                    if controller.is_screen_on() {
                        if let Some(sample) = session.drain() {
                            // 探针现在按 200 ms 出样本；静态页面里这些样本的
                            // busy/foreground 几乎不变，状态快照不需要每拍重写。
                            if sample.foreground_changed
                                && sample.ts_us.saturating_sub(last_foreground_us) >= 2_000_000
                            {
                                last_foreground_us = sample.ts_us;
                                let ids = session.foreground_ids();
                                let previous = foreground.package().map(str::to_owned);
                                foreground.resolve(&ids);
                                let package_changed = foreground.package() != previous.as_deref();
                                if let Ok(mut state) = status.lock() {
                                    state.foreground_package =
                                        foreground.package().map(str::to_owned);
                                    state.foreground_pids = ids.len();
                                }
                                // IME overlays change the focused window without
                                // changing the app; only a real package switch
                                // earns the warm-start lift.
                                if package_changed {
                                    pending_start_us = None;
                                    launch_invalidated_us = sample.ts_us;
                                    let decisions = controller.on_foreground_changed(sample.ts_us);
                                    commit(&decisions, &mut writer, &status);
                                    match controller.next_deadline(sample.ts_us) {
                                        Some(deadline) => deadline_source.arm(deadline.at_us),
                                        None => deadline_source.disarm(),
                                    }
                                }
                                if refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path) {
                                    tracing::info!("fas configuration reloaded from sidecar");
                                }
                                refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                                if let Some(collector) = fas.as_mut() {
                                    collector
                                        .request(foreground.package().unwrap_or(""), sample.ts_us);
                                }
                                // Workload class: reading / video / IME change
                                // the floor and cap policy, not just the load.
                                let scene = scene_of(foreground.package(), &config);
                                if let Ok(mut state) = status.lock() {
                                    state.scene = scene.as_str().to_owned();
                                }
                                let scene_decisions = controller.on_scene(sample.ts_us, scene);
                                if !scene_decisions.is_empty() {
                                    commit(&scene_decisions, &mut writer, &status);
                                    match controller.next_deadline(sample.ts_us) {
                                        Some(deadline) => deadline_source.arm(deadline.at_us),
                                        None => deadline_source.disarm(),
                                    }
                                }
                            }
                            let decisions = controller.on_demand(DemandSample {
                                ts_us: sample.ts_us,
                                cluster_busy: sample.busy.clone(),
                                foreground_busy: sample.foreground.clone(),
                                foreground_active: sample.foreground_active,
                            });
                            // 负载突增交给需求换档；倍速播放和弹幕不是应用启动。
                            let quiet = controller.is_quiet();
                            // One lock for both the sample snapshot and the
                            // smoothed usage: the WebUI only needs the newest
                            // values, and a static page must not pay for a
                            // second mutex round trip per sample.
                            record(&status, Some(&sample), Some(quiet));
                            commit(&decisions, &mut writer, &status);
                            // Thermal guard is sampled on demand events only.
                            // A quiet, cool device stretches the interval to
                            // 6 s: every zone read is a QMI transaction, and
                            // 40 transactions/minute bought nothing on a
                            // static page. Heat or interaction restores 1.5 s.
                            if sample.ts_us.saturating_sub(last_thermal_us) >= thermal_interval_us {
                                last_thermal_us = sample.ts_us;
                                let temp = power::thermal_c();
                                if let Ok(mut state) = status.lock() {
                                    state.thermal_available = temp.is_some();
                                }
                                let hot = temp
                                    .map(|value| value >= config.thermal.guard_c - 3.0)
                                    .unwrap_or(false);
                                thermal_interval_us =
                                    if hot || !quiet { 1_500_000 } else { 6_000_000 };
                                let cap =
                                    temp.map(|value| thermal_cap(value, config.thermal.guard_c));
                                let decisions = controller.on_thermal_cap(sample.ts_us, cap);
                                commit(&decisions, &mut writer, &status);
                            }
                            match controller.next_deadline(sample.ts_us) {
                                Some(deadline) => deadline_source.arm(deadline.at_us),
                                None => deadline_source.disarm(),
                            }
                        }
                    } else {
                        let _ = session.drain();
                    }
                }
                continue;
            }

            if token == SCREEN {
                if let Some(watcher) = screen.as_mut() {
                    while let Some(on) = watcher.refresh() {
                        screen_on = on;
                        ceiling_timer.arm(now_us() + ceiling_backoff.reset());
                        refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path);
                        refresh_fas(&config, &foreground, &mut fas, &status, on);
                        if let Some(session) = probe.as_mut() {
                            if let Err(error) = session.set_screen(on) {
                                tracing::warn!(%error, "screen gate could not be applied");
                            }
                        }
                        let at_us = now_us();
                        pending_start_us = None;
                        launch_invalidated_us = at_us;
                        let decisions = controller.on_screen(at_us, on);
                        if let Ok(mut state) = status.lock() {
                            state.screen_on = on;
                            state.quiet = controller.is_quiet();
                            state.fas_active = false;
                        }
                        commit(&decisions, &mut writer, &status);
                        if on {
                            // Unlock animation and the first screen after wake
                            // need the vendor's boost just like a launch.
                            refresh_fas(&config, &foreground, &mut fas, &status, true);
                            if let Some(collector) = fas.as_mut() {
                                collector.request(foreground.package().unwrap_or(""), at_us);
                            }
                        }
                        match controller.next_deadline(at_us) {
                            Some(deadline) => deadline_source.arm(deadline.at_us),
                            None => deadline_source.disarm(),
                        }
                        tracing::info!(on, "display state changed");
                    }
                }
                continue;
            }

            if token == FRAME {
                refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path);
                refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                if let Some(collector) = fas.as_mut() {
                    let reports = collector.drain();
                    for report in reports {
                        if let Some(problem) = &report.error {
                            tracing::warn!(%problem, package = report.package.as_str(), "fas probe failed");
                            continue;
                        }
                        // 工作线程可能跨越关屏、禁用或切包；消费时重新核对资格。
                        if !controller.is_screen_on()
                            || !config.fas.enabled
                            || foreground.package() != Some(report.package.as_str())
                            || !config.fas.apps.iter().any(|app| app == &report.package)
                        {
                            continue;
                        }
                        let decisions = controller.on_frame(FrameSample {
                            ts_us: now_us(),
                            render_ms: report.worst_ms,
                            budget_ms: config.fas.frame_budget_ms,
                            foreground: true,
                        });
                        commit(&decisions, &mut writer, &status);
                        match controller.next_deadline(now_us()) {
                            Some(deadline) => deadline_source.arm(deadline.at_us),
                            None => deadline_source.disarm(),
                        }
                    }
                }
                continue;
            }

            if token == CHARGE {
                if let Some(watcher) = charge.as_mut() {
                    if watcher.drain() > 0 {
                        let decision = watcher.update(&config, now_us());
                        tracing::debug!(
                            action = decision.action.as_str(),
                            stopped = watcher.is_stopped(),
                            phase = watcher.phase(),
                            trickle = watcher.is_trickling(),
                            latched = watcher.is_latched(),
                            current_ma = ?watcher.last_current_ma(),
                            reasons = decision.reasons_joined().as_str(),
                            "charge state updated"
                        );
                        publish_charge(watcher, &status);
                        arm_charge_timer(charge.as_ref(), &charge_timer);
                    }
                }
                continue;
            }

            if token == CHARGE_TIMER {
                charge_timer.drain();
                if let Some(watcher) = charge.as_mut() {
                    let decision = watcher.update(&config, now_us());
                    // 涓流采样只在锁存状态翻转时留 INFO，其余降到 DEBUG：
                    // 100% 场景下每拍一条日志本身就是可测量的 I/O。
                    let latched = watcher.is_latched();
                    let level_changed = latched != last_charge_latched;
                    last_charge_latched = latched;
                    if level_changed {
                        tracing::info!(
                            action = decision.action.as_str(),
                            stopped = watcher.is_stopped(),
                            phase = watcher.phase(),
                            trickle = watcher.is_trickling(),
                            latched = watcher.is_latched(),
                            current_ma = ?watcher.last_current_ma(),
                            reasons = decision.reasons_joined().as_str(),
                            "charge trickle latched"
                        );
                    } else {
                        tracing::debug!(
                            action = decision.action.as_str(),
                            phase = watcher.phase(),
                            trickle = watcher.is_trickling(),
                            current_ma = ?watcher.last_current_ma(),
                            "charge trickle sample"
                        );
                    }
                    publish_charge(watcher, &status);
                }
                arm_charge_timer(charge.as_ref(), &charge_timer);
                continue;
            }

            if token == CONFIG_WATCH {
                let changed = config_watch
                    .as_mut()
                    .map(ConfigWatcher::poll)
                    .unwrap_or(false);
                if changed {
                    refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path);
                    refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                    match read_valid_config(config_path) {
                        Ok(Some(parsed)) if parsed != config => {
                            let restart_required = ipc::requires_restart(&startup_config, &parsed);
                            config = parsed;
                            // 主配置替换后必须再次合并侧车，即使内容未发生变化。
                            merge_fas_sidecar(&mut config, fas_stamp.as_deref());
                            let decisions = reload_controller(
                                &mut controller,
                                &profile,
                                &config,
                                &foreground,
                                screen_on,
                                &status,
                            );
                            commit(&decisions, &mut writer, &status);
                            match controller.next_deadline(now_us()) {
                                Some(deadline) => deadline_source.arm(deadline.at_us),
                                None => deadline_source.disarm(),
                            }
                            if let Ok(mut state) = status.lock() {
                                state.config_generation = state.config_generation.saturating_add(1);
                                state.restart_required = restart_required;
                            }
                            refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                            tracing::info!("configuration reloaded from disk");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                              %error,
                              "configuration change rejected; previous valid config kept"
                            );
                        }
                    }
                    // `charge_plan.json` may have changed without the main
                    // config changing; refresh the plan and re-evaluate.
                    if let Some(watcher) = charge.as_mut() {
                        let decision = watcher.update(&config, now_us());
                        tracing::info!(
                            action = decision.action.as_str(),
                            reasons = decision.reasons_joined().as_str(),
                            "charge reloaded from sidecar"
                        );
                        publish_charge(watcher, &status);
                    }
                    arm_charge_timer(charge.as_ref(), &charge_timer);
                }
                continue;
            }

            if token.0 >= TOUCH_BASE && token.0 < TOUCH_BASE + touches.len() {
                let index = token.0 - TOUCH_BASE;
                let batch = touches[index].poll();
                if !batch.is_empty() && controller.is_screen_on() {
                    let at_us = now_us();
                    if let Ok(mut state) = status.lock() {
                        state.touch_events = state.touch_events.saturating_add(1);
                    }
                    if batch.downs > 0 {
                        // A touch is the natural moment to pick up WebUI edits
                        // to the FAS sidecar; no polling, no timer.
                        if refresh_fas_sidecar(&mut config, &mut fas_stamp, &fas_path) {
                            tracing::info!("fas configuration reloaded from sidecar");
                        }
                        refresh_fas(&config, &foreground, &mut fas, &status, true);
                    }
                    if batch.downs > 0 || batch.moves > 0 {
                        // A touch is the earliest visible sign that the user
                        // needs a lift; the controller adds exactly one tier.
                        // Pick up a WebUI change to the FAS list immediately:
                        // the user toggling the switch should not have to switch
                        // apps before the probe starts.
                        refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                        if let Some(collector) = fas.as_mut() {
                            collector.request(foreground.package().unwrap_or(""), at_us);
                        }
                    }
                    // 保留同批抬起→重新按下的顺序，不能先合并按下再统一抬起。
                    for edge in batch.edges {
                        let decisions = match edge {
                            crate::touch::TouchEdge::Up => controller.on_touch_up(at_us),
                            crate::touch::TouchEdge::Down | crate::touch::TouchEdge::Move => {
                                controller.on_touch(at_us)
                            }
                        };
                        commit(&decisions, &mut writer, &status);
                    }
                    match controller.next_deadline(at_us) {
                        Some(deadline) => deadline_source.arm(deadline.at_us),
                        None => deadline_source.disarm(),
                    }
                }
                continue;
            }

            if token == VERIFY {
                verify_timer.drain();
                let at_us = now_us();
                let decisions = controller.on_deadline(at_us);
                commit(&decisions, &mut writer, &status);
                match controller.next_deadline(at_us) {
                    Some(deadline) => deadline_source.arm(deadline.at_us),
                    None => deadline_source.disarm(),
                }
                continue;
            }

            if token == CEILING_TIMER {
                ceiling_timer.drain();
                let disturbed = writer
                    .as_mut()
                    .is_some_and(|writer| writer.reconcile().unwrap_or(true));
                if let Ok(mut state) = status.lock() {
                    state.writes = writer.as_ref().map(CpufreqWriter::applied).unwrap_or(0);
                    state.cap_writes = writer.as_ref().map(CpufreqWriter::cap_writes).unwrap_or(0);
                    state.floor_writes = writer
                        .as_ref()
                        .map(CpufreqWriter::floor_writes)
                        .unwrap_or(0);
                    state.rejected_floor = writer
                        .as_ref()
                        .map(CpufreqWriter::rejected_floor)
                        .unwrap_or(0);
                    if let Some(writer) = writer.as_ref() {
                        state.write_error = writer.last_error.clone();
                    }
                }
                ceiling_timer.arm(now_us() + ceiling_backoff.observe(screen_on, disturbed));
                continue;
            }

            if token == DEADLINE {
                deadline_source.drain();
                let at_us = now_us();
                let decisions = controller.on_deadline(at_us);
                commit(&decisions, &mut writer, &status);
                match controller.next_deadline(at_us) {
                    Some(deadline) => deadline_source.arm(deadline.at_us),
                    None => deadline_source.disarm(),
                }
                continue;
            }

            if token == WAKE {
                wake.drain();
                if let Some(events) = launch_events.as_ref() {
                    let snapshot = events.snapshot();
                    let at_us = now_us();
                    if snapshot.start_us > last_start_us {
                        last_start_us = snapshot.start_us;
                        pending_start_us = None;
                        if screen_on
                            && snapshot.start_us > launch_invalidated_us
                            && at_us.saturating_sub(snapshot.start_us)
                                < config.control.launch_lift_ms * 1_000
                        {
                            pending_start_us = Some(snapshot.start_us);
                            let decisions = controller.on_app_launch(snapshot.start_us);
                            commit(&decisions, &mut writer, &status);
                        }
                    }
                    if screen_on
                        && pending_start_us == Some(snapshot.start_us)
                        && snapshot.displayed_us >= snapshot.start_us
                        && snapshot.displayed_us != 0
                    {
                        pending_start_us = None;
                        let decisions = controller.on_app_displayed(snapshot.displayed_us);
                        commit(&decisions, &mut writer, &status);
                    }
                    // Use consumption time: a delayed wake must not arm a
                    // deadline in the past or revive an expired launch window.
                    let decisions = controller.on_deadline(at_us);
                    commit(&decisions, &mut writer, &status);
                    match controller.next_deadline(at_us) {
                        Some(deadline) => deadline_source.arm(deadline.at_us),
                        None => deadline_source.disarm(),
                    }
                }
                continue;
            }

            let mut responses = Vec::new();
            let mut closed = false;

            if let Some(connection) = connections.get_mut(&token) {
                // Readiness is edge triggered, so the socket must be drained
                // until WouldBlock. A `config.apply` payload is larger than a
                // single read; stopping after the first chunk left the request
                // unfinished and the client (APP / tempoctl) waiting forever.
                let mut chunk = [0u8; 8192];
                loop {
                    match connection.stream.read(&mut chunk) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        Ok(count) => {
                            connection.buffer.extend_from_slice(&chunk[..count]);
                            while let Some(position) =
                                connection.buffer.iter().position(|byte| *byte == b'\n')
                            {
                                let line = String::from_utf8_lossy(&connection.buffer[..position])
                                    .trim()
                                    .to_owned();
                                connection.buffer.drain(..=position);
                                if line.is_empty() {
                                    continue;
                                }
                                let runtime =
                                    status.lock().map(|state| state.clone()).unwrap_or_default();
                                let mut context = ControlContext {
                                    config: &mut config,
                                    profile: &profile,
                                    runtime: &runtime,
                                    config_path,
                                    log_path,
                                    started,
                                    writer: writer.as_mut(),
                                };
                                let (text, outcome) = ipc::handle(&mut context, &line);
                                responses.push(text);
                                if outcome == Outcome::Reloaded {
                                    merge_fas_sidecar(&mut config, fas_stamp.as_deref());
                                    let decisions = reload_controller(
                                        &mut controller,
                                        &profile,
                                        &config,
                                        &foreground,
                                        screen_on,
                                        &status,
                                    );
                                    commit(&decisions, &mut writer, &status);
                                    match controller.next_deadline(now_us()) {
                                        Some(deadline) => deadline_source.arm(deadline.at_us),
                                        None => deadline_source.disarm(),
                                    }
                                    if let Ok(mut state) = status.lock() {
                                        state.config_generation =
                                            state.config_generation.saturating_add(1);
                                        state.restart_required =
                                            ipc::requires_restart(&startup_config, &config);
                                    }
                                    refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                                    if let Some(watcher) = charge.as_mut() {
                                        let decision = watcher.update(&config, now_us());
                                        tracing::info!(
                                            action = decision.action.as_str(),
                                            reasons = decision.reasons_joined().as_str(),
                                            "charge reloaded after config.apply"
                                        );
                                        publish_charge(watcher, &status);
                                    }
                                    arm_charge_timer(charge.as_ref(), &charge_timer);
                                } else if outcome == Outcome::Restored {
                                    writer_restored = true;
                                    if let Ok(mut state) = status.lock() {
                                        state.write_enabled = false;
                                        state.guard_active = false;
                                        state.write_error = None;
                                    }
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            closed = true;
                            break;
                        }
                    }
                }
            } else {
                closed = true;
            }

            if closed {
                connections.remove(&token);
                continue;
            }

            if let Some(connection) = connections.get_mut(&token) {
                for response in responses {
                    let _ = connection.stream.write_all(response.as_bytes());
                }
            }
        }
    }
}

// 侧车很小，事件到来时直接比较内容，不依赖文件系统时间戳精度。
fn refresh_fas_sidecar(config: &mut Config, stamp: &mut Option<String>, path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    if stamp.as_deref() == Some(text.as_str()) {
        return false;
    }
    if !merge_fas_sidecar(config, Some(&text)) {
        return false;
    }
    *stamp = Some(text);
    true
}

fn merge_fas_sidecar(config: &mut Config, text: Option<&str>) -> bool {
    let Some(text) = text else { return false };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let enabled = value
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let apps: Vec<String> = value
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .take(64)
                .collect()
        })
        .unwrap_or_default();
    config.fas.enabled = enabled;
    config.fas.apps = apps;
    true
}

fn refresh_fas(
    config: &Config,
    foreground: &ForegroundResolver,
    fas: &mut Option<FasCollector>,
    status: &Arc<Mutex<RuntimeStatus>>,
    screen_on: bool,
) {
    let package = foreground.package().unwrap_or("");
    let eligible = screen_on
        && config.fas.enabled
        && !config.fas.apps.is_empty()
        && config.fas.apps.iter().any(|app| app == package);
    if let Some(collector) = fas.as_mut() {
        collector.set_enabled(eligible, &config.fas);
    }
    if let Ok(mut state) = status.lock() {
        state.fas_active = eligible && fas.is_some();
        state.fas_available = fas.is_some();
    }
    tracing::debug!(package, eligible, "fas eligibility updated");
}

/// Maps the foreground package to a workload class. Unknown packages are
/// `Default`, which never lowers a floor and never caps a cluster.
pub(crate) fn scene_of(package: Option<&str>, config: &Config) -> Scene {
    let Some(package) = package else {
        return Scene::Default;
    };
    if config.scene.ime_packages.iter().any(|app| app == package) {
        Scene::Ime
    } else if config.scene.video_packages.iter().any(|app| app == package) {
        Scene::Video
    } else if config
        .scene
        .reading_packages
        .iter()
        .any(|app| app == package)
    {
        Scene::Reading
    } else {
        Scene::Default
    }
}

#[allow(clippy::too_many_arguments)]
fn reload_controller(
    controller: &mut Controller,
    profile: &DeviceProfile,
    config: &Config,
    foreground: &ForegroundResolver,
    screen_on: bool,
    status: &Arc<Mutex<RuntimeStatus>>,
) -> Vec<Decision> {
    *controller = Controller::new(profile.clone(), config.clone());
    let at_us = now_us();
    let scene = scene_of(foreground.package(), config);
    // 先恢复场景再恢复屏幕，只提交最终状态，避免短暂写入默认场景上限。
    controller.on_scene(at_us, scene);
    let decisions = controller.on_screen(at_us, screen_on);
    if let Ok(mut state) = status.lock() {
        state.scene = scene.as_str().to_owned();
        state.quiet = controller.is_quiet();
    }
    decisions
}

fn commit(
    decisions: &[Decision],
    writer: &mut Option<CpufreqWriter>,
    status: &Arc<Mutex<RuntimeStatus>>,
) {
    let bounds_changed = status.lock().ok().is_some_and(|state| {
        state.decisions.len() != decisions.len()
            || state.decisions.iter().zip(decisions).any(|(old, new)| {
                old.cluster != new.cluster
                    || old.ceiling_khz != new.ceiling_khz
                    || old.cap_khz != new.cap_khz
                    || old.idle != new.idle
            })
    });
    if let Some(writer) = writer.as_mut() {
        if bounds_changed || writer.needs_initial_apply() {
            let _ = writer.apply(decisions);
        }
    }
    if let Ok(mut state) = status.lock() {
        state.decisions = decisions.to_vec();
        state.writes = writer.as_ref().map(CpufreqWriter::applied).unwrap_or(0);
        state.cap_writes = writer.as_ref().map(CpufreqWriter::cap_writes).unwrap_or(0);
        state.floor_writes = writer
            .as_ref()
            .map(CpufreqWriter::floor_writes)
            .unwrap_or(0);
        state.rejected_floor = writer
            .as_ref()
            .map(CpufreqWriter::rejected_floor)
            .unwrap_or(0);
        state.guard_active = writer.as_ref().is_some_and(CpufreqWriter::guard_active);
        if let Some(writer) = writer.as_ref() {
            state.write_error = writer.last_error.clone();
        }
    }
}

fn record(
    status: &Arc<Mutex<RuntimeStatus>>,
    sample: Option<&probe::DemandSnapshot>,
    quiet: Option<bool>,
) {
    if let Ok(mut state) = status.lock() {
        if let Some(sample) = sample {
            state.events = sample.events;
            state.active_events = sample.active_events;
            state.busy = sample.busy.clone();
            state.foreground = sample.foreground.clone();
            state.fg_switches = sample.fg_switches.clone();
            state.switches = sample.switches.clone();
            // Fixed-window usage estimate for the WebUI, smoothed in the same
            // lock: the probe already publishes a real per-cluster ratio every
            // quantum, this only keeps a stable "sampled" value for the page.
            if state.usage.len() != sample.busy.len() {
                state.usage = sample.busy.clone();
            } else {
                for (slot, busy) in state.usage.iter_mut().zip(sample.busy.iter()) {
                    *slot += (busy.clamp(0.0, 1.0) - *slot) * 0.35;
                }
            }
            state.usage_sampled_us = sample.ts_us;
        }
        if let Some(quiet) = quiet {
            state.quiet = quiet;
        }
    }
}

/// Thermal ceiling as a fraction of the profile maximum. Vendor thermal
/// services are never stopped; this only narrows our own lift range.
fn thermal_cap(temp_c: f32, guard_c: f32) -> f32 {
    if temp_c >= guard_c + 3.0 {
        0.55
    } else if temp_c >= guard_c {
        0.75
    } else if temp_c >= guard_c - 3.0 {
        0.90
    } else {
        1.0
    }
}

pub(crate) fn now_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000 + (now.tv_nsec as u64) / 1_000
}

fn acquire_instance_lock(run_dir: &Path) -> io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let path = run_dir.join("Aegis.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)?;
    let code = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if code == 0 {
        Ok(Some(file))
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn publish_charge(watcher: &ChargeWatcher, status: &Arc<Mutex<RuntimeStatus>>) {
    if let Ok(mut state) = status.lock() {
        state.charge_stopped = watcher.is_stopped();
        state.charge_reasons = watcher.reasons();
        state.charge_plan_allowed = watcher.plan_allowed();
        state.charge_plan_enabled = watcher.plan_enabled();
        state.charge_phase = watcher.phase().to_owned();
        state.charge_trickle = watcher.is_trickling();
        state.charge_latched = watcher.is_latched();
        state.charge_current_ma = watcher.last_current_ma();
    }
}

/// 写入失败短时重试；预约复核本地时间，避免校时后仍等待旧的单调时钟期限。
fn arm_charge_timer(watcher: Option<&ChargeWatcher>, timer: &OneShotTimer) {
    let Some(watcher) = watcher else {
        timer.disarm();
        return;
    };
    let mut delay = watcher
        .next_boundary_secs()
        .map(|seconds| seconds.max(1) as u64);
    let review = if watcher.needs_retry() {
        Some(3)
    } else if watcher.is_trickling() {
        Some(3)
    } else if watcher.plan_enabled() || watcher.is_stopped() {
        Some(30)
    } else {
        None
    };
    if let Some(review) = review {
        delay = Some(delay.map_or(review, |boundary| boundary.min(review)));
    }
    match delay {
        Some(seconds) => timer.arm(now_us() + seconds * 1_000_000),
        None => timer.disarm(),
    }
}

/// Reads the config only when all sections are present. A mid-write or
/// truncated file is rejected instead of silently turning into defaults.
fn read_valid_config(path: &Path) -> Result<Option<Config>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    // Sections the console always writes. Optional tables ([fas], [ui]) fall
    // back to defaults and are not required here.
    for section in ["[daemon]", "[events]", "[control]", "[charge]", "[thermal]"] {
        if !text.contains(section) {
            return Err(format!("incomplete configuration: missing {section}"));
        }
    }
    let config: Config = toml::from_str(&text).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(Some(config))
}

/// Directory watcher for atomic WebUI saves (`mv config.toml.new config.toml`).
/// Only the two sidecars we care about are considered; the daemon log lives in
/// the same directory and would otherwise wake the loop on every log line.
struct ConfigWatcher {
    fd: libc::c_int,
}

impl ConfigWatcher {
    fn open(directory: &Path) -> io::Result<Self> {
        use std::ffi::CString;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let path = CString::new(directory.display().to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad config directory"))?;
        let watch = unsafe {
            libc::inotify_add_watch(
                fd,
                path.as_ptr(),
                libc::IN_CLOSE_WRITE
                    | libc::IN_MOVED_TO
                    | libc::IN_CREATE
                    | libc::IN_MODIFY
                    | libc::IN_DELETE,
            )
        };
        if watch < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        Ok(Self { fd })
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut buffer = [0u8; 4096];
        loop {
            let count = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if count <= 0 {
                break;
            }
            let mut offset = 0usize;
            let header = std::mem::size_of::<libc::inotify_event>();
            while offset + header <= count as usize {
                let event =
                    unsafe { &*(buffer.as_ptr().add(offset) as *const libc::inotify_event) };
                let name_start = offset + header;
                let name_end = name_start + event.len as usize;
                if name_end <= count as usize && event.len > 0 {
                    let name = String::from_utf8_lossy(&buffer[name_start..name_end]);
                    let name = name.trim_end_matches('\0');
                    if name == "config.toml" || name == "charge_plan.json" || name == "fas.json" {
                        changed = true;
                    }
                }
                offset = name_end;
            }
        }
        changed
    }
}

impl mio::event::Source for ConfigWatcher {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&self.fd))
    }
}

/// An absolute one-shot `CLOCK_MONOTONIC` timer, shared by deadline and
/// bounded safety/retry scheduling. Each caller explicitly rearms or disarms it.
struct OneShotTimer {
    fd: libc::c_int,
}

impl OneShotTimer {
    fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn arm(&self, at_us: u64) {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: (at_us / 1_000_000) as libc::time_t,
                tv_nsec: ((at_us % 1_000_000) * 1_000) as libc::c_long,
            },
        };
        unsafe {
            libc::timerfd_settime(
                self.fd,
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            );
        }
    }

    fn disarm(&self) {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        unsafe {
            libc::timerfd_settime(
                self.fd,
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            );
        }
    }

    fn drain(&self) {
        let mut count: u64 = 0;
        unsafe {
            libc::read(self.fd, &mut count as *mut u64 as *mut libc::c_void, 8);
        }
    }
}

impl mio::event::Source for OneShotTimer {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&self.fd))
    }
}

/// Prefixes an I/O error with the failing startup step so device logs identify
/// exactly which operation was refused.
fn context(step: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{step}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_detects_back_to_back_changes_and_survives_reload() {
        let path = std::env::temp_dir().join(format!("aegis-fas-{}.json", std::process::id()));
        let mut config = Config::default();
        assert!(!config.fas.enabled);
        let mut stamp = None;
        std::fs::write(&path, r#"{"enabled":true,"packages":["first"]}"#).unwrap();
        assert!(refresh_fas_sidecar(&mut config, &mut stamp, &path));
        std::fs::write(&path, r#"{"enabled":true,"packages":["other"]}"#).unwrap();
        assert!(refresh_fas_sidecar(&mut config, &mut stamp, &path));
        assert_eq!(config.fas.apps, vec!["other"]);
        assert!(!refresh_fas_sidecar(&mut config, &mut stamp, &path));
        config = Config::default();
        assert!(merge_fas_sidecar(&mut config, stamp.as_deref()));
        assert!(config.fas.enabled);
        assert_eq!(config.fas.apps, vec!["other"]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn marker_is_complete_and_atomically_replaceable() {
        let dir = std::env::temp_dir().join(format!("aegis-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_clean_exit_marker(&dir).unwrap();
        assert_eq!(
            std::fs::read(dir.join("clean-exit")).unwrap(),
            b"aegis-clean-exit-v1\nwriter_restored=1\n"
        );
        assert!(!dir.join(".clean-exit.tmp").exists());
        write_clean_exit_marker(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reload_preserves_current_screen_state() {
        let profile: DeviceProfile = serde_json::from_value(serde_json::json!({
            "schema": 1, "platform": "test", "sdk": 34,
            "cluster": [{"cpus": [0], "role": "efficiency", "min_khz": 300,
                "max_khz": 1200, "capacity": 256, "freq_khz": [300, 600, 1200]}]
        }))
        .unwrap();
        let config = Config::default();
        let mut controller = Controller::new(profile.clone(), config.clone());
        let foreground = ForegroundResolver::new();
        let status = Arc::new(Mutex::new(RuntimeStatus::default()));
        reload_controller(
            &mut controller,
            &profile,
            &config,
            &foreground,
            false,
            &status,
        );
        assert!(!controller.is_screen_on());
        reload_controller(
            &mut controller,
            &profile,
            &config,
            &foreground,
            true,
            &status,
        );
        assert!(controller.is_screen_on());
    }

    #[test]
    fn shutdown_signal_wakes_poll() {
        use std::os::unix::io::AsRawFd;
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let mut source = ShutdownSignals::new().unwrap();
            let mut poll = Poll::new().unwrap();
            poll.registry()
                .register(
                    &mut mio::unix::SourceFd(&source.file.as_raw_fd()),
                    SIGNAL,
                    Interest::READABLE,
                )
                .unwrap();
            // 仅投递给当前测试线程，避免影响并行测试和测试进程。
            assert_eq!(
                unsafe { libc::pthread_kill(libc::pthread_self(), signal) },
                0
            );
            let mut events = Events::with_capacity(4);
            poll.poll(&mut events, Some(Duration::from_secs(1)))
                .unwrap();
            assert!(events.iter().any(|event| event.token() == SIGNAL));
            let mut data = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
            source.file.read_exact(&mut data).unwrap();
        }
    }
}
