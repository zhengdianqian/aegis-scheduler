// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 默默的阿文
// Tempo is an independent implementation. No third-party module source is included.

//! Tempo runtime probe.
//!
//! The program accumulates per-cluster run time and foreground run time on
//! every context switch. Once per demand window it evaluates each cluster's
//! demand ratio and wakes the control loop only when the ratio actually moved
//! by a meaningful step. A device whose load is steady — including a fully
//! idle one — produces no events at all, so the daemon stays blocked in
//! `epoll_wait` with no timer running.
//!
//! There is deliberately no sampling period here: the emission decision is
//! derived from observed demand, not from a clock.

#![no_std]
#![no_main]

use aya_ebpf::{
    helpers::{bpf_get_smp_processor_id, bpf_ktime_get_ns},
    macros::{map, tracepoint},
    maps::{Array, HashMap, PerCpuArray, RingBuf},
    programs::TracePointContext,
};

// `trace_event_raw_sched_switch` starts with the 8-byte trace_entry header on
// 64-bit kernels, then prev_comm[16], prev_pid, prev_prio, prev_state,
// next_comm[16], next_pid, next_prio.
const PREV_PID_OFFSET: usize = 24;
const NEXT_PID_OFFSET: usize = 56;

const MAX_CPUS: u32 = 32;
const MAX_CLUSTERS: u32 = 8;
const FG_SLOTS: u32 = 4096;
const RING_BYTES: u32 = 8192;

const REASON_ACTIVE: u32 = 0;

/// Per-cluster accumulator. The monotonic sums describe the cluster's total
/// busy and foreground time; the window fields drive the change detector.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClusterState {
    pub busy_us: u64,
    pub fg_us: u64,
    pub win_start_us: u64,
    pub acc_us: u64,
    pub fg_acc_us: u64,
    pub busy_bp: u32,
    pub fg_bp: u32,
    /// Diagnostic counters: switches attributed to the foreground set and
    /// switches observed at all, per cluster.
    pub fg_switches: u64,
    pub switches: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Params {
    pub quantum_us: u64,
    pub step_bp: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub ts_us: u64,
    pub cpu: u32,
    pub reason: u32,
}

/// Cluster membership, preloaded by the daemon from the device profile.
#[map]
static CPU_CLUSTER: Array<u32> = Array::<u32>::with_max_entries(MAX_CPUS, 0);

/// Cluster width (number of CPUs), preloaded from the device profile.
#[map]
static CLUSTER_CORES: Array<u32> = Array::<u32>::with_max_entries(MAX_CLUSTERS, 0);

#[map]
static CLUSTERS: Array<ClusterState> = Array::<ClusterState>::with_max_entries(MAX_CLUSTERS, 0);

#[map]
static PARAMS: Array<Params> = Array::<Params>::with_max_entries(1, 0);

#[map]
static FG_PIDS: HashMap<u32, u8> = HashMap::<u32, u8>::with_max_entries(FG_SLOTS, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(RING_BYTES, 0);

/// Screen gate, written by the daemon from its display-state event source.
/// While the screen is off the probe only invalidates measurement anchors,
/// skipping demand accounting and publishing no control wake-ups.
#[map]
static SCREEN_ON: Array<u32> = Array::<u32>::with_max_entries(1, 0);

/// Last context-switch timestamp, per CPU.
#[map]
static CPU_TS: PerCpuArray<u64> = PerCpuArray::<u64>::with_max_entries(1, 0);

#[tracepoint(category = "sched", name = "sched_switch")]
pub fn tempo_sched_switch(ctx: TracePointContext) -> u32 {
    match unsafe { handle(&ctx) } {
        Ok(()) => 0,
        Err(_) => 0,
    }
}

unsafe fn handle(ctx: &TracePointContext) -> Result<(), i64> {
    let last_ts = CPU_TS.get_ptr_mut(0).ok_or(0_i64)?;
    let cpu = bpf_get_smp_processor_id();
    let cluster = *CPU_CLUSTER.get(cpu).ok_or(0_i64)?;
    if cluster >= MAX_CLUSTERS {
        return Ok(());
    }
    let state = CLUSTERS.get_ptr_mut(cluster).ok_or(0_i64)?;
    if !screen_on() {
        // Zero marks a fresh CPU baseline on wake; do not charge the sleeping
        // interval or retain a partially accumulated cluster window.
        last_ts.write_volatile(0);
        (*state).win_start_us = 0;
        (*state).acc_us = 0;
        (*state).fg_acc_us = 0;
        (*state).busy_bp = 0;
        (*state).fg_bp = 0;
        return Ok(());
    }

    // `bpf_ktime_get_ns` returns nanoseconds; counters use microseconds.
    let now = bpf_ktime_get_ns() / 1_000;
    let previous = last_ts.read_volatile();
    last_ts.write_volatile(now);
    if previous == 0 || (*state).win_start_us == 0 {
        (*state).win_start_us = now;
        (*state).acc_us = 0;
        (*state).fg_acc_us = 0;
        (*state).busy_bp = 0;
        (*state).fg_bp = 0;
        return Ok(());
    }

    let prev_pid: u32 = ctx.read_at(PREV_PID_OFFSET)?;
    let next_pid: u32 = ctx.read_at(NEXT_PID_OFFSET)?;
    // An idle CPU coming back to life contributes no busy time.
    if prev_pid == 0 {
        let _ = next_pid;
        return Ok(());
    }
    // A sibling CPU may have reset the shared window while this CPU slept.
    let delta = now.saturating_sub(previous.max((*state).win_start_us));
    if delta == 0 {
        return Ok(());
    }
    let params = PARAMS.get(0).ok_or(0_i64)?;
    (*state).busy_us = (*state).busy_us.saturating_add(delta);
    (*state).switches = (*state).switches.saturating_add(1);
    let is_foreground = FG_PIDS.get(&prev_pid).is_some();
    if is_foreground {
        (*state).fg_switches = (*state).fg_switches.saturating_add(1);
        (*state).fg_us = (*state).fg_us.saturating_add(delta);
    }

    if (*state).win_start_us == 0 {
        (*state).win_start_us = now;
        return Ok(());
    }

    let window = now.saturating_sub((*state).win_start_us);
    if window < params.quantum_us {
        (*state).acc_us = (*state).acc_us.saturating_add(delta);
        if is_foreground {
            (*state).fg_acc_us = (*state).fg_acc_us.saturating_add(delta);
        }
        return Ok(());
    }

    // One ratio observation per window, anchored on the previous observation:
    // this is a moving measurement, not a repeating sample. Ratios stay in
    // permille and use 32-bit division on purpose: a 64-bit divide becomes a
    // `__udivdi3` libcall that the loader cannot relocate.
    let acc = (*state).acc_us.saturating_add(delta);
    let fg_acc = (*state).fg_acc_us.saturating_add(if is_foreground { delta } else { 0 });
    let cores = (*CLUSTER_CORES.get(cluster).ok_or(0_i64)?).max(1);
    let acc_ms = (acc / 1_000) as u32;
    let fg_ms = (fg_acc / 1_000) as u32;
    let span_ms = (window / 1_000).max(1) as u32 * cores;
    let busy_bp = acc_ms.saturating_mul(1_000) / span_ms;
    let fg_bp = fg_ms.saturating_mul(1_000) / span_ms;

    let changed = busy_bp.abs_diff((*state).busy_bp) >= params.step_bp
        || fg_bp.abs_diff((*state).fg_bp) >= params.step_bp;
    (*state).win_start_us = now;
    (*state).acc_us = 0;
    (*state).fg_acc_us = 0;
    if changed {
        (*state).busy_bp = busy_bp;
        (*state).fg_bp = fg_bp;
        if screen_on() {
            emit(now, REASON_ACTIVE, cpu);
        }
    }

    Ok(())
}

#[inline(always)]
fn screen_on() -> bool {
    SCREEN_ON.get(0).copied().unwrap_or(0) != 0
}

#[inline(always)]
fn emit(ts_us: u64, reason: u32, cpu: u32) {
    if let Some(mut entry) = EVENTS.reserve::<Event>(0) {
        entry.write(Event {
            ts_us,
            cpu,
            reason,
        });
        entry.submit(0);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
