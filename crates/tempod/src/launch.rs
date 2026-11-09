// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Launch timing from the framework's own log stream.
//!
//! The probe can only observe that the foreground became busy on a runtime
//! quantum; the actual `START` and `Displayed` lines arrive earlier and with
//! the exact instant the first frame was drawn. That is what the launch window
//! needs: release the floor on `START`, restore it on `Displayed` plus a short
//! settle, and never on a guessed fixed delay.

use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TAG: &str = "aegis-launch";
const RESTART_DELAY: Duration = Duration::from_secs(2);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);

fn retry_delay(delay: &mut Duration) -> Duration {
    let current = *delay;
    *delay = delay.saturating_mul(2).min(MAX_RESTART_DELAY);
    current
}

#[derive(Debug, PartialEq, Eq)]
enum LaunchLine {
    Start {
        component: String,
        user: u32,
    },
    Displayed {
        component: String,
        user: Option<u32>,
    },
}

fn user_id(word: &str) -> Option<u32> {
    word.strip_prefix('u')?.parse().ok()
}

fn normalize_component(word: &str) -> Option<String> {
    let (package, activity) = word.trim_end_matches(['}', ':']).split_once('/')?;
    if package.is_empty() || activity.is_empty() || activity.contains('/') {
        return None;
    }
    let activity = if activity.starts_with('.') {
        format!("{package}{activity}")
    } else {
        activity.to_owned()
    };
    Some(format!("{package}/{activity}"))
}

fn parse_launch_line(line: &str) -> Option<LaunchLine> {
    let words: Vec<_> = line.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        let tail = &words[index + 1..];
        match *word {
            "START" => {
                let user = user_id(tail.first()?)?;
                let component = tail
                    .iter()
                    .find_map(|word| {
                        normalize_component(word.trim_start_matches('{').strip_prefix("cmp=")?)
                    })?;
                return Some(LaunchLine::Start { component, user });
            }
            "Displayed" => {
                let user = user_id(tail.first()?);
                let word = *tail.get(usize::from(user.is_some()))?;
                if !word.ends_with(':') {
                    return None;
                }
                return Some(LaunchLine::Displayed {
                    component: normalize_component(word)?,
                    user,
                });
            }
            _ => {}
        }
    }
    None
}

fn epoch_us(line: &str) -> Option<u64> {
    let (seconds, fraction) = line.split_whitespace().next()?.split_once('.')?;
    if fraction.len() != 6 {
        return None;
    }
    seconds
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(fraction.parse::<u64>().ok()?)
}

fn fresh_log(logged_us: u64, since_us: u64, now_us: u64) -> bool {
    logged_us > since_us && logged_us <= now_us && now_us - logged_us <= 1_000_000
}

#[derive(Clone, Copy, Default)]
pub struct LaunchSnapshot {
    pub start_us: u64,
    pub displayed_us: u64,
}

#[derive(Default)]
struct LaunchState {
    snapshot: LaunchSnapshot,
    pending: Option<(String, u32, u64, bool)>,
    // Repeated launches of a component make userless completions ambiguous:
    // they may belong to an earlier START, including one for a different user.
    seen: std::collections::HashSet<String>,
}

/// Both timestamps belong to one START and use CLOCK_MONOTONIC microseconds.
#[derive(Default)]
pub struct LaunchEvents {
    state: Mutex<LaunchState>,
}

impl LaunchEvents {
    pub fn snapshot(&self) -> LaunchSnapshot {
        self.state.lock().unwrap().snapshot
    }

    fn record(&self, event: LaunchLine, logged_us: u64, now: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        match event {
            LaunchLine::Start { component, user } => {
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| logged_us <= pending.2)
                {
                    return false;
                }
                let userless = state.seen.insert(component.clone());
                state.pending = Some((component, user, logged_us, userless));
                state.snapshot = LaunchSnapshot {
                    start_us: now,
                    displayed_us: 0,
                };
                true
            }
            LaunchLine::Displayed { component, user } => {
                let Some((pending_component, pending_user, start, userless)) =
                    state.pending.as_ref()
                else {
                    return false;
                };
                if component != *pending_component
                    || logged_us <= *start
                    || !user.map_or(*userless, |user| user == *pending_user)
                    || state.snapshot.displayed_us != 0
                {
                    return false;
                }
                state.snapshot.displayed_us = now;
                state.seen.remove(&component);
                true
            }
        }
    }
}

/// Starts the reader thread. `None` means the log stream is unavailable and
/// the caller must fall back to the probe's launch edge.
pub fn spawn(wake: Arc<WakePipe>) -> Option<Arc<LaunchEvents>> {
    let events = Arc::new(LaunchEvents::default());
    let stream_since_us = realtime_us();
    let child = match launch_logcat() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "launch log stream unavailable; using probe edges");
            return None;
        }
    };
    let reader_events = Arc::clone(&events);
    let spawned = thread::Builder::new().name(TAG.to_owned()).spawn(move || {
        let mut child = Some(child);
        let mut stream_since_us = stream_since_us;
        let mut delay = RESTART_DELAY;
        loop {
            if child.is_none() {
                stream_since_us = realtime_us();
                reader_events.state.lock().unwrap().pending = None;
            }
            let mut process = match child.take() {
                Some(child) => child,
                None => match launch_logcat() {
                    Ok(child) => child,
                    Err(error) => {
                        tracing::warn!(%error, "launch log stream could not be restarted");
                        thread::sleep(retry_delay(&mut delay));
                        continue;
                    }
                },
            };
            let started = Instant::now();
            if let Some(stdout) = process.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let Some(logged_us) = epoch_us(&line) else {
                        continue;
                    };
                    // AOSP clamps `-T 0` to one historical line. Reject that
                    // replay explicitly using the timestamp from logd.
                    if !fresh_log(logged_us, stream_since_us, realtime_us()) {
                        continue;
                    }
                    if let Some(event) = parse_launch_line(&line) {
                        if reader_events.record(event, logged_us, crate::event_loop::now_us()) {
                            wake.wake();
                        }
                    }
                }
            }
            let _ = process.kill();
            let _ = process.wait();
            if started.elapsed() >= MAX_RESTART_DELAY {
                delay = RESTART_DELAY;
            }
            child = None;
            thread::sleep(retry_delay(&mut delay));
        }
    });
    if spawned.is_err() {
        tracing::warn!("launch reader thread could not start; using probe edges");
        return None;
    }
    Some(events)
}

fn realtime_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// START and Displayed may land in different log buffers.
fn launch_logcat() -> io::Result<Child> {
    Command::new("logcat")
        .args([
            "-b",
            "main,system",
            "-v",
            "epoch,usec",
            "-T",
            "0",
            "-s",
            "ActivityTaskManager:I",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(events: &LaunchEvents, line: &str, at: u64) -> bool {
        events.record(parse_launch_line(line).unwrap(), at, at)
    }

    #[test]
    fn parses_and_normalizes_components_for_all_users() {
        for user in [0, 10, 999] {
            let line = format!("I/ActivityTaskManager( 100): START u{user} {{act=MAIN cmp=com.demo/.Main}} from uid 1000");
            assert_eq!(
                parse_launch_line(&line),
                Some(LaunchLine::Start {
                    component: "com.demo/com.demo.Main".to_owned(),
                    user,
                })
            );
        }
        assert_eq!(
            normalize_component("com.demo/.Main}"),
            normalize_component("com.demo/com.demo.Main:")
        );
        for line in [
            "START u cmp=com.demo/.Main",
            "START ux cmp=com.demo/.Main",
            "START u0 {act=MAIN}",
            "Displayed unrelated text",
        ] {
            assert_eq!(parse_launch_line(line), None);
        }
        assert_eq!(
            epoch_us("1794220000.123456 100 100 I ActivityTaskManager: START"),
            Some(1_794_220_000_123_456)
        );
    }

    #[test]
    fn completion_requires_latest_component_user_and_timestamp() {
        let events = LaunchEvents::default();
        assert!(!record(&events, "Displayed com.demo/.Main: +10ms", 50));
        record(&events, "START u10 {cmp=com.demo/.Main}", 100);
        assert!(!record(
            &events,
            "Displayed u10 com.other/.Main: +10ms",
            200
        ));
        assert!(!record(&events, "Displayed u0 com.demo/.Main: +10ms", 200));
        assert!(!record(&events, "Displayed u10 com.demo/.Main: +10ms", 90));
        assert_eq!(events.snapshot().displayed_us, 0);
        assert!(record(
            &events,
            "Displayed com.demo/com.demo.Main: +10ms",
            200
        ));
        assert_eq!(events.snapshot().start_us, 100);
        assert_eq!(events.snapshot().displayed_us, 200);
        record(&events, "START u10 {cmp=com.other/.Main}", 300);
        assert_eq!(events.snapshot().displayed_us, 0);
        assert!(!record(&events, "Displayed u10 com.demo/.Main: +10ms", 400));
        assert!(!record(&events, "START u10 {cmp=com.demo/.Main}", 250));
        assert_eq!(events.snapshot().start_us, 300);
    }

    #[test]
    fn userless_completion_does_not_guess_across_users_or_overlapping_starts() {
        let events = LaunchEvents::default();
        record(&events, "START u0 {cmp=com.demo/.Main}", 100);
        record(&events, "START u10 {cmp=com.demo/.Main}", 200);
        assert!(!record(&events, "Displayed com.demo/.Main: +10ms", 300));
        assert!(!record(&events, "Displayed u0 com.demo/.Main: +10ms", 300));
        assert!(record(&events, "Displayed u10 com.demo/.Main: +10ms", 300));
        record(&events, "START u10 {cmp=com.other/.Main}", 400);
        record(&events, "START u10 {cmp=com.other/.Main}", 500);
        assert!(!record(&events, "Displayed com.other/.Main: +10ms", 600));
        assert_eq!(events.snapshot().displayed_us, 0);
    }

    #[test]
    fn completed_component_can_launch_again_without_user_in_displayed() {
        let events = LaunchEvents::default();
        for at in [100, 300, 500] {
            assert!(record(&events, "START u0 {cmp=com.demo/.Main}", at));
            assert!(record(&events, "Displayed com.demo/.Main: +10ms", at + 50));
            assert_eq!(events.snapshot().displayed_us, at + 50);
        }
    }

    #[test]
    fn replay_and_delayed_start_logs_are_not_fresh() {
        assert!(!fresh_log(99, 100, 200));
        assert!(!fresh_log(100, 100, 200));
        assert!(!fresh_log(101, 100, 1_000_102));
        assert!(!fresh_log(300, 100, 200));
        assert!(fresh_log(150, 100, 200));
    }

    #[test]
    fn userless_old_completion_cannot_cross_intervening_launch() {
        let events = LaunchEvents::default();
        record(&events, "START u0 {cmp=com.demo/.Main}", 100);
        record(&events, "START u0 {cmp=com.other/.Main}", 200);
        record(&events, "START u0 {cmp=com.demo/.Main}", 300);
        assert!(!record(&events, "Displayed com.demo/.Main: +10ms", 400));
        assert_eq!(events.snapshot().displayed_us, 0);
    }

    #[test]
    fn restart_backoff_doubles_and_caps() {
        let mut delay = RESTART_DELAY;
        for seconds in [2, 4, 8, 16, 32, 60, 60] {
            assert_eq!(retry_delay(&mut delay), Duration::from_secs(seconds));
        }
    }
}

/// Self-pipe used to interrupt the epoll loop from the log reader thread.
pub struct WakePipe {
    read: libc::c_int,
    write: libc::c_int,
}

impl WakePipe {
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        let code = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: fds[0],
            write: fds[1],
        })
    }

    pub fn wake(&self) {
        let byte = 1u8;
        unsafe {
            libc::write(self.write, &byte as *const u8 as *const libc::c_void, 1);
        }
    }

    pub fn drain(&self) {
        let mut buffer = [0u8; 64];
        loop {
            let count = unsafe {
                libc::read(
                    self.read,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if count <= 0 {
                break;
            }
        }
    }
}

impl Drop for WakePipe {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

impl mio::event::Source for WakePipe {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&self.read), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&self.read), token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&self.read))
    }
}
