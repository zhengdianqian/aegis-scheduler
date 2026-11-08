// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Runtime probe session.
//!
//! The BPF object is embedded at compile time. If it is missing, or loading
//! fails, the daemon reports the reason and disables scheduling: there is no
//! degraded sampling fallback.
//!
//! The session owns the loaded object and the events ring. Its ring fd is
//! registered directly in the control loop's epoll set, so a meaningful demand
//! change costs one wake-up of the single daemon thread rather than a thread
//! hand-off. The probe itself emits only when a cluster's demand ratio moved by
//! a real step, so an idle or steady device keeps the daemon asleep.

use std::collections::HashSet;
use std::fs;
use std::os::unix::io::{AsRawFd, RawFd};

use aya::maps::{Array, HashMap as BpfHashMap, RingBuf};
use aya::programs::TracePoint;
use aya::{Ebpf, EbpfLoader};
use tempo_core::{Config, DeviceProfile};

/// The probe object produced by `scripts/build-probe.ps1`. An empty file means
/// "not built", which is reported as a capability failure at runtime.
const PROBE_OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tempo.bpf.o"));

const EVENT_BYTES: usize = 16;
const MAX_CPUS: u32 = 32;
const UNASSIGNED_CLUSTER: u32 = u32::MAX;
const FOREGROUND_TASKS: &str = "/dev/cpuset/top-app/tasks";
/// Foreground membership is refreshed per accumulated event batch, not on a
/// clock: an idle device never touches the file. The batch gate used to be
/// "32 events **or** 1 s", which a busy top-app crosses on every quantum
/// (hundreds of sched_switch events per 250 ms), so the cpuset file was read
/// four times a second and made `seq_read`/cgroup iteration a visible CPU
/// item. Requiring both bounds caps it at one read per second.
const FOREGROUND_REFRESH_EVENTS: u64 = 32;
const FOREGROUND_REFRESH_US: u64 = 1_000_000;
/// A batch that moves every cluster by less than this is not worth a publish.
/// This is a coalescing threshold, not a sampling period.
const REPORT_DELTA: f32 = 0.02;

/// Mirror of the BPF-side per-cluster accumulator.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ClusterState {
    busy_us: u64,
    fg_us: u64,
    win_start_us: u64,
    acc_us: u64,
    fg_acc_us: u64,
    busy_bp: u32,
    fg_bp: u32,
    fg_switches: u64,
    switches: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    quantum_us: u64,
    step_bp: u32,
    _pad: u32,
}

// The BPF maps use these layouts verbatim; they are plain scalars with no
// padding, so exposing them to the kernel as Pod is sound.
unsafe impl aya::Pod for ClusterState {}
unsafe impl aya::Pod for Params {}

/// One demand snapshot derived from per-cluster counters.
#[derive(Debug, Clone)]
pub struct DemandSnapshot {
    pub ts_us: u64,
    pub busy: Vec<f32>,
    pub foreground: Vec<f32>,
    pub foreground_active: bool,
    pub fg_switches: Vec<u64>,
    pub switches: Vec<u64>,
    pub events: u64,
    pub active_events: u64,
    /// True when the top-app task set changed during this batch. The control
    /// loop resolves the package and re-arms FAS only on this edge.
    pub foreground_changed: bool,
}

struct ClusterShape {
    cpus: Vec<u32>,
}

/// A loaded probe, ready to be registered in the control loop's epoll set.
pub struct ProbeSession {
    ebpf: Ebpf,
    ring: RingBuf<aya::maps::MapData>,
    ring_fd: RawFd,
    cpu_count: usize,
    shape: Vec<ClusterShape>,
    foreground_pids: HashSet<u32>,
    events: u64,
    active_events: u64,
    last_fg_event: u64,
    last_fg_ts_us: u64,
    last_reported_busy: Vec<f32>,
    last_reported_foreground_active: bool,
    foreground_error: Option<String>,
}

impl ProbeSession {
    pub fn open(profile: &DeviceProfile, config: &Config) -> Result<Self, String> {
        if PROBE_OBJECT.is_empty() {
            return Err("probe object was not built; run scripts/build-probe.ps1".to_owned());
        }

        // `include_bytes!` only guarantees alignment 1, while the ELF parser
        // requires the header to be naturally aligned; load from a heap copy.
        let object = PROBE_OBJECT.to_vec();
        let mut ebpf = EbpfLoader::new()
            .load(&object)
            .map_err(|error| format!("load probe object: {}", describe(&error)))?;

        let shape: Vec<ClusterShape> = profile
            .clusters
            .iter()
            .map(|cluster| ClusterShape {
                cpus: cluster.cpus.clone(),
            })
            .collect();
        let cpu_count = profile
            .clusters
            .iter()
            .flat_map(|cluster| cluster.cpus.iter().copied())
            .max()
            .map(|cpu| cpu as usize + 1)
            .unwrap_or(1);

        map_clusters(&mut ebpf, &shape, cpu_count)?;
        let quantum_us = config.events.runtime_quantum_us;
        let step_bp = (config.events.demand_step * 1_000.0)
            .round()
            .clamp(10.0, 200.0) as u32;
        set_params(&mut ebpf, quantum_us, step_bp)?;
        set_screen_on(&mut ebpf, true)?;

        let program: &mut TracePoint = ebpf
            .program_mut("tempo_sched_switch")
            .ok_or("probe program missing")?
            .try_into()
            .map_err(|error| format!("probe program type: {error}"))?;
        program
            .load()
            .map_err(|error| format!("verify probe program: {error}"))?;
        program
            .attach("sched", "sched_switch")
            .map_err(|error| format!("attach sched_switch: {error}"))?;

        let ring = RingBuf::try_from(ebpf.take_map("EVENTS").ok_or("events map missing")?)
            .map_err(|error| format!("open events ring: {error}"))?;
        let ring_fd = ring.as_raw_fd();

        let mut foreground_pids = HashSet::new();
        // Seed the foreground set once; afterwards it follows event batches.
        let foreground_error = refresh_foreground(&mut ebpf, &mut foreground_pids).err();

        Ok(Self {
            ebpf,
            ring,
            ring_fd,
            cpu_count,
            shape,
            foreground_pids,
            events: 0,
            active_events: 0,
            last_fg_event: 0,
            last_fg_ts_us: 0,
            last_reported_busy: Vec::new(),
            last_reported_foreground_active: false,
            foreground_error,
        })
    }

    pub fn foreground_count(&self) -> usize {
        self.foreground_pids.len()
    }

    pub fn foreground_error(&self) -> Option<&str> {
        self.foreground_error.as_deref()
    }

    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    pub fn foreground_ids(&self) -> Vec<u32> {
        self.foreground_pids.iter().copied().collect()
    }

    /// Opens or closes the probe's demand gate. Closing it while the screen is
    /// off keeps the kernel side silent without unloading the program.
    pub fn set_screen(&mut self, on: bool) -> Result<(), String> {
        set_screen_on(&mut self.ebpf, on)
    }

    /// Consumes everything currently in the ring and, when the batch moved a
    /// cluster enough to matter, returns one snapshot for the whole batch.
    pub fn drain(&mut self) -> Option<DemandSnapshot> {
        let Self {
            ebpf,
            ring,
            ring_fd: _,
            cpu_count: _,
            shape,
            foreground_pids,
            events,
            active_events,
            last_fg_event,
            last_fg_ts_us,
            last_reported_busy,
            last_reported_foreground_active,
            foreground_error: _,
        } = self;

        let mut latest_ts_us: Option<u64> = None;
        let mut foreground_changed = false;
        while let Some(item) = ring.next() {
            if item.len() < EVENT_BYTES {
                continue;
            }
            let ts_us = u64::from_le_bytes(item[0..8].try_into().unwrap());
            *events += 1;
            *active_events += 1;
            if ts_us > latest_ts_us.unwrap_or(0) {
                latest_ts_us = Some(ts_us);
            }
            let by_count = events.saturating_sub(*last_fg_event) >= FOREGROUND_REFRESH_EVENTS;
            let by_time = ts_us.saturating_sub(*last_fg_ts_us) >= FOREGROUND_REFRESH_US;
            if by_count && by_time {
                *last_fg_event = *events;
                *last_fg_ts_us = ts_us;
                if refresh_foreground(ebpf, foreground_pids).unwrap_or(false) {
                    foreground_changed = true;
                }
            }
        }

        let ts_us = latest_ts_us?;

        // The probe publishes a quantized demand ratio per cluster; the
        // control loop consumes that observation directly instead of
        // re-deriving a ratio over an arbitrary user-space interval.
        let counters = Array::<_, ClusterState>::try_from(ebpf.map("CLUSTERS")?)
            .map_err(|error| format!("open cluster counters: {error}"))
            .ok()?;

        let mut cluster_busy = Vec::with_capacity(shape.len());
        let mut cluster_foreground = Vec::with_capacity(shape.len());
        let mut fg_switches = Vec::with_capacity(shape.len());
        let mut switches = Vec::with_capacity(shape.len());
        for index in 0..shape.len() {
            match counters.get(&(index as u32), 0) {
                Ok(state) => {
                    cluster_busy.push(state.busy_bp as f32 / 1_000.0);
                    cluster_foreground.push(state.fg_bp as f32 / 1_000.0);
                    fg_switches.push(state.fg_switches);
                    switches.push(state.switches);
                }
                Err(_) => {
                    cluster_busy.push(0.0);
                    cluster_foreground.push(0.0);
                    fg_switches.push(0);
                    switches.push(0);
                }
            }
        }

        let foreground_active = !foreground_pids.is_empty();

        // Keep the counters baseline moving even when nothing is published, so
        // a later report always covers a short window instead of a long one.
        let significant = last_reported_busy.len() != cluster_busy.len()
            || cluster_busy
                .iter()
                .zip(last_reported_busy.iter())
                .any(|(now, before)| (now - before).abs() >= REPORT_DELTA)
            || foreground_active != *last_reported_foreground_active
            || foreground_changed;
        if !significant {
            return None;
        }
        last_reported_busy.clone_from(&cluster_busy);
        *last_reported_foreground_active = foreground_active;

        Some(DemandSnapshot {
            ts_us,
            busy: cluster_busy,
            foreground: cluster_foreground,
            foreground_active,
            fg_switches,
            switches,
            events: *events,
            active_events: *active_events,
            foreground_changed,
        })
    }
}

impl mio::event::Source for ProbeSession {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> std::io::Result<()> {
        registry.register(&mut mio::unix::SourceFd(&mut self.ring_fd), token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> std::io::Result<()> {
        registry.reregister(&mut mio::unix::SourceFd(&mut self.ring_fd), token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> std::io::Result<()> {
        registry.deregister(&mut mio::unix::SourceFd(&mut self.ring_fd))
    }
}

/// Publishes the device profile's CPU-to-cluster layout to the probe.
fn map_clusters(ebpf: &mut Ebpf, shape: &[ClusterShape], cpu_count: usize) -> Result<(), String> {
    let mut map: Array<_, u32> = Array::try_from(
        ebpf.map_mut("CPU_CLUSTER")
            .ok_or("cluster map missing")?,
    )
    .map_err(|error| format!("open cluster map: {error}"))?;
    for cpu in 0..(cpu_count.min(MAX_CPUS as usize)) {
        map.set(cpu as u32, UNASSIGNED_CLUSTER, 0)
            .map_err(|error| format!("write cluster map: {error}"))?;
    }
    for (index, cluster) in shape.iter().enumerate() {
        for &cpu in &cluster.cpus {
            if (cpu as usize) < cpu_count.min(MAX_CPUS as usize) {
                map.set(cpu, index as u32, 0)
                    .map_err(|error| format!("write cluster map: {error}"))?;
            }
        }
    }
    drop(map);

    let mut cores: Array<_, u32> = Array::try_from(
        ebpf.map_mut("CLUSTER_CORES")
            .ok_or("cluster width map missing")?,
    )
    .map_err(|error| format!("open cluster width map: {error}"))?;
    for (index, cluster) in shape.iter().enumerate() {
        cores
            .set(index as u32, cluster.cpus.len().max(1) as u32, 0)
            .map_err(|error| format!("write cluster width: {error}"))?;
    }
    Ok(())
}

fn set_params(ebpf: &mut Ebpf, quantum_us: u64, step_bp: u32) -> Result<(), String> {
    let params = Params {
        quantum_us: quantum_us.max(1_000),
        step_bp: step_bp.max(10),
        _pad: 0,
    };
    let mut map: Array<_, Params> =
        Array::try_from(ebpf.map_mut("PARAMS").ok_or("params map missing")?)
            .map_err(|error| format!("open params map: {error}"))?;
    map.set(0, params, 0)
        .map_err(|error| format!("write params: {error}"))
}

/// Publishes the display state to the probe gate. With the screen off the
/// probe stops emitting demand events, so the daemon can stay blocked.
pub fn set_screen_on(ebpf: &mut Ebpf, on: bool) -> Result<(), String> {
    let mut map: Array<_, u32> = Array::try_from(
        ebpf.map_mut("SCREEN_ON")
            .ok_or("screen gate map missing")?,
    )
    .map_err(|error| format!("open screen gate: {error}"))?;
    map.set(0, u32::from(on), 0)
        .map_err(|error| format!("write screen gate: {error}"))
}

fn refresh_foreground(ebpf: &mut Ebpf, known: &mut HashSet<u32>) -> Result<bool, String> {
    let text = match fs::read_to_string(FOREGROUND_TASKS) {
        Ok(text) => text,
        Err(_) => return Ok(false),
    };
    let current: HashSet<u32> = text
        .split_whitespace()
        .filter_map(|part| part.parse::<u32>().ok())
        .collect();
    if current == *known {
        return Ok(false);
    }

    let mut map: BpfHashMap<_, u32, u8> =
        BpfHashMap::try_from(ebpf.map_mut("FG_PIDS").ok_or("foreground map missing")?)
            .map_err(|error| format!("open foreground map: {error}"))?;
    for pid in known.difference(&current) {
        map.remove(pid)
            .map_err(|error| format!("drop foreground pid {pid}: {error}"))?;
    }
    for pid in current.difference(known) {
        map.insert(*pid, 1u8, 0)
            .map_err(|error| format!("track foreground pid {pid}: {error}"))?;
    }
    *known = current;
    Ok(true)
}

/// Renders an error together with its whole cause chain, so device logs name
/// the real reason instead of only the outermost wrapper.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str(" <- ");
        text.push_str(&format!("{inner:?}"));
        source = inner.source();
    }
    text
}
