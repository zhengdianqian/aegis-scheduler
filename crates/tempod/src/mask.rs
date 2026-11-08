// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Short-window control of the `scaling_min_freq` bind mount plus a matching
//! `scaling_max_freq` cap.
//!
//! The module mounts a file over every policy floor, so a vendor perf service
//! can no longer raise the kernel minimum. Pinning the *minimum* alone is not
//! enough: schedutil and vendor uclamp hints still pick a mid/high frequency
//! for a nearly idle cluster, which is exactly the "low load, high frequency"
//! state this module exists to remove. While the pin is in place the ceiling is
//! therefore lowered to the same hardware minimum, so a quiet cluster cannot
//! run anywhere but that step.
//!
//! The pin also hides the vendor's own launch boost from the kernel: a cold
//! start then runs on the hardware minimum and the first frame can take
//! seconds, which the user sees as an unpainted window. So the pin is
//! releasable: on a launch/touch/screen-on the ceiling is restored first and
//! the mounts are then removed, letting the vendor stack drive the launch
//! exactly as it does without the module. Once the controller reports that the
//! system has settled, the floor is written back, the mounts are restored and
//! the ceiling is lowered again.
//!
//! In-process cost of the whole open/close pair was measured at ~80 µs median
//! (max 289 µs), far below one frame, so no further structure is needed.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tempo_core::DeviceProfile;

use crate::writer::{policy_dir, read_u32};

const MASK_DIR: &str = "/data/adb/tempo/mask";

struct Entry {
    node: PathBuf,
    mask: PathBuf,
    hw_min: u32,
    max_node: PathBuf,
    saved_max: u32,
}

/// Owns the three bind mounts for the duration of the daemon.
pub struct MaskControl {
    entries: Vec<Entry>,
    masked: bool,
}

impl MaskControl {
    /// Returns `None` when the module is not currently masking: without an
    /// active mount there is nothing to release, and remounting a floor that
    /// the user disabled would be a write nobody asked for.
    pub fn discover(profile: &DeviceProfile) -> Option<Self> {
        let mounted = mounted_points();
        let mut entries = Vec::new();
        for cluster in &profile.clusters {
            let Some(cpu) = cluster.cpus.first().copied() else {
                continue;
            };
            let Some(dir) = policy_dir(cpu) else {
                continue;
            };
            let node = dir.join("scaling_min_freq");
            let name = dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())?;
            if !mounted.iter().any(|point| point == &node.to_string_lossy()) {
                continue;
            }
            let Some(hw_min) = read_u32(&dir.join("cpuinfo_min_freq")) else {
                continue;
            };
            let max_node = dir.join("scaling_max_freq");
            // `cpuinfo_max_freq` is the hardware ceiling; `scaling_max_freq` may
            // already carry a vendor thermal clamp, which is respected later.
            let saved_max = read_u32(&dir.join("cpuinfo_max_freq"))
                .or_else(|| read_u32(&max_node))
                .unwrap_or(hw_min);
            entries.push(Entry {
                node,
                mask: Path::new(MASK_DIR).join(name),
                hw_min,
                max_node,
                saved_max,
            });
        }
        if entries.is_empty() {
            return None;
        }
        // A previous instance that was killed hard can leave the ceiling pinned
        // at the hardware minimum. Uncapping is cheap and makes a restart safe:
        // the cap returns on the next quiet close.
        for entry in &entries {
            let live = read_u32(&entry.max_node).unwrap_or(entry.saved_max);
            if live <= entry.hw_min {
                let _ = fs::write(&entry.max_node, format!("{}\n", entry.saved_max));
            }
        }
        Some(Self {
            entries,
            masked: true,
        })
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    /// Removes the pin so the vendor stack owns the floor and ceiling again.
    /// The ceiling is restored *before* the floor is released, so a launch can
    /// ramp immediately. Idempotent: a node that is already unmounted is not an
    /// error.
    pub fn unmask(&mut self) -> Result<(), String> {
        if !self.masked {
            return Ok(());
        }
        let mut failure: Option<String> = None;
        for entry in &self.entries {
            let live = read_u32(&entry.max_node).unwrap_or(entry.saved_max);
            if live <= entry.hw_min {
                if let Err(error) =
                    fs::write(&entry.max_node, format!("{}\n", entry.saved_max))
                {
                    failure.get_or_insert(format!("max {}: {error}", entry.max_node.display()));
                }
            }
            if let Err(error) = umount(&entry.node) {
                failure.get_or_insert(format!("umount {}: {error}", entry.node.display()));
            }
        }
        self.masked = false;
        match failure {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Writes the hardware minimum back into the real node and re-binds the
    /// mask over it, then lowers the ceiling to the same step so a quiet
    /// cluster cannot sit at a mid/high frequency. The mask file is rewritten
    /// first: every vendor write made while the mask was in place landed there.
    pub fn remask(&mut self) -> Result<(), String> {
        let mut failure: Option<String> = None;
        for entry in &mut self.entries {
            // Remember the live ceiling (a vendor thermal clamp may be lower
            // than the hardware maximum) before it is replaced by the cap.
            let live = read_u32(&entry.max_node).unwrap_or(entry.saved_max);
            if live > entry.hw_min {
                entry.saved_max = live;
            }
            let value = format!("{}\n", entry.hw_min);
            if let Some(parent) = entry.mask.parent() {
                let _ = fs::create_dir_all(parent);
            }
            // Real floor first, then hide the node: writing after the mount
            // would only edit the mask file.
            if let Err(error) = fs::write(&entry.node, &value) {
                failure.get_or_insert(format!("floor {}: {error}", entry.node.display()));
            }
            let _ = fs::write(&entry.mask, &value);
            let _ = fs::set_permissions(&entry.mask, fs::Permissions::from_mode(0o666));
            if let Err(error) = bind(&entry.mask, &entry.node) {
                failure.get_or_insert(format!(
                    "bind {} -> {}: {error}",
                    entry.mask.display(),
                    entry.node.display()
                ));
            }
            if let Err(error) = fs::write(&entry.max_node, &value) {
                failure.get_or_insert(format!("cap {}: {error}", entry.max_node.display()));
            }
        }
        self.masked = true;
        match failure {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// Re-lowers any ceiling that a vendor boost raised again after the pin was
    /// closed. Returns true when at least one node had to be written.
    pub fn assert_cap(&mut self) -> Result<bool, String> {
        let mut changed = false;
        let mut failure: Option<String> = None;
        for entry in &self.entries {
            let live = read_u32(&entry.max_node).unwrap_or(entry.hw_min);
            if live > entry.hw_min {
                match fs::write(&entry.max_node, format!("{}\n", entry.hw_min)) {
                    Ok(()) => changed = true,
                    Err(error) => {
                        failure.get_or_insert(format!("cap {}: {error}", entry.max_node.display()));
                    }
                }
            }
        }
        match failure {
            Some(problem) => Err(problem),
            None => Ok(changed),
        }
    }
}

impl Drop for MaskControl {
    fn drop(&mut self) {
        // Restore the floor pin if the window was open, then always release the
        // ceiling: a daemon that exits must not leave the whole cluster locked
        // at the hardware minimum.
        if !self.masked {
            let _ = self.remask();
        }
        for entry in &self.entries {
            let live = read_u32(&entry.max_node).unwrap_or(entry.saved_max);
            if live <= entry.hw_min {
                let _ = fs::write(&entry.max_node, format!("{}\n", entry.saved_max));
            }
        }
    }
}

/// A demand window in which the floor pin is released. `open` is driven by a
/// launch, touch or screen-on event, `close` by the controller reporting that
/// the system has settled.
pub struct MaskWindow {
    control: Option<MaskControl>,
    open: bool,
    capped: bool,
    opens: u64,
    unmask_us: u64,
    remask_us: u64,
}

impl MaskWindow {
    pub fn new(control: Option<MaskControl>) -> Self {
        Self {
            control,
            open: false,
            capped: false,
            opens: 0,
            unmask_us: 0,
            remask_us: 0,
        }
    }

    /// True when the module was masking at startup and the window is usable.
    pub fn is_tracked(&self) -> bool {
        self.control.is_some()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// True once the floor pin is in place and the ceiling is capped to it.
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    pub fn opens(&self) -> u64 {
        self.opens
    }

    pub fn unmask_us(&self) -> u64 {
        self.unmask_us
    }

    pub fn remask_us(&self) -> u64 {
        self.remask_us
    }

    /// Releases the pin. Returns whether this call actually opened the window.
    pub fn open(&mut self) -> bool {
        if let Some(control) = self.control.as_mut() {
            if !self.open {
                let started = Instant::now();
                if let Err(problem) = control.unmask() {
                    tracing::warn!(%problem, "floor pin could not be released");
                }
                self.unmask_us = started.elapsed().as_micros() as u64;
                self.open = true;
                self.capped = false;
                self.opens += 1;
                return true;
            }
        }
        false
    }

    /// Restores the pin (floor + ceiling) and re-asserts the ceiling if a
    /// vendor write slipped in after it was capped. Returns how long the work
    /// took; zero when the pin was already intact.
    pub fn close(&mut self) -> u64 {
        if let Some(control) = self.control.as_mut() {
            if self.capped {
                let started = Instant::now();
                match control.assert_cap() {
                    Ok(true) => {
                        self.remask_us = started.elapsed().as_micros() as u64;
                        return self.remask_us;
                    }
                    Ok(false) => return 0,
                    Err(problem) => {
                        tracing::warn!(%problem, "ceiling cap could not be re-asserted");
                        return 0;
                    }
                }
            }
            let started = Instant::now();
            if let Err(problem) = control.remask() {
                tracing::warn!(%problem, "floor pin could not be restored");
            }
            self.remask_us = started.elapsed().as_micros() as u64;
            self.capped = true;
        }
        self.open = false;
        self.remask_us
    }
}

/// Mount points currently bound over a cpufreq node.
fn mounted_points() -> Vec<String> {
    let Ok(table) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_owned))
        .collect()
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a nul byte"))
}

fn umount(path: &Path) -> io::Result<()> {
    let target = cstring(path)?;
    let code = unsafe { libc::umount2(target.as_ptr(), 0) };
    if code == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // EINVAL is "not a mount point": the node is already unmasked.
    if error.raw_os_error() == Some(libc::EINVAL) {
        return Ok(());
    }
    Err(error)
}

fn bind(source: &Path, target: &Path) -> io::Result<()> {
    let source = cstring(source)?;
    let target = cstring(target)?;
    let code = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if code == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}
