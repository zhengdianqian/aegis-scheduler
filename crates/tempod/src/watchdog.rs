// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! AegisAuto 安全看门狗。
//!
//! 三层防线，覆盖接管期间所有已知的失败方式：
//!
//! 1. **进程内心跳线程**：每秒向主循环发一次 `SIGUSR1`（signalfd 唤醒主
//!    循环并刷新心跳）。主调频循环卡死超过 `STALL` 时，线程按启动备份执行
//!    紧急回退（恢复 scaling_governor + min/max）后退出进程；shell 侧
//!    `watchdog.sh` 随后确认现场。
//! 2. **panic 钩子**：任意线程 panic 时先执行同样的紧急回退（release 构建
//!    `panic = "abort"`，钩子仍会先运行），并写入干净退出标记避免重复恢复。
//! 3. **可捕获信号**：SIGTERM/SIGINT 走事件循环的正常析构路径（writer
//!    恢复）；SIGKILL 无法捕获，由 shell `watchdog.sh` + `restore.sh` 兜底。
//!
//! `prctl(PR_SET_PDEATHSIG)` 不适用于本守护：它的父进程（service.sh）在
//! 启动后约 90 秒正常退出，PDEATHSIG 会把守护一并杀掉；父进程死亡场景由
//! shell 看门狗与开机 guard 覆盖。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::writer::{self, EmergencyPolicy};

/// 主事件循环允许的最大无心跳时长。
const STALL: Duration = Duration::from_secs(5);
/// 心跳线程每秒拍一次。
const BEAT: Duration = Duration::from_secs(1);

static HEARTBEAT_US: AtomicU64 = AtomicU64::new(0);
static EMERGENCY_DONE: AtomicBool = AtomicBool::new(false);

struct Restore {
    policies: Vec<EmergencyPolicy>,
    marker: PathBuf,
    log: PathBuf,
}

static RESTORE: OnceLock<Restore> = OnceLock::new();

/// 注册紧急回退所需的启动备份与状态文件路径。只注册一次（首次发现 writer）。
pub fn register(policies: Vec<EmergencyPolicy>, run_dir: PathBuf, log_path: PathBuf) {
    if policies.is_empty() {
        return;
    }
    let _ = RESTORE.set(Restore {
        policies,
        marker: run_dir.join("clean-exit"),
        log: log_path,
    });
}

/// 主循环每次醒来调用：刷新心跳时间戳。
pub fn beat() {
    HEARTBEAT_US.store(crate::event_loop::now_us(), Ordering::Relaxed);
}

/// 执行紧急回退（幂等）。成功后写入干净退出标记，shell 看门狗不再重复恢复。
fn emergency(reason: &str) -> bool {
    if EMERGENCY_DONE.swap(true, Ordering::SeqCst) {
        return false;
    }
    let Some(state) = RESTORE.get() else {
        return false;
    };
    let outcome = writer::emergency_restore(&state.policies);
    let detail = match &outcome {
        Ok(writes) => format!("aegis: emergency restore ok ({writes} nodes): {reason}\n"),
        Err(problem) => format!("aegis: emergency restore FAILED: {reason}: {problem}\n"),
    };
    // 尽力记录与落标记；任何文件系统失败都不影响已执行的 sysfs 回退。
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.log)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(detail.as_bytes())
        });
    if outcome.is_ok() {
        let _ = fs::write(&state.marker, b"aegis-clean-exit-v1\nwriter_restored=1\n");
    }
    outcome.is_ok()
}

/// 安装 panic 钩子：任何线程 panic 都先按备份回退 governor 与频率区间。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emergency(&format!("panic: {info}"));
        previous(info);
    }));
}

/// spawn 独立心跳看门狗线程。
///
/// 线程每秒 `kill(pid, SIGUSR1)` 唤醒主循环（事件循环收到后刷新心跳并继续）；
/// 心跳停摆超过 `STALL` 判定主循环卡死 → 紧急回退 → 进程退出。
pub fn spawn_heartbeat_thread() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("aegis-watchdog".to_owned())
        .spawn(|| loop {
            std::thread::sleep(BEAT);
            // 阻塞在 signalfd 上的主循环会被该信号唤醒（不改变退出语义）。
            unsafe {
                libc::kill(libc::getpid(), libc::SIGUSR1);
            }
            let last = HEARTBEAT_US.load(Ordering::Relaxed);
            if last == 0 {
                continue;
            }
            let age = crate::event_loop::now_us().saturating_sub(last);
            if age > STALL.as_micros() as u64 {
                emergency(&format!(
                    "heartbeat stale for {} ms; main loop stuck",
                    age / 1_000
                ));
                // 已回退；结束进程，由 shell 看门狗确认退出现场。
                std::process::exit(70);
            }
        })
        .map(|_| ())
}
