// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Display-state event source.
//!
//! Android boards expose the panel state through the backlight class:
//! `bl_power == FB_BLANK_UNBLANK` means the panel is lit. There is no periodic
//! sampling here — the daemon registers the kernel's `inotify` notifications
//! for the backlight device directory in the same epoll set that serves the
//! probe, so a screen transition is just one more readable descriptor.
//!
//! When the configured path is empty the active panel is discovered: the
//! device whose `bl_power` currently reads unblank wins, otherwise the first
//! readable one is used.

use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

const BACKLIGHT_ROOT: &str = "/sys/class/backlight";
const EVENT_BUFFER: usize = 4_096;

/// Panel state watcher bound to a backlight device directory.
pub struct ScreenWatcher {
    fd: RawFd,
    device: PathBuf,
    on: bool,
}

impl ScreenWatcher {
    /// Opens the watcher for the configured device directory, or for the
    /// discovered active panel when `configured` is empty.
    pub fn open(configured: &str) -> io::Result<Self> {
        let device = if configured.trim().is_empty() {
            discover()?
        } else {
            let path = PathBuf::from(configured.trim());
            if path.is_dir() {
                path
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad screen path"))?
            }
        };

        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let interest = libc::IN_MODIFY
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_TO
            | libc::IN_CREATE
            | libc::IN_DELETE;
        let watch = unsafe {
            libc::inotify_add_watch(fd, c_path(&device).as_ptr(), interest as libc::c_uint)
        };
        if watch < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }

        let on = read_bl_power(&device).unwrap_or(0) == 0;
        Ok(Self { fd, device, on })
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn device(&self) -> &Path {
        &self.device
    }

    /// Consumes pending notifications and reports the new state when it differs
    /// from the one already published. Brightness-only changes return `None`.
    pub fn refresh(&mut self) -> Option<bool> {
        let mut buffer = [0u8; EVENT_BUFFER];
        loop {
            let read = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted
                {
                    break;
                }
                // A broken watch must not spin the loop: treat it as no change.
                break;
            }
            if read == 0 {
                break;
            }
        }

        let now_on = read_bl_power(&self.device).unwrap_or(0) == 0;
        if now_on == self.on {
            None
        } else {
            self.on = now_on;
            Some(now_on)
        }
    }
}

impl mio::event::Source for ScreenWatcher {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&self.fd), token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&self.fd))
    }
}

impl Drop for ScreenWatcher {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Picks the backlight device that is currently unblank.
fn discover() -> io::Result<PathBuf> {
    let entries = std::fs::read_dir(BACKLIGHT_ROOT)?;
    let mut first: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(value) = read_bl_power(&path) else {
            continue;
        };
        if value == 0 {
            return Ok(path);
        }
        if first.is_none() {
            first = Some(path);
        }
    }
    first.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no readable backlight device found",
        )
    })
}

fn read_bl_power(device: &Path) -> Option<u32> {
    std::fs::read_to_string(device.join("bl_power"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn c_path(path: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| std::ffi::CString::new("/sys/class/backlight").unwrap())
}
