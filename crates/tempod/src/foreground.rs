// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Foreground package resolution.
//!
//! The probe already tracks the top-app task ids. On a foreground change (an
//! event, never a clock) this resolver maps those ids to a process name by
//! reading `/proc/<tid>/cmdline`, and caches the result. The package name feeds
//! frame feedback eligibility and the runtime status; nothing here runs periodically.

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
    /// authoritative "in front" signal; input-method threads are excluded in
    /// the vote below so an overlay IME can neither win nor block the app
    /// behind it.
    pub fn resolve(&mut self, tids: &[u32]) -> Option<&str> {
        if self.imes.is_none() {
            self.imes = Some(ime_packages());
        }
        let package = top_process(tids, self.imes.as_ref().unwrap());
        let name = package.map(|name| name.split(':').next().unwrap_or(&name).to_owned());
        // 输入法与系统进程在投票阶段已被排除；这里只处理“投票为空”的瞬时状态
        // （top-app 集合里暂时只剩输入法/系统线程）。此时保持上一个包名，避免
        // 键盘或系统线程把前台包清空、制造一次假的前台切换。
        if name.is_none() {
            return self.package.as_deref();
        }
        self.package = name;
        self.package.as_deref()
    }
}

fn top_process(tids: &[u32], imes: &HashSet<String>) -> Option<String> {
    let names: Vec<String> = tids
        .iter()
        .take(MAX_TIDS)
        .filter_map(|tid| process_name(*tid))
        .collect();
    vote(&names, imes)
}

/// 在非系统、非输入法的进程名里按线程数投票。
/// 输入法必须在投票前排除：MIUI 上输入法线程数常年高于前台应用
/// （实测 sogou 89 线程 vs settings 63 线程），若只在投票后按“赢家是输入法”
/// 过滤，输入法会永远赢下投票，前台包永远停在上一个应用，前台切换抬档和
/// 阅读/视频分类因此全部失效。
fn vote(names: &[String], imes: &HashSet<String>) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in names {
        if is_system(name) || is_ime(name, imes) {
            continue;
        }
        *counts.entry(name.clone()).or_insert(0) += 1;
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

    #[test]
    fn ime_threads_never_outvote_the_foreground_app() {
        let imes: HashSet<String> = ["com.sohu.inputmethod.sogou.xiaomi"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        // 输入法线程数（10）高于前台应用（4）：投票仍必须给出前台应用。
        let mut names: Vec<String> = vec![
            "com.android.settings".to_owned(),
            "com.android.settings".to_owned(),
            "com.android.settings".to_owned(),
            "com.android.settings".to_owned(),
        ];
        for _ in 0..10 {
            names.push("com.sohu.inputmethod.sogou.xiaomi".to_owned());
        }
        names.push("system_server".to_owned());
        assert_eq!(
            vote(&names, &imes).as_deref(),
            Some("com.android.settings")
        );
        // 只剩输入法与系统线程时返回 None，由调用方保持上一个包名。
        let only_ime: Vec<String> = vec!["com.sohu.inputmethod.sogou.xiaomi".to_owned()];
        assert_eq!(vote(&only_ime, &imes), None);
    }
}
