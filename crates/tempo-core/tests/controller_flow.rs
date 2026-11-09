// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Controller integration tests for frequency bounds, workload events and deadlines.

use tempo_core::config::Config;
use tempo_core::controller::{Controller, DemandSample, FrameSample, Scene};
use tempo_core::profile::{CapacitySource, ClusterProfile, DeviceProfile, Role};

const MS: u64 = 1_000;

fn profile() -> DeviceProfile {
    let mut clusters = Vec::new();
    let specs = [
        (
            vec![0, 1, 2, 3],
            Role::Efficiency,
            307_200,
            2_016_000,
            256,
            13usize,
        ),
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
fn low_load_does_not_amplify_or_latch_a_previous_high_tier() {
    let mut controller = Controller::new(profile(), Config::default());
    for ts in (100..4_000).step_by(200) {
        controller.on_demand(demand(ts, 0.9));
    }
    assert!(controller.tier(0) >= 5);
    for ts in (4_000..8_000).step_by(100) {
        controller.on_demand(demand(ts, 0.08));
    }
    for index in 0..3 {
        assert_eq!(
            controller.tier(index),
            0,
            "8% load must reach its actual target"
        );
    }
    for ts in (8_000..12_000).step_by(100) {
        controller.on_demand(demand(ts, 0.15));
    }
    for index in 0..3 {
        assert!(
            controller.tier(index) <= 1,
            "15% load should need at most one tier"
        );
    }
    let lifted = controller.on_touch(12_100 * MS);
    assert!(lifted[0].tier > 0, "touch must still respond immediately");
}

#[test]
fn total_load_releases_quiet_without_foreground_detection() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_demand(demand(100, 0.0));
    controller.on_deadline(2_000 * MS);
    assert!(controller.is_quiet());
    for ts in [2_100, 2_400, 2_800] {
        let mut sample = demand(ts, 0.85);
        sample.foreground_busy = vec![0.0; 3];
        sample.foreground_active = false;
        controller.on_demand(sample);
        controller.on_deadline((ts + 150) * MS);
        assert!(!controller.is_quiet());
    }
    assert!(controller.tier(0) > 0);
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
            assert!(
                tier - previous[decision.cluster] <= 2,
                "one step is bounded"
            );
            previous[decision.cluster] = tier;
        }
    }
}

#[test]
fn sustained_cpu_demand_reaches_hardware_max_without_frame_feedback() {
    for foreground_known in [false, true] {
        let mut config = Config::default();
        config.control.prime_cap_ratio = 0.30;
        let mut controller = Controller::new(profile(), config);
        for ts in (100..3_000).step_by(160) {
            let mut sample = demand(ts, 0.9);
            if !foreground_known {
                sample.foreground_busy = vec![0.0; 3];
                sample.foreground_active = false;
            }
            controller.on_demand(sample);
        }
        for (index, cluster) in controller.profile().clusters.iter().enumerate() {
            assert_eq!(controller.ceiling_khz(index), cluster.max_khz);
            assert_eq!(controller.cap_khz(index), cluster.max_khz);
        }
        controller.on_thermal_cap(3_000 * MS, Some(0.25));
        for ts in (3_100..5_000).step_by(160) {
            controller.on_demand(demand(ts, 0.95));
        }
        for (index, cluster) in controller.profile().clusters.iter().enumerate() {
            let limit = cluster.min_khz + cluster.span_khz() / 4;
            assert!(controller.ceiling_khz(index) <= controller.cap_khz(index));
            assert!(controller.cap_khz(index) <= limit);
        }
    }
}

#[test]
fn touch_and_isolated_load_noise_do_not_unlock_prime() {
    let mut config = Config::default();
    config.control.prime_cap_ratio = 0.30;
    let mut controller = Controller::new(profile(), config);
    controller.on_demand(demand(100, 0.0));
    controller.on_deadline(2_000 * MS);
    let touch = controller.on_touch(2_100 * MS);
    assert_eq!(touch[2].tier, 0, "touch alone must not lift Prime");
    controller.on_touch_up(2_110 * MS);
    let spike = controller.on_demand(demand(2_200, 0.95));
    assert_eq!(spike[2].tier, 0, "a spike must await confirmation");
    controller.on_demand(demand(2_210, 0.0));
    for ts in (2_250..5_000).step_by(50) {
        controller.on_demand(demand(ts, 0.0));
    }
    assert_eq!(controller.tier(2), 0);
    assert!(controller.cap_khz(2) < controller.profile().clusters[2].max_khz);
}

#[test]
fn prime_cpu_demand_does_not_remove_ime_scene_limit() {
    let config = Config::default();
    let ratio = config.control.ime_cap_ratio;
    let mut controller = Controller::new(profile(), config);
    controller.on_scene(100 * MS, Scene::Ime);
    for ts in (200..3_000).step_by(160) {
        controller.on_demand(demand(ts, 0.95));
    }
    let limit = (ratio * (controller.tier_count(2) - 1) as f32).round() as usize;
    assert_eq!(controller.tier(2), limit);
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
fn sparse_multicluster_launch_raises_caps_then_releases_them() {
    for count in 2..=4 {
        let mut device = profile();
        device.clusters.truncate(count);
        if count == 4 {
            let mut fixed = device.clusters[0].clone();
            fixed.cpus = vec![8];
            fixed.max_khz = fixed.min_khz;
            fixed.freq_khz = vec![fixed.min_khz];
            device.clusters.push(fixed);
        }
        for (index, cluster) in device.clusters.iter_mut().take(3).enumerate() {
            cluster.freq_khz = match index {
                0 => vec![cluster.min_khz, cluster.max_khz],
                1 => vec![cluster.min_khz, 900_000, cluster.max_khz],
                _ => vec![
                    cluster.min_khz,
                    790_000,
                    1_110_000,
                    2_180_000,
                    cluster.max_khz,
                ],
            };
        }
        device.validate().unwrap();
        let mut controller = Controller::new(device, Config::default());
        controller.on_demand(demand(100, 0.0));
        let idle = controller.on_deadline(2_000 * MS);
        let launch = controller.on_app_launch(2_100 * MS);
        assert_eq!(launch.len(), count);
        for ((idle, launch), cluster) in
            idle.iter().zip(&launch).zip(&controller.profile().clusters)
        {
            assert!(launch.ceiling_khz <= launch.cap_khz);
            assert!(cluster.freq_khz.contains(&launch.ceiling_khz));
            assert!(cluster.freq_khz.contains(&launch.cap_khz));
            if cluster.freq_khz.len() > 1 {
                assert!(launch.changed && launch.cap_khz > idle.cap_khz);
                assert!(launch.cap_khz > launch.ceiling_khz);
            }
            if cluster.freq_khz.len() > 2 {
                assert!(launch.cap_khz < cluster.max_khz);
            }
        }
        controller.on_demand(demand(2_200, 0.0));
        controller.on_deadline(3_100 * MS);
        controller.on_deadline(3_200 * MS);
        for previous in idle {
            assert_eq!(controller.cap_khz(previous.cluster), previous.cap_khz);
            assert_eq!(
                controller.ceiling_khz(previous.cluster),
                previous.ceiling_khz
            );
        }
    }
}

#[test]
fn app_launch_foreground_change_and_wake_share_the_warm_start_band() {
    let mut app = Controller::new(profile(), Config::default());
    let mut foreground = Controller::new(profile(), Config::default());
    let mut wake = Controller::new(profile(), Config::default());
    for controller in [&mut app, &mut foreground, &mut wake] {
        controller.on_demand(demand(100, 0.0));
        controller.on_deadline(200 * MS);
        assert!(controller.is_quiet());
    }
    wake.on_screen(250 * MS, false);
    let app_launch = app.on_app_launch(300 * MS);
    let foreground_launch = foreground.on_foreground_changed(300 * MS);
    let wake_launch = wake.on_screen(300 * MS, true);
    for ((app, foreground), wake) in app_launch.iter().zip(foreground_launch).zip(wake_launch) {
        assert_eq!(
            (app.ceiling_khz, app.cap_khz),
            (foreground.ceiling_khz, foreground.cap_khz)
        );
        assert_eq!(
            (app.ceiling_khz, app.cap_khz),
            (wake.ceiling_khz, wake.cap_khz)
        );
        assert!(app.cap_khz > app.ceiling_khz);
    }
}

#[test]
fn thermal_limit_overrides_launch_pressure_quiet_and_screen_off() {
    let mut config = Config::default();
    config.scene.video_floor_tier = 2;
    let mut controller = Controller::new(profile(), config);
    controller.on_app_launch(100 * MS);
    let throttled = controller.on_thermal_cap(101 * MS, Some(0.25));
    assert!(throttled.iter().all(|decision| decision.changed));
    let check = |controller: &Controller| {
        for (index, cluster) in controller.profile().clusters.iter().enumerate() {
            let limit = cluster.min_khz + cluster.span_khz() / 4;
            let floor = controller.ceiling_khz(index);
            let cap = controller.cap_khz(index);
            assert!(floor <= cap && cap <= limit);
            assert!(cluster.freq_khz.contains(&floor) && cluster.freq_khz.contains(&cap));
        }
    };
    check(&controller);
    controller.on_foreground_changed(150 * MS);
    check(&controller);
    for ts in [200, 2_200, 4_200] {
        controller.on_frame(frame(ts, 30.0));
    }
    check(&controller);
    controller.on_screen(4_300 * MS, false);
    check(&controller);
    controller.on_screen(4_400 * MS, true);
    controller.on_scene(4_500 * MS, Scene::Video);
    controller.on_demand(demand(4_600, 0.0));
    controller.on_deadline(6_000 * MS);
    assert!(controller.is_quiet());
    check(&controller);
    controller.on_thermal_cap(6_100 * MS, None);
    let launch = controller.on_app_launch(6_200 * MS);
    assert!(launch
        .iter()
        .all(|decision| decision.cap_khz > decision.ceiling_khz));
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
fn touch_moves_and_deadlines_keep_the_initial_lift_target() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_touch(100 * MS);
    let lifted: Vec<_> = (0..3).map(|i| controller.tier(i)).collect();
    assert_eq!(lifted, vec![1, 1, 0]);
    for ts in (110..1_000).step_by(10) {
        controller.on_touch(ts * MS);
        controller.on_demand(demand(ts, 0.0));
        controller.on_deadline((ts + 1) * MS);
        for index in 0..3 {
            assert_eq!(controller.tier(index), lifted[index]);
        }
    }
    controller.on_deadline(1_200 * MS);
    controller.on_touch(1_300 * MS);
    assert_eq!(controller.tier(0), 1);
}

#[test]
fn long_press_pause_then_move_does_not_stack_lift() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_touch(100 * MS);
    // 长按期间真实需求可以升档，但移动不能在该档位上再叠一档。
    for ts in (200..1_000).step_by(100) {
        controller.on_demand(demand(ts, 0.40));
    }
    let before: Vec<_> = (0..3).map(|i| controller.tier(i)).collect();
    controller.on_touch(1_100 * MS);
    for index in 0..3 {
        assert!(controller.tier(index) <= before[index]);
    }
    controller.on_touch_up(1_200 * MS);
    controller.on_touch(1_300 * MS);
    assert!(!controller.is_quiet());
}

#[test]
fn quiet_entry_deadline_works_without_a_second_demand_sample() {
    let config = Config::default();
    let mut controller = Controller::new(profile(), config.clone());
    controller.on_demand(demand(100, 0.0));
    let deadline = controller.next_deadline(100 * MS).unwrap();
    assert_eq!(deadline.at_us, (100 + config.control.floor_settle_ms) * MS);
    controller.on_deadline(deadline.at_us - 1);
    assert!(!controller.is_quiet());
    controller.on_deadline(deadline.at_us);
    assert!(controller.is_quiet());
    assert!(controller.next_deadline(deadline.at_us).is_none());
}

#[test]
fn quiet_entry_waits_for_launch_window_expiry() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_app_launch(100 * MS);
    controller.on_demand(demand(200, 0.0));
    controller.on_deadline(260 * MS);
    assert!(!controller.is_quiet());
    assert_eq!(
        controller.next_deadline(260 * MS).unwrap().at_us,
        1_100 * MS
    );
    controller.on_deadline(1_100 * MS);
    assert!(controller.is_quiet());
}

#[test]
fn first_frame_shortens_launch_to_eighty_ms() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_app_launch(100 * MS);
    controller.on_app_displayed(200 * MS);
    assert_eq!(controller.next_deadline(200 * MS).unwrap().at_us, 280 * MS);
    controller.on_demand(demand(210, 0.0));
    controller.on_deadline(279 * MS);
    assert!(!controller.is_quiet());
    controller.on_deadline(280 * MS);
    assert!(controller.is_quiet());
}

#[test]
fn first_frame_never_extends_existing_or_expired_deadline() {
    let mut controller = Controller::new(profile(), Config::default());
    assert!(controller.on_app_displayed(50 * MS).is_empty());
    controller.on_app_launch(100 * MS);
    controller.on_app_displayed(1_060 * MS);
    assert_eq!(
        controller.next_deadline(1_060 * MS).unwrap().at_us,
        1_100 * MS
    );
    assert!(controller.on_app_displayed(1_200 * MS).is_empty());
    assert!(controller.next_deadline(1_200 * MS).is_none());
    controller.on_screen(1_300 * MS, false);
    assert!(controller.on_app_displayed(1_400 * MS).is_empty());
    assert!(controller.next_deadline(1_400 * MS).is_none());
}

#[test]
fn repeated_first_frame_cannot_extend_shortened_deadline() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_app_launch(100 * MS);
    controller.on_app_displayed(200 * MS);
    controller.on_app_displayed(250 * MS);
    assert_eq!(controller.next_deadline(250 * MS).unwrap().at_us, 280 * MS);
}

#[test]
fn expired_launch_decays_using_current_time_without_new_demand() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_demand(demand(100, 0.0));
    controller.on_app_launch(200 * MS);
    controller.on_demand(demand(300, 0.0));
    // 前台切换清除静默候选，但保留最后一次发生在启动窗口内的负载时间。
    controller.on_foreground_changed(400 * MS);
    let before = controller.tier(0);
    controller.on_deadline(1_400 * MS);
    assert!(!controller.is_quiet());
    assert_eq!(controller.tier(0), before - 1);
    let next = controller.next_deadline(1_400 * MS).unwrap();
    controller.on_deadline(next.at_us);
    assert_eq!(controller.tier(0), before - 2);
}

fn frame(ts_ms: u64, render_ms: f32) -> FrameSample {
    FrameSample {
        ts_us: ts_ms * MS,
        render_ms,
        budget_ms: 16.7,
        foreground: true,
    }
}

#[test]
fn slow_fas_samples_two_seconds_apart_trigger_pressure() {
    let mut config = Config::default();
    // 保持目标档位低于 cap 饱和区，便于区分普通突发与持续压力。
    config.control.max_up_step = 1;
    let mut controller = Controller::new(profile(), config);
    for ts in [100, 2_100, 4_100] {
        controller.on_frame(frame(ts, 30.0));
    }
    for index in 0..3 {
        assert_eq!(
            controller.cap_tier(index, 4_300 * MS),
            controller.tier_count(index) - 1
        );
    }
    assert!(controller.cap_tier(0, 5_200 * MS) < controller.tier_count(0) - 1);
}

#[test]
fn healthy_or_stale_frame_samples_reset_pressure_confirmation() {
    for (middle_ts, middle_render, last_ts) in [(2_100, 10.0, 4_100), (3_100, 30.0, 5_100)] {
        let mut config = Config::default();
        config.control.max_up_step = 1;
        let mut controller = Controller::new(profile(), config);
        controller.on_frame(frame(100, 30.0));
        controller.on_frame(frame(middle_ts, middle_render));
        controller.on_frame(frame(last_ts, 30.0));
        assert!(controller.cap_tier(0, (last_ts + 200) * MS) < controller.tier_count(0) - 1);
    }
}

#[test]
fn isolated_video_noise_does_not_raise_session_floor() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_scene(0, Scene::Video);
    controller.on_frame(frame(100, 30.0));
    controller.on_frame(frame(2_100, 10.0));
    controller.on_frame(frame(4_100, 30.0));
    controller.on_frame(frame(6_100, 10.0));
    for ts in (7_000..12_000).step_by(100) {
        controller.on_demand(demand(ts, 0.0));
    }
    for index in 0..3 {
        assert_eq!(
            controller.tier(index),
            0,
            "noise must not permanently raise floor"
        );
    }
}

#[test]
fn video_tracks_cpu_demand_without_any_frame_events() {
    let mut video = Controller::new(profile(), Config::default());
    let mut normal = Controller::new(profile(), Config::default());
    video.on_scene(0, Scene::Video);
    for ts in (100..2_000).step_by(200) {
        video.on_demand(demand(ts, 0.45));
        normal.on_demand(demand(ts, 0.45));
    }
    for index in 0..3 {
        assert!(video.tier(index) > 0);
        assert_eq!(video.tier(index), normal.tier(index));
    }
}

#[test]
fn screen_off_uses_minimum_floor_and_hardware_max_cap() {
    let mut controller = Controller::new(profile(), Config::default());
    controller.on_screen(0, false);
    for index in 0..3 {
        assert_eq!(
            controller.ceiling_khz(index),
            controller.profile().clusters[index].min_khz
        );
        assert_eq!(
            controller.cap_khz(index),
            controller.profile().clusters[index].max_khz
        );
    }
}
