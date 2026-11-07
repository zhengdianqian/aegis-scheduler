// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Touch event source.
//!
//! Every touch panel exposes an evdev node with `ABS_MT_POSITION_X` and
//! `BTN_TOUCH`. Those descriptors join the control loop's epoll set, so a
//! finger landing on a static page wakes the daemon exactly once and no timer
//! is involved. Devices that do not expose a usable node are reported as
//! unavailable; the daemon keeps running with the other event sources.

use std::io;
use std::os::unix::io::RawFd;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const BTN_TOUCH: u16 = 0x14a;
const ABS_MT_POSITION_X: usize = 53;
const EVENT_SIZE: usize = 24;
const MAX_DEVICES: usize = 6;

const EXCLUDED_NAMES: &[&str] = &[
    "fpc", "uinput", "jack", "haptics", "pwrkey", "resin", "gpio", "headset",
];

/// What one read of a touch node contained.
#[derive(Debug, Clone, Copy, Default)]
pub struct TouchBatch {
    pub downs: u32,
    pub ups: u32,
    pub moves: u32,
}

impl TouchBatch {
    pub fn is_empty(&self) -> bool {
        self.downs == 0 && self.ups == 0 && self.moves == 0
    }
}

/// One evdev node that carries multi-touch contacts.
pub struct TouchWatcher {
    fd: RawFd,
    name: String,
}

impl TouchWatcher {
    /// Scans `/dev/input/event*` and opens every multi-touch node. Two panels
    /// (foldable inner/outer) are both kept; anything else is ignored.
    pub fn open_all() -> Vec<Self> {
        let mut devices = Vec::new();
        for index in 0..32 {
            if devices.len() >= MAX_DEVICES {
                break;
            }
            let path = format!("/dev/input/event{index}");
            let Some(watcher) = Self::open(&path) else {
                continue;
            };
            devices.push(watcher);
        }
        devices
    }

    fn open(path: &str) -> Option<Self> {
        let c_path = std::ffi::CString::new(path).ok()?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }

        let name = device_name(fd).unwrap_or_default();
        let excluded = EXCLUDED_NAMES
            .iter()
            .any(|needle| name.to_ascii_lowercase().contains(needle));
        let usable = !excluded
            && has_abs(fd, ABS_MT_POSITION_X)
            && has_key(fd, BTN_TOUCH as usize);
        if !usable {
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Self { fd, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Consumes pending events. A finger down or a contact move refreshes the
    /// lift window; a finger up ends it and lets demand decide again.
    pub fn poll(&mut self) -> TouchBatch {
        let mut batch = TouchBatch::default();
        let mut buffer = [0u8; EVENT_SIZE * 64];
        loop {
            let read = unsafe {
                libc::read(self.fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len())
            };
            if read <= 0 {
                break;
            }
            let count = read as usize / EVENT_SIZE;
            for chunk in buffer[..count * EVENT_SIZE].chunks_exact(EVENT_SIZE) {
                let kind = u16::from_le_bytes([chunk[16], chunk[17]]);
                let code = u16::from_le_bytes([chunk[18], chunk[19]]);
                let value = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
                if kind == EV_KEY && code == BTN_TOUCH {
                    if value != 0 {
                        batch.downs += 1;
                    } else {
                        batch.ups += 1;
                    }
                } else if kind == EV_ABS && code == ABS_MT_POSITION_X as u16 && value != 0 {
                    batch.moves += 1;
                }
            }
        }
        batch
    }
}

impl mio::event::Source for TouchWatcher {
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

impl Drop for TouchWatcher {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn device_name(fd: RawFd) -> Option<String> {
    let mut buffer = [0u8; 128];
    let request = eviocgname(buffer.len());
    let result = unsafe { libc::ioctl(fd, request, buffer.as_mut_ptr() as *mut libc::c_void) };
    if result < 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&buffer)
        .split('\0')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn has_abs(fd: RawFd, bit: usize) -> bool {
    let mut buffer = [0u8; 8];
    let request = eviocgbit(3, buffer.len());
    let result = unsafe { libc::ioctl(fd, request, buffer.as_mut_ptr() as *mut libc::c_void) };
    result >= 0 && bit_is_set(&buffer, bit)
}

fn has_key(fd: RawFd, bit: usize) -> bool {
    let mut buffer = [0u8; 64];
    let request = eviocgbit(1, buffer.len());
    let result = unsafe { libc::ioctl(fd, request, buffer.as_mut_ptr() as *mut libc::c_void) };
    result >= 0 && bit_is_set(&buffer, bit)
}

fn bit_is_set(buffer: &[u8], bit: usize) -> bool {
    buffer
        .get(bit / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

fn eviocgbit(ev: usize, len: usize) -> libc::c_int {
    let number = 0x20 + ev;
    (2u32 << 30 | (len as u32) << 16 | (b'E' as u32) << 8 | number as u32) as libc::c_int
}

fn eviocgname(len: usize) -> libc::c_int {
    (2u32 << 30 | (len as u32) << 16 | (b'E' as u32) << 8 | 0x06) as libc::c_int
}
