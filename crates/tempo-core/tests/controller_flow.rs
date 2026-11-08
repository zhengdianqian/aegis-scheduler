// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! v3 integration invariants: the ladder never jumps between the extremes, the
//! writer cap always keeps headroom, and the quiet path has no timer.

use tempo_core::config::Config;
use tempo_core::controller::{Controller, DemandSample, Scene};
use tempo_core::profile::{CapacitySource, ClusterProfile, DeviceProfile, Role};

const MS: u64 = 1_000;

fn profile() -> DeviceProfile {
    let mut clusters = Vec::new();
    let specs = [
        (vec![0, 1, 2, 3], Role::Efficiency, 307_200, 2_016_000, 256, 13usize),
        (vec![4, 5, 6], Role::Balanced, 499_200, 2_803_200, 768, 15),
        (vec![7], Role::Prime, 595_200, 3_360_000, 1024, 17),
    ];
    for (cpus, role, min_khz, max_khz, capacity, steps) in specs {
        let freq_khz = (0..steps)
            .map(|step| min_khz + (max_khz - min_khz) * step as u32 / (steps as u32 - 1))
            .collect();
        clusters.push(ClusterProfile {
            cpus,
            role,
            min_khz,
            max_khz,
            capacity,
            freq_khz,
            capacity_source: CapacitySource::Sysfs,
        });
    }
    DeviceProfile {
        schema: 1,
        platform: "test".to_owned(),
        sdk: 34,
        clusters,
    }
}

fn demand(ts_ms: u64, busy: f32) -> DemandSample {
    DemandSample {
        ts_us: ts_ms * MS,
        cluster_busy: vec![busy; 3],
        foreground_busy: vec![busy; 3],
        foreground_active: busy > 0.0,
    }
}

#[test]
fn load_climbs_in_steps_and_never_starts_at_the_top() {
    let mut controller = Controller::new(profile(), Config::default());
    let mut previous: Vec<usize> = vec![0; 3];
    let mut ts = 100;
    for _ in 0..40 {
        ts += 200;
        let decisions = controller.on_demand(demand(ts, 0.95));
        for decision in decisions {
            let tier = decision.tier as usize;
            assert!(tier >= previous[decision.cluster]);
            assert!(tier - previous[decision.cluster] <= 2, "one step is bounded");
            previous[decision.cluster] = tier;
        }
    }
}

#[test]
fn cap_is_never_below_the_safety_band() {
    let mut controller = Controller::new(profile(), Config::default());
    for _ in 0..30 {
        controller.on_demand(demand(1_000, 0.0));
    }
    for index in 0..3 {
        let cluster = &controller.profile().clusters[index];
        let span = cluster.max_khz - cluster.min_khz;
        assert!(controller.cap_khz(index) >= cluster.min_khz + span * 30 / 100);
        assert!(controller.cap_khz(index) <= cluster.max_khz);
    }
}

#[test]
fn reading_scene_stays_low_and_page_turn_is_one_tier() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_scene(0, Scene::Reading);
    let mut ts = 0;
    for _ in 0..20 {
        ts += 200;
        controller.on_demand(demand(ts, 0.0));
    }
    let before: Vec<usize> = (0..3).map(|i| controller.tier(i)).collect();
    ts += 1;
    controller.on_touch(ts * MS);
    for index in 0..3 {
        assert!(controller.tier(index) <= before[index] + 1);
    }
}

#[test]
fn screen_off_uses_minimum_floor_and_hardware_max_cap() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_screen(0, false);
    for index in 0..3 {
        assert_eq!(controller.ceiling_khz(index), controller.profile().clusters[index].min_khz);
        assert_eq!(controller.cap_khz(index), controller.profile().clusters[index].max_khz);
    }
}
