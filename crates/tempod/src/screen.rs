// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Display-state event source.
//!
//! Android boards expose the panel state through the backlight class:
//! `bl_power == FB_BLANK_UNBLANK` means the panel is lit. There is no periodic
//! sampling here — the daemon registers the kernel's `inotify` notifications
//! for every backlight device directory in the same epoll set that serves the
//! load source, so a screen transition is just one more readable descriptor.
//!
//! v5.2 起监听 /sys/class/backlight 下的**全部**面板并按“任一面板点亮”判定：
//! 开机后活跃面板的注册顺序可能漂移（实测重启后 panel0/panel1 互换，单面板
//! 监听把亮屏误判为息屏、Aegis 长期挂起且频率被外部下限钉死）。任一亮屏的
//! 语义对双屏（折叠/副屏）也正确：任一面板工作即视为亮屏。

use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

const BACKLIGHT_ROOT: &str = "/sys/class/backlight";
const EVENT_BUFFER: usize = 4_096;

/// Panel state watcher bound to every backlight device directory.
pub struct ScreenWatcher {
    fd: RawFd,
    devices: Vec<PathBuf>,
    on: bool,
    source: String,
}

impl ScreenWatcher {
    /// Opens one inotify for the backlight root plus each panel directory.
    /// `configured` 只作为首选提示：存在的配置面板会被纳入监听，但最终判定
    /// 始终基于全部面板的“或”逻辑，不再依赖单一路径。
    pub fn open(configured: &str) -> io::Result<Self> {
        let mut devices = scan_panels();
        let configured = configured.trim();
        if !configured.is_empty() {
            let path = PathBuf::from(configured);
            let dir = if path.is_dir() {
                path
            } else {
                path.parent().map(Path::to_path_buf).unwrap_or(path)
            };
            if dir.is_dir() && !devices.contains(&dir) {
                devices.push(dir);
            }
        }
        if devices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no readable backlight device found",
            ));
        }

        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // 根目录事件用于发现新注册的面板（例如另一块屏的驱动后注册）。
        if add_watch(fd, Path::new(BACKLIGHT_ROOT)).is_none() {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        for device in &devices {
            if add_watch(fd, device).is_none() {
                unsafe { libc::close(fd) };
                return Err(io::Error::last_os_error());
            }
        }

        let on = any_panel_on(&devices);
        Ok(Self {
            fd,
            source: source_label(&devices),
            devices,
            on,
        })
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    /// 诊断用的面板清单（如 `panel0-backlight+panel1-backlight`）。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Consumes pending notifications and reports the new state when it differs
    /// from the one already published. Brightness-only changes return `None`.
    pub fn refresh(&mut self) -> Option<bool> {
        let mut saw_structural_event = false;
        let mut buffer = [0u8; EVENT_BUFFER];
        loop {
            let read = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if read <= 0 {
                break;
            }
            let mut offset = 0usize;
            let header = std::mem::size_of::<libc::inotify_event>();
            while offset + header <= read as usize {
                let event = unsafe { &*(buffer.as_ptr().add(offset) as *const libc::inotify_event) };
                // 根目录 watch 的 cookie 对应面板目录的创建/删除。
                if event.mask & (libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_TO) != 0 {
                    saw_structural_event = true;
                }
                offset += header + event.len as usize;
            }
        }
        if saw_structural_event {
            self.rescan_panels();
        }

        let now_on = any_panel_on(&self.devices);
        if now_on == self.on {
            None
        } else {
            self.on = now_on;
            Some(now_on)
        }
    }

    /// 面板集合可能随驱动注册变化；发现新面板时补挂监听。
    fn rescan_panels(&mut self) {
        for device in scan_panels() {
            if !self.devices.contains(&device) {
                add_watch(self.fd, &device);
                self.devices.push(device);
            }
        }
        self.source = source_label(&self.devices);
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

/// 列出全部可读 bl_power 的 backlight 目录（固定排序，保证 source 标签稳定）。
fn scan_panels() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(BACKLIGHT_ROOT) else {
        return Vec::new();
    };
    let mut panels: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("bl_power").exists())
        .collect();
    panels.sort();
    panels
}

fn source_label(devices: &[PathBuf]) -> String {
    devices
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("+")
}

/// 任一面板点亮即为亮屏。
fn any_panel_on(devices: &[PathBuf]) -> bool {
    devices.iter().any(|device| panel_on(device))
}

fn add_watch(fd: i32, device: &Path) -> Option<i32> {
    let watch = unsafe { libc::inotify_add_watch(fd, c_path(device).as_ptr(), interest()) };
    if watch < 0 {
        None
    } else {
        Some(watch)
    }
}

fn interest() -> libc::c_uint {
    (libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_CREATE
        | libc::IN_DELETE) as libc::c_uint
}

fn read_bl_power(device: &Path) -> Option<u32> {
    std::fs::read_to_string(device.join("bl_power"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 面板是否真的点亮。`bl_power==0`（unblank）在 AOD/异常路径下可能与实际
/// 画面不一致（实测息屏期间 brightness 已为 0 但 bl_power 仍报 unblank，
/// 导致息屏后仍按亮屏接管、持续写频率）：因此 brightness>0 才算亮。
/// brightness 节点缺失的面板退回只看 bl_power。
fn panel_on(device: &Path) -> bool {
    if read_bl_power(device).unwrap_or(1) != 0 {
        return false;
    }
    match std::fs::read_to_string(device.join("brightness")) {
        Ok(text) => text.trim().parse::<u32>().unwrap_or(0) > 0,
        Err(_) => true,
    }
}

fn c_path(path: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| std::ffi::CString::new("/sys/class/backlight").unwrap())
}
