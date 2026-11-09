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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TAG: &str = "aegis-launch";
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// Timestamps are `CLOCK_MONOTONIC` microseconds, the same clock the event
/// loop uses, so the two can be compared directly.
pub struct LaunchEvents {
    start_us: AtomicU64,
}

impl LaunchEvents {
    pub fn start_us(&self) -> u64 {
        self.start_us.load(Ordering::Relaxed)
    }
}

/// Starts the reader thread. `None` means the log stream is unavailable and
/// the caller must fall back to the probe's launch edge.
pub fn spawn(wake: Arc<WakePipe>) -> Option<Arc<LaunchEvents>> {
    let events = Arc::new(LaunchEvents {
        start_us: AtomicU64::new(0),
    });
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
        loop {
            let mut process = match child.take() {
                Some(child) => child,
                None => match launch_logcat() {
                    Ok(child) => child,
                    Err(error) => {
                        tracing::warn!(%error, "launch log stream could not be restarted");
                        thread::sleep(RESTART_DELAY);
                        continue;
                    }
                },
            };
            if let Some(stdout) = process.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.contains("START u0 ") && line.contains("cmp=") {
                        let now = crate::event_loop::now_us();
                        reader_events.start_us.store(now, Ordering::Relaxed);
                        wake.wake();
                    }
                }
            }
            let _ = process.wait();
            child = None;
            thread::sleep(RESTART_DELAY);
        }
    });
    if spawned.is_err() {
        tracing::warn!("launch reader thread could not start; using probe edges");
        return None;
    }
    Some(events)
}

/// `START` lines land in the system buffer, `Displayed` in the main buffer on
/// this platform, so both must be read.
fn launch_logcat() -> io::Result<Child> {
    Command::new("logcat")
        .args([
            "-b",
            "main,system",
            "-v",
            "brief",
            "-s",
            "ActivityTaskManager:I",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
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
