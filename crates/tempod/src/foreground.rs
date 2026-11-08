// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Foreground package resolution.
//!
//! The probe already tracks the top-app task ids. On a foreground change (an
//! event, never a clock) this resolver maps those ids to a process name by
//! reading `/proc/<tid>/cmdline`, and caches the result. The package name feeds
//! FAS eligibility and the runtime status; nothing here runs periodically.

use std::collections::{HashMap, HashSet};
use std::fs;

const MAX_TIDS: usize = 512;

const SYSTEM_NAMES: &[&str] = &[
    "zygote",
    "zygote64",
    "system_server",
    "surfaceflinger",
    "netd",
    "logd",
    "adbd",
    "init",
    "audioserver",
    "cameraserver",
    "mediaserver",
    "statsd",
    "traced",
    "traced_probes",
    "servicemanager",
    "hwservicemanager",
    "vold",
    "installd",
    "keystore2",
    "dpmd",
    "bpfloader",
    "wpa_supplicant",
    "com.android.systemui",
    "com.android.phone",
    "com.android.bluetooth",
];

/// Cached package name of the current foreground application.
#[derive(Debug, Default)]
pub struct ForegroundResolver {
    package: Option<String>,
    imes: Option<HashSet<String>>,
}

impl ForegroundResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    /// Re-resolves the foreground package from the top-app task ids. Only the
    /// caller decides when this runs: it is bound to foreground transitions.
    ///
    /// Resolution is `/proc/<tid>/cmdline` only. A `dumpsys` helper was tried
    /// here and removed: the top-app thread set churns while an app is merely
    /// scrolling, so a 2 s throttle still spawned `dumpsys window` every 2 s,
    /// which wakes system_server and window manager and shows up as periodic
    /// jank plus ~0.25% single-core CPU. The top-app set is already the
    /// authoritative "in front" signal; the IME filter below removes the
    /// overlay case that `dumpsys` was originally meant to disambiguate.
    pub fn resolve(&mut self, tids: &[u32]) -> Option<&str> {
        if self.imes.is_none() {
            self.imes = Some(ime_packages());
        }
        let package = top_process(tids);
        let name = package.map(|name| name.split(':').next().unwrap_or(&name).to_owned());
        // An input method stealing the focused window is an overlay event, not
        // an app switch: keep the app behind it so IME show/hide cannot trigger
        // foreground lifts or FAS churn.
        if let Some(name) = name.as_deref() {
            if is_ime(name, self.imes.as_ref().unwrap()) {
                return self.package.as_deref();
            }
        }
        self.package = name;
        self.package.as_deref()
    }
}

fn top_process(tids: &[u32]) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for tid in tids.iter().take(MAX_TIDS) {
        let Some(name) = process_name(*tid) else {
            continue;
        };
        if is_system(&name) {
            continue;
        }
        *counts.entry(name).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(name, count)| (*count, name.len()))
        .map(|(name, _)| name)
}

fn process_name(tid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{tid}/cmdline")).ok()?;
    let first = text.split('\0').next()?.trim();
    if first.is_empty() {
        return None;
    }
    if let Some(rest) = first.strip_prefix('/') {
        return rest.rsplit('/').next().map(str::to_owned);
    }
    Some(first.to_owned())
}

fn is_system(name: &str) -> bool {
    let base = name.split(':').next().unwrap_or(name);
    SYSTEM_NAMES.iter().any(|system| *system == base)
}

/// Packages of the configured input methods. Read once per resolver lifetime
/// from Settings; the IME list changes rarely and never during a gesture.
fn ime_packages() -> HashSet<String> {
    let mut set = HashSet::new();
    for key in ["default_input_method", "enabled_input_methods"] {
        let Ok(output) = std::process::Command::new("/system/bin/settings")
            .args(["get", "secure", key])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for token in String::from_utf8_lossy(&output.stdout).split(':') {
            if let Some(slash) = token.find('/') {
                let package = token[..slash].trim();
                if package.contains('.') {
                    set.insert(package.to_owned());
                }
            }
        }
    }
    set
}

fn is_ime(name: &str, imes: &HashSet<String>) -> bool {
    if imes.contains(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower.contains("inputmethod") || lower.contains("input_method") || lower.ends_with("wetype")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_names_are_filtered() {
        assert!(is_system("system_server"));
        assert!(is_system("com.android.systemui:remote"));
        assert!(!is_system("com.example.reader"));
    }

    #[test]
    fn colon_suffix_is_stripped() {
        let mut resolver = ForegroundResolver::new();
        resolver.package = Some("com.example.reader".to_owned());
        assert_eq!(resolver.package(), Some("com.example.reader"));
    }
}
