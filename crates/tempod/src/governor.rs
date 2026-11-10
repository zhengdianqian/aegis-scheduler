// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! 接管厂商 walt governor 的换档节奏。
//!
//! 只写 `walt/pl` 和 `walt/{up,down}_rate_limit_us`，不碰 `scaling_min_freq`
//! / `scaling_max_freq`，因此不构成任何频率上限，也不改变调度器自己的档位表。
//!
//! 实测（红果播放，ftrace `power:cpu_frequency`，10 秒窗口）：
//! 厂商标配（`pl=1` + 不限速）中核换档 300~750 次，占用只有 7% 时也会反复落到
//! 最高频；关闭预测并加 2ms 升 / 8ms 降的节流后换档降到 140~310 次，加权平均
//! 频率不升反降，Settings 滑动仍为 0 掉帧。

use std::fs;
use std::path::Path;

use tempo_core::config::GovernorConfig;
use tempo_core::profile::DeviceProfile;

use crate::writer::policy_dir;

/// 启动时记录厂商 walt 原值；关闭接管时写回，保证“关闭 = 完全不动厂商设置”。
pub struct Snapshot {
    nodes: Vec<(std::path::PathBuf, u64)>,
}

const WALT_NODES: [&str; 3] = ["pl", "up_rate_limit_us", "down_rate_limit_us"];

/// 在写入任何值之前抓取原值。
pub fn capture(profile: &DeviceProfile) -> Snapshot {
    let mut nodes = Vec::new();
    for cluster in &profile.clusters {
        let Some(cpu) = cluster.cpus.first() else {
            continue;
        };
        let Some(dir) = policy_dir(*cpu) else {
            continue;
        };
        let walt = dir.join("walt");
        if !walt.is_dir() {
            continue;
        }
        for name in WALT_NODES {
            let path = walt.join(name);
            if let Ok(text) = fs::read_to_string(&path) {
                if let Ok(value) = text.trim().parse::<u64>() {
                    nodes.push((path, value));
                }
            }
        }
    }
    Snapshot { nodes }
}

/// 关闭接管时把原值写回；值与快照一致时不重复写。
pub fn restore(snapshot: &Snapshot) -> usize {
    snapshot
        .nodes
        .iter()
        .filter(|(path, value)| write_node(path, *value) == 1)
        .count()
}

/// 把平滑参数写到每个簇的 walt 目录。节点不存在时静默跳过，兼容非 walt 机型。
/// 返回实际写入的节点数。
pub fn apply(profile: &DeviceProfile, governor: &GovernorConfig) -> usize {
    if !governor.smooth {
        return 0;
    }
    let prediction = u64::from(governor.prediction);
    let mut written = 0;
    for cluster in &profile.clusters {
        let Some(cpu) = cluster.cpus.first() else {
            continue;
        };
        let Some(dir) = policy_dir(*cpu) else {
            continue;
        };
        let walt = dir.join("walt");
        if !walt.is_dir() {
            continue;
        }
        written += write_node(&walt.join("pl"), prediction);
        written += write_node(&walt.join("up_rate_limit_us"), governor.up_rate_limit_us);
        written += write_node(&walt.join("down_rate_limit_us"), governor.down_rate_limit_us);
    }
    written
}

fn write_node(path: &Path, value: u64) -> usize {
    // 值已一致就不重复写，避免无意义的 sysfs 唤醒。
    if let Ok(current) = fs::read_to_string(path) {
        if current.trim() == value.to_string() {
            return 0;
        }
    }
    match fs::write(path, value.to_string()) {
        Ok(()) => 1,
        Err(error) => {
            // 只读节点或 SELinux 拒绝时保持厂商原样，不重试。
            tracing::debug!(path = %path.display(), %error, "governor node skipped");
            0
        }
    }
}
