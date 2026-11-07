// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Event loop.
//!
//! Every source registers in one epoll set: the BPF probe ring, the backlight
//! inotify descriptor, the evdev touch nodes and the FAS trace pipe. There is
//! no repeating timer anywhere; the only timer is a one-shot `timerfd` armed
//! for a controller deadline and disarmed when the window closes.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token};
use tempo_core::controller::{Controller, Decision, DemandSample, FrameSample};
use tempo_core::{Config, DeviceProfile};

use crate::charge::ChargeWatcher;
use crate::fas::FasCollector;
use crate::foreground::ForegroundResolver;
use crate::ipc::{self, ControlContext, Outcome, RuntimeStatus};
use crate::power;
use crate::probe::{self, ProbeSession};
use crate::screen::ScreenWatcher;
use crate::touch::TouchWatcher;
use crate::writer::CpufreqWriter;

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
const FIRST_CONNECTION: usize = 16;

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

    let status: Arc<Mutex<RuntimeStatus>> = Arc::new(Mutex::new(RuntimeStatus {
        config_generation: 1,
        ..RuntimeStatus::default()
    }));

    let mut config = config;
    let startup_profile_path = config.daemon.profile_path.clone();
    let startup_log_level = config.daemon.log_level;
    let mut controller = Controller::new(profile.clone(), config.clone());

    // The writer is the only component that touches cpufreq. Without the
    // capability the daemon stays observe-only rather than writing blind.
    let mut writer = if apply {
        match CpufreqWriter::discover(&profile, config.control.parked_headroom) {
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
    };

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
    let screen_on = screen.as_ref().map(ScreenWatcher::is_on).unwrap_or(true);
    let screen_decisions = controller.on_screen(now_us(), screen_on);
    commit(&screen_decisions, &mut writer, &status);
    // One-shot re-assert twenty seconds after the first write. A vendor writer
    // can raise `scaling_min_freq` in the seconds after boot; re-applying the
    // ceiling once guarantees the cluster is not left at max==min until the
    // next real event. It is a single timer, never a periodic one.
    let startup_decisions = screen_decisions;
    let mut verify_retries = 0u8;
    verify_timer.arm(now_us() + 20_000_000);
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

    let mut fas_stamp: u64 = 0;
    refresh_fas_sidecar(&mut config, &mut fas_stamp);

    // Charging control: netlink power-supply uevents drive the state machine;
    // the only timer is a one-shot reservation boundary. When no switch is
    // writable the capability is reported as false instead of silently
    // pretending to control charging.
    let data_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("/data/adb/tempo"));
    let mut charge = match ChargeWatcher::open(data_dir.join("charge_plan.json")) {
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
            Some(watcher)
        }
        Err(error) => {
            tracing::warn!(%error, "charge control unavailable");
            None
        }
    };
    let mut charge_timer =
        OneShotTimer::new().map_err(|error| context("create charge timer", error))?;
    poll.registry()
        .register(&mut charge_timer, CHARGE_TIMER, Interest::READABLE)
        .map_err(|error| context("register charge timer", error))?;
    arm_charge_timer(charge.as_ref(), &charge_timer);

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
        state.charge_stopped = charge.as_ref().map(ChargeWatcher::is_stopped).unwrap_or(false);
        state.charge_reasons = charge.as_ref().map(ChargeWatcher::reasons).unwrap_or_default();
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
    // The probe publishes one sample per scheduler switch burst. Coalesce
    // bursts to at most one demand evaluation per 50 ms so a busy system
    // cannot turn the event loop into a spinning consumer; this is an event
    // gate, not a timer.
    let mut last_probe_us: u64 = 0;
    // Resolving the foreground package spawns a dumpsys-backed fallback and is
    // far more expensive than the probe itself. Coalesce real foreground
    // changes to at most one resolution per 2 s, and skip it entirely during
    // the 1.5 s after an app-start edge.
    let mut last_foreground_us: u64 = 0;
    // Debounce for the app-start edge lift: one launch boost per 800 ms.
    let mut last_launch_us: u64 = 0;

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            let token = event.token();

            if token == LISTENER {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
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
                            if sample.ts_us.saturating_sub(last_probe_us) < 50_000 {
                                continue;
                            }
                            last_probe_us = sample.ts_us;
                            if sample.foreground_changed
                                && sample.ts_us.saturating_sub(last_foreground_us) >= 2_000_000
                                // A dumpsys-backed resolution spawns a process
                                // and binds to system_server; doing that inside
                                // the first frames of an app start adds jank.
                                // The launch edge already has its own lift.
                                && sample.ts_us.saturating_sub(last_launch_us) >= 1_500_000
                            {
                                last_foreground_us = sample.ts_us;
                                let ids = session.foreground_ids();
                                foreground.resolve(&ids);
                                if let Ok(mut state) = status.lock() {
                                    state.foreground_package =
                                        foreground.package().map(str::to_owned);
                                    state.foreground_pids = ids.len();
                                }
                                let decisions = controller.on_foreground_changed(sample.ts_us);
                                commit(&decisions, &mut writer, &status);
                                match controller.next_deadline(sample.ts_us) {
                                    Some(deadline) => deadline_source.arm(deadline.at_us),
                                    None => deadline_source.disarm(),
                                }
                                if refresh_fas_sidecar(&mut config, &mut fas_stamp) {
                                    tracing::info!("fas configuration reloaded from sidecar");
                                }
                                refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                                if let Some(collector) = fas.as_mut() {
                                    collector.request(
                                        foreground.package().unwrap_or(""),
                                        sample.ts_us,
                                    );
                                }
                            }
                            let was_quiet = controller.is_quiet();
                            let mut decisions = controller.on_demand(DemandSample {
                                ts_us: sample.ts_us,
                                cluster_busy: sample.busy.clone(),
                                foreground_busy: sample.foreground.clone(),
                                foreground_active: sample.foreground_active,
                            });
                            // Application start edge: the foreground app itself
                            // jumped from idle to real work. Lift all clusters
                            // (prime included) for the short launch window so
                            // the first frames are not scheduled at minimum.
                            let fg_peak = sample
                                .foreground
                                .iter()
                                .copied()
                                .fold(0.0f32, f32::max);
                            if was_quiet
                                && sample.foreground_active
                                && fg_peak >= 0.30
                                && sample.ts_us.saturating_sub(last_launch_us) >= 800_000
                            {
                                last_launch_us = sample.ts_us;
                                decisions = controller.on_app_launch(sample.ts_us);
                                if let Ok(mut state) = status.lock() {
                                    state.launch_lifts += 1;
                                }
                                tracing::debug!(fg_peak, "app launch edge: launch lift");
                            }
                            let quiet = controller.is_quiet();
                            record(&status, &decisions, Some(&sample), Some(quiet));
                            commit(&decisions, &mut writer, &status);
                            // Thermal guard is sampled on demand events only,
                            // throttled to 1.5 s: no timer, no busy loop, and
                            // far fewer QMI sensor transactions at idle.
                            if sample.ts_us.saturating_sub(last_thermal_us) >= 1_500_000 {
                                last_thermal_us = sample.ts_us;
                                let temp = power::thermal_c();
                                if let Ok(mut state) = status.lock() {
                                    state.thermal_available = temp.is_some();
                                }
                                let cap = temp
                                    .map(|value| thermal_cap(value, config.thermal.guard_c));
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
                        if let Some(session) = probe.as_mut() {
                            if let Err(error) = session.set_screen(on) {
                                tracing::warn!(%error, "screen gate could not be applied");
                            }
                        }
                        let at_us = now_us();
                        let decisions = controller.on_screen(at_us, on);
                        if let Ok(mut state) = status.lock() {
                            state.screen_on = on;
                            state.quiet = controller.is_quiet();
                            state.fas_active = false;
                        }
                        commit(&decisions, &mut writer, &status);
                        if on {
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
                if let Some(collector) = fas.as_mut() {
                    let reports = collector.drain();
                    for report in reports {
                        if let Some(problem) = &report.error {
                            tracing::warn!(%problem, package = report.package.as_str(), "fas probe failed");
                            continue;
                        }
                        if !controller.is_screen_on()
                            || report.worst_ms <= config.fas.frame_budget_ms
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
                        tracing::info!(
                            action = decision.action.as_str(),
                            stopped = watcher.is_stopped(),
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
                    tracing::info!(
                        action = decision.action.as_str(),
                        stopped = watcher.is_stopped(),
                        reasons = decision.reasons_joined().as_str(),
                        "charge schedule boundary"
                    );
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
                    match read_valid_config(config_path) {
                        Ok(Some(parsed)) if parsed != config => {
                            let restart_required = parsed.daemon.profile_path
                                != startup_profile_path
                                || parsed.daemon.log_level != startup_log_level;
                            config = parsed;
                            if let Some(writer) = writer.as_mut() {
                                writer.set_parked_headroom(config.control.parked_headroom);
                            }
                            controller = Controller::new(profile.clone(), config.clone());
                            let decisions = controller.on_screen(now_us(), screen_on);
                            commit(&decisions, &mut writer, &status);
                            match controller.next_deadline(now_us()) {
                                Some(deadline) => deadline_source.arm(deadline.at_us),
                                None => deadline_source.disarm(),
                            }
                            if let Ok(mut state) = status.lock() {
                                state.config_generation =
                                    state.config_generation.saturating_add(1);
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
                        if refresh_fas_sidecar(&mut config, &mut fas_stamp) {
                            tracing::info!("fas configuration reloaded from sidecar");
                        }
                        refresh_fas(&config, &foreground, &mut fas, &status, true);
                    }
                    if batch.downs > 0 || batch.moves > 0 {
                        // Pick up a WebUI change to the FAS list immediately:
                        // the user toggling the switch should not have to switch
                        // apps before the probe starts.
                        refresh_fas(&config, &foreground, &mut fas, &status, screen_on);
                        if let Some(collector) = fas.as_mut() {
                            collector.request(foreground.package().unwrap_or(""), at_us);
                        }
                        // The controller owns the staged lift; the writer only
                        // publishes the resulting ceiling. No min pinning here.
                        let decisions = controller.on_touch(at_us);
                        commit(&decisions, &mut writer, &status);
                        match controller.next_deadline(at_us) {
                            Some(deadline) => deadline_source.arm(deadline.at_us),
                            None => deadline_source.disarm(),
                        }
                    }
                    if batch.ups > 0 {
                        let decisions = controller.on_touch_up(at_us);
                        commit(&decisions, &mut writer, &status);
                        match controller.next_deadline(at_us) {
                            Some(deadline) => deadline_source.arm(deadline.at_us),
                            None => deadline_source.disarm(),
                        }
                    }
                }
                continue;
            }

            if token == VERIFY {
                verify_timer.drain();
                if let Some(writer) = writer.as_mut() {
                    let _ = writer.apply(&startup_decisions);
                }
                if let Ok(mut state) = status.lock() {
                    state.writes = writer.as_ref().map(CpufreqWriter::applied).unwrap_or(0);
                }
                // A late vendor write can still park a policy on its minimum.
                // Re-check once more; this remains a bounded one-shot, not a
                // periodic poll.
                let locked = writer.as_ref().map(CpufreqWriter::is_locked).unwrap_or(false);
                if locked && verify_retries < 2 {
                    verify_retries += 1;
                    verify_timer.arm(now_us() + 20_000_000);
                }
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

            let mut responses = Vec::new();
            let mut closed = false;

            if let Some(connection) = connections.get_mut(&token) {
                let mut chunk = [0u8; 1024];
                match connection.stream.read(&mut chunk) {
                    Ok(0) => closed = true,
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
                                if let Some(writer) = writer.as_mut() {
                                    writer.set_parked_headroom(config.control.parked_headroom);
                                }
                                controller = Controller::new(profile.clone(), config.clone());
                                let decisions = controller.on_screen(now_us(), screen_on);
                                commit(&decisions, &mut writer, &status);
                                match controller.next_deadline(now_us()) {
                                    Some(deadline) => deadline_source.arm(deadline.at_us),
                                    None => deadline_source.disarm(),
                                }
                                if let Ok(mut state) = status.lock() {
                                    state.config_generation =
                                        state.config_generation.saturating_add(1);
                                    state.restart_required =
                                        config.daemon.profile_path != startup_profile_path
                                            || config.daemon.log_level != startup_log_level;
                                }
                                refresh_fas(
                                    &config,
                                    &foreground,
                                    &mut fas,
                                    &status,
                                    screen_on,
                                );
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
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => closed = true,
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

/// The WebUI owns FAS selection in `/data/adb/tempo/fas.json`. It is sampled
/// only when a foreground, touch or screen event arrives, so a UI change costs
/// one metadata read and never introduces a timer.
fn refresh_fas_sidecar(config: &mut Config, stamp: &mut u64) -> bool {
    const PATH: &str = "/data/adb/tempo/fas.json";
    let Ok(metadata) = std::fs::metadata(PATH) else {
        return false;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if modified == *stamp {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(PATH) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
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
    *stamp = modified;
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

fn commit(
    decisions: &[Decision],
    writer: &mut Option<CpufreqWriter>,
    status: &Arc<Mutex<RuntimeStatus>>,
) {
    let mut error = None;
    if let Some(writer) = writer.as_mut() {
        if let Err(problem) = writer.apply(decisions) {
            error = Some(problem);
        }
    }
    if let Ok(mut state) = status.lock() {
        state.decisions = decisions.to_vec();
        state.writes = writer.as_ref().map(CpufreqWriter::applied).unwrap_or(0);
        state.write_error = error;
    }
}

fn record(
    status: &Arc<Mutex<RuntimeStatus>>,
    decisions: &[Decision],
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
        }
        if let Some(quiet) = quiet {
            state.quiet = quiet;
        }
        state.decisions = decisions.to_vec();
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

fn now_us() -> u64 {
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
    }
}

/// Arms a single one-shot timer at the next reservation boundary. No timer is
/// pending when the plan is disabled, so an idle device has no scheduled wakeup.
fn arm_charge_timer(watcher: Option<&ChargeWatcher>, timer: &OneShotTimer) {
    match watcher.and_then(ChargeWatcher::next_boundary_secs) {
        Some(seconds) if seconds > 0 => {
            timer.arm(now_us() + seconds as u64 * 1_000_000);
        }
        _ => timer.disarm(),
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
                let event = unsafe {
                    &*(buffer.as_ptr().add(offset) as *const libc::inotify_event)
                };
                let name_start = offset + header;
                let name_end = name_start + event.len as usize;
                if name_end <= count as usize && event.len > 0 {
                    let name = String::from_utf8_lossy(&buffer[name_start..name_end]);
                    let name = name.trim_end_matches('\0');
                    if name == "config.toml" || name == "charge_plan.json" {
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

/// An absolute one-shot `CLOCK_MONOTONIC` timer. Only armed while the
/// controller has a pending window; disarmed otherwise so the loop can block.
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
