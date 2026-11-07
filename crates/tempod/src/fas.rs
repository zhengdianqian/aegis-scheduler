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

use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

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

pub struct FasCollector {
    sender: Sender<String>,
    reports: Receiver<FasReport>,
    last_request_us: u64,
    enabled: bool,
    available: Arc<AtomicBool>,
    pub last_error: Option<String>,
}

impl FasCollector {
    pub fn new(registry: &mio::Registry, token: mio::Token) -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<String>();
        let (report_tx, report_rx) = mpsc::channel::<FasReport>();
        let waker = mio::Waker::new(registry, token)?;
        let available = Arc::new(AtomicBool::new(false));
        let worker_available = available.clone();

        std::thread::Builder::new()
            .name("fas-probe".to_owned())
            .spawn(move || loop {
                let Ok(package) = request_rx.recv() else {
                    return;
                };
                let report = probe(&package);
                let ok = report.error.is_none();
                worker_available.store(ok, Ordering::Relaxed);
                if report_tx.send(report).is_err() {
                    return;
                }
                let _ = waker.wake();
            })?;

        Ok(Self {
            sender: request_tx,
            reports: report_rx,
            last_request_us: 0,
            enabled: false,
            available,
            last_error: None,
        })
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&mut self, enabled: bool, config: &FasConfig) {
        self.enabled = enabled;
        if !enabled {
            return;
        }
        if self.last_error.is_some() {
            self.last_error = None;
        }
        let _ = config;
    }

    pub fn request(&mut self, package: &str, at_us: u64) {
        if !self.enabled || package.is_empty() {
            return;
        }
        if at_us.saturating_sub(self.last_request_us) < REQUEST_INTERVAL_US {
            return;
        }
        self.last_request_us = at_us;
        let _ = self.sender.send(package.to_owned());
    }

    pub fn drain(&mut self) -> Vec<FasReport> {
        let mut reports = Vec::new();
        loop {
            match self.reports.try_recv() {
                Ok(report) => reports.push(report),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(problem) = reports.iter().find_map(|report| report.error.clone()) {
            self.last_error = Some(problem);
        }
        reports
    }
}

fn probe(package: &str) -> FasReport {
    let output = Command::new(DUMPSYS)
        .arg("gfxinfo")
        .arg(package)
        .arg("framestats")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return FasReport {
                package: package.to_owned(),
                worst_ms: 0.0,
                over_budget: 0,
                frames: 0,
                error: Some(format!("dumpsys: {error}")),
            }
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_framestats(package, &text)
}

/// Parses the CSV block of `framestats`. Columns 1 and 13 are the intended
/// vsync and the frame completion timestamp in nanoseconds.
pub fn parse_framestats(package: &str, text: &str) -> FasReport {
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
        if trimmed.is_empty() || !trimmed.starts_with(|character: char| character.is_ascii_digit()) {
            continue;
        }
        let columns: Vec<&str> = trimmed.split(',').collect();
        if columns.len() < 14 {
            continue;
        }
        let intended: u64 = columns[1].parse().unwrap_or(0);
        let completed: u64 = columns[13].parse().unwrap_or(0);
        if intended == 0 || completed <= intended {
            continue;
        }
        sample.push((completed - intended) as f32 / 1_000_000.0);
    }
    if sample.len() > SAMPLE_FRAMES {
        sample.drain(..sample.len() - SAMPLE_FRAMES);
    }
    if sample.is_empty() {
        // Streaming framestats is not exposed on every release; the summary
        // carries the 95th percentile, which is enough for one decision.
        if let Some(worst) = percentile_95(text) {
            return FasReport {
                package: package.to_owned(),
                worst_ms: worst,
                over_budget: usize::from(worst > 16.0),
                frames: 1,
                error: None,
            };
        }
    }
    FasReport {
        package: package.to_owned(),
        worst_ms: sample.iter().copied().fold(0.0, f32::max),
        over_budget: sample.iter().filter(|value| **value > 16.0).count(),
        frames: sample.len(),
        error: None,
    }
}

fn percentile_95(text: &str) -> Option<f32> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("95th percentile:")?;
        value.trim().trim_end_matches("ms").parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Flags,IntendedVsync,Vsync,OldestInputEvent,NewestInputEvent,HandleInputStart,AnimationStart,PerformTraversalsStart,DrawStart,SyncQueued,SyncStart,IssueDrawCommandsStart,SwapBuffers,FrameCompleted";

    #[test]
    fn parse_marks_only_frames() {
        let text = format!(
            "{HEADER}\n0,1000,1000,0,0,0,0,0,0,0,0,0,0,17000000\n0,2000,2000,0,0,0,0,0,0,0,0,0,0,25000000\n"
        );
        let report = parse_framestats("demo", &text);
        assert_eq!(report.frames, 2);
        assert!((report.worst_ms - 23.8).abs() < 0.001);
        assert_eq!(report.over_budget, 1);
    }

    #[test]
    fn empty_report_is_not_an_error() {
        let text = "no framestats";
        let report = parse_framestats("demo", text);
        assert_eq!(report.frames, 0);
        assert!(report.error.is_none());
    }
}
