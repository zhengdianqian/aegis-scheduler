// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! FAS frame probe.
//!
//! Only the applications the user selected are ever probed, and only while one
//! of them owns the foreground. A probe is a single `dumpsys gfxinfo
//! <package> framestats` run on a detached worker thread; it is triggered by a
//! foreground transition, a touch or a screen wake, never by a timer. The
//! worker wakes the event loop through a mio `Waker`, so the scheduler keeps
//! running while the dump is produced.
//!
//! `Scored` frames below the budget are ignored; the worst frame in the sample
//! is handed to the controller only when it exceeded the budget.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tempo_core::config::FasConfig;

const DUMPSYS: &str = "/system/bin/dumpsys";
const REQUEST_INTERVAL_US: u64 = 2_000_000;
const SAMPLE_FRAMES: usize = 60;

#[derive(Debug, Clone)]
pub struct FasReport {
    pub package: String,
    pub worst_ms: f32,
    pub over_budget: usize,
    pub frames: usize,
    pub error: Option<String>,
}

#[derive(Default)]
struct ProbeState {
    pending: Option<String>,
    report: Option<FasReport>,
    generation: u64,
    enabled: bool,
    available: bool,
    closed: bool,
}

impl ProbeState {
    fn disable(&mut self) {
        self.enabled = false;
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.report = None;
        self.available = false;
    }

    fn publish(&mut self, generation: u64, report: FasReport) -> bool {
        if !self.enabled || self.closed || generation != self.generation {
            return false;
        }
        self.available = report.error.is_none();
        self.report = Some(report);
        true
    }
}

pub struct FasCollector {
    shared: Arc<(Mutex<ProbeState>, Condvar)>,
    last_request_us: u64,
    pub last_error: Option<String>,
}

impl FasCollector {
    pub fn new(registry: &mio::Registry, token: mio::Token) -> io::Result<Self> {
        let waker = mio::Waker::new(registry, token)?;
        let shared = Arc::new((Mutex::new(ProbeState::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("fas-probe".to_owned())
            .spawn(move || {
                let mut watermarks = std::collections::HashMap::new();
                let mut last_generation = 0;
                loop {
                    let (lock, ready) = &*worker;
                    let mut state = lock.lock().unwrap();
                    while state.pending.is_none() && !state.closed {
                        state = ready.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    let package = state.pending.take().unwrap();
                    let generation = state.generation;
                    drop(state);
                    if generation != last_generation {
                        watermarks.clear();
                        last_generation = generation;
                    }
                    let watermark = watermarks.entry(package.clone()).or_insert(0);
                    let report = probe(&package, watermark);
                    if lock.lock().unwrap().publish(generation, report) {
                        let _ = waker.wake();
                    }
                }
            })?;

        Ok(Self {
            shared,
            last_request_us: 0,
            last_error: None,
        })
    }

    pub fn is_available(&self) -> bool {
        self.shared.0.lock().unwrap().available
    }

    pub fn set_enabled(&mut self, enabled: bool, config: &FasConfig) {
        let mut state = self.shared.0.lock().unwrap();
        if !enabled {
            state.disable();
            self.last_request_us = 0;
        } else {
            state.enabled = true;
        }
        self.last_error = None;
        let _ = config;
    }

    pub fn request(&mut self, package: &str, at_us: u64) {
        let mut state = self.shared.0.lock().unwrap();
        if !state.enabled || package.is_empty() {
            return;
        }
        if state.pending.is_some() {
            state.pending = Some(package.to_owned());
            return;
        }
        if at_us.saturating_sub(self.last_request_us) < REQUEST_INTERVAL_US {
            return;
        }
        self.last_request_us = at_us;
        state.pending = Some(package.to_owned());
        self.shared.1.notify_one();
    }

    pub fn drain(&mut self) -> Vec<FasReport> {
        let report = self.shared.0.lock().unwrap().report.take();
        if let Some(problem) = report.as_ref().and_then(|report| report.error.clone()) {
            self.last_error = Some(problem);
        }
        report.into_iter().collect()
    }
}

impl Drop for FasCollector {
    fn drop(&mut self) {
        let mut state = self.shared.0.lock().unwrap();
        state.disable();
        state.closed = true;
        self.shared.1.notify_one();
    }
}

fn probe(package: &str, watermark: &mut u64) -> FasReport {
    let output = command_output(
        Command::new(DUMPSYS).args(["-t", "2", "gfxinfo", package, "framestats"]),
        Duration::from_secs(3),
    );
    match output {
        Ok(output) => parse_new_framestats(package, &String::from_utf8_lossy(&output), watermark),
        Err(error) => FasReport {
            package: package.to_owned(),
            worst_ms: 0.0,
            over_budget: 0,
            frames: 0,
            error: Some(format!("dumpsys: {error}")),
        },
    }
}

fn command_output(command: &mut Command, timeout: Duration) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().unwrap();
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut eof = false;
        loop {
            // Bound each drain so even a continuously writing child hits the deadline.
            for _ in 0..8 {
                let mut buffer = [0u8; 8192];
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!("exit status {status}")));
                }
                if eof {
                    return Ok(output);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "probe timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // Do not wait on a pipe reader: inherited stdout descriptors cannot hold us up.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Parses the CSV block of `framestats`. Columns 1 and 13 are the intended
/// vsync and the frame completion timestamp in nanoseconds.
pub fn parse_framestats(package: &str, text: &str) -> FasReport {
    parse_new_framestats(package, text, &mut 0)
}

fn parse_new_framestats(package: &str, text: &str, watermark: &mut u64) -> FasReport {
    let previous = *watermark;
    let mut sample: Vec<f32> = Vec::new();
    let mut header_seen = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !header_seen {
            if trimmed.starts_with("Flags,IntendedVsync") {
                header_seen = true;
            }
            continue;
        }
        if trimmed.is_empty() || !trimmed.starts_with(|character: char| character.is_ascii_digit())
        {
            continue;
        }
        let columns: Vec<&str> = trimmed.split(',').collect();
        if columns.len() < 14 {
            continue;
        }
        let intended: u64 = columns[1].parse().unwrap_or(0);
        let completed: u64 = columns[13].parse().unwrap_or(0);
        if intended == 0
            || completed <= intended
            || completed == i64::MAX as u64
            || completed <= previous
        {
            continue;
        }
        *watermark = (*watermark).max(completed);
        sample.push((completed - intended) as f32 / 1_000_000.0);
    }
    if sample.len() > SAMPLE_FRAMES {
        sample.drain(..sample.len() - SAMPLE_FRAMES);
    }
    // 无时间戳的累计百分位无法去重，不将其作为新的慢帧再次提频。
    FasReport {
        package: package.to_owned(),
        worst_ms: sample.iter().copied().fold(0.0, f32::max),
        over_budget: sample.iter().filter(|value| **value > 16.0).count(),
        frames: sample.len(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Flags,IntendedVsync,Vsync,OldestInputEvent,NewestInputEvent,HandleInputStart,AnimationStart,PerformTraversalsStart,DrawStart,SyncQueued,SyncStart,IssueDrawCommandsStart,SwapBuffers,FrameCompleted";

    #[test]
    fn pending_requests_keep_only_latest() {
        let mut collector = FasCollector {
            shared: Arc::new((Mutex::new(ProbeState::default()), Condvar::new())),
            last_request_us: 0,
            last_error: None,
        };
        collector.shared.0.lock().unwrap().enabled = true;
        collector.request("first", REQUEST_INTERVAL_US);
        // Model the worker taking the one running request.
        let running = collector.shared.0.lock().unwrap().pending.take();
        collector.request("old", REQUEST_INTERVAL_US * 2);
        collector.request("latest", REQUEST_INTERVAL_US * 2 + 1);
        assert_eq!(running.as_deref(), Some("first"));
        assert_eq!(
            collector.shared.0.lock().unwrap().pending.as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn disable_invalidates_queued_and_running_results() {
        let mut state = ProbeState {
            enabled: true,
            pending: Some("queued".into()),
            ..ProbeState::default()
        };
        let old_generation = state.generation;
        assert!(state.publish(old_generation, parse_framestats("old", "")));
        state.disable();
        assert!(state.pending.is_none());
        assert!(state.report.is_none());
        assert!(!state.available);
        state.enabled = true;
        assert!(!state.publish(old_generation, parse_framestats("late", "")));
        assert!(state.publish(state.generation, parse_framestats("new", "")));
        assert_eq!(state.report.unwrap().package, "new");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn child_output_is_drained_and_exit_failure_reported() {
        let output = command_output(
            Command::new("sh").args(["-c", "head -c 262144 /dev/zero"]),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output.len(), 262144);
        assert!(command_output(
            Command::new("sh").args(["-c", "exit 7"]),
            Duration::from_secs(2),
        )
        .is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn timeout_kills_and_reaps_child_without_waiting_for_pipe_eof() {
        let started = Instant::now();
        let error = command_output(
            Command::new("sh").args(["-c", "sleep 2 & exec sleep 10"]),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn parse_marks_only_frames() {
        let text = format!(
            "{HEADER}\n0,1000,1000,0,0,0,0,0,0,0,0,0,0,17000000\n0,2000,2000,0,0,0,0,0,0,0,0,0,0,25000000\n"
        );
        let report = parse_framestats("demo", &text);
        assert_eq!(report.frames, 2);
        assert!((report.worst_ms - 24.998).abs() < 0.001);
        assert_eq!(report.over_budget, 2);
    }

    #[test]
    fn repeated_dump_does_not_repeat_slow_frames() {
        let text = format!("{HEADER}\n0,1000,1000,0,0,0,0,0,0,0,0,0,0,25000000\n");
        let mut watermark = 0;
        assert_eq!(
            parse_new_framestats("demo", &text, &mut watermark).frames,
            1
        );
        assert_eq!(watermark, 25_000_000);
        let repeated = parse_new_framestats("demo", &text, &mut watermark);
        assert_eq!(repeated.frames, 0);
        assert_eq!(repeated.worst_ms, 0.0);
        let fresh = format!("{text}0,30000000,30000000,0,0,0,0,0,0,0,0,0,0,31000000\n");
        let report = parse_new_framestats("demo", &fresh, &mut watermark);
        assert_eq!(report.frames, 1);
        assert_eq!(report.worst_ms, 1.0);
    }

    #[test]
    fn summary_without_timestamp_does_not_trigger_lift() {
        assert_eq!(parse_framestats("demo", "95th percentile: 100ms").frames, 0);
    }

    #[test]
    fn empty_report_is_not_an_error() {
        let text = "no framestats";
        let report = parse_framestats("demo", text);
        assert_eq!(report.frames, 0);
        assert!(report.error.is_none());
    }
}
