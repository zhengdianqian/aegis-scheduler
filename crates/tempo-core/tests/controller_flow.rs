// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use tempo_core::controller::{
    Controller, DeadlineKind, Decision, DecisionReason, DemandSample, FrameSample,
};
use tempo_core::{Config, DeviceProfile};

const US: u64 = 1_000;

const PROFILE_THREE: &str = r#"
schema = 1
platform = "test-three"
sdk = 36

[[cluster]]
cpus = [0, 1, 2]
role = "efficiency"
min_khz = 307200
max_khz = 2016000
capacity = 266
freq_khz = [307200, 441600, 576000, 691200, 787200, 960000, 1171200, 1344000, 1516800, 1651200, 1785600, 1920000, 2016000]

[[cluster]]
cpus = [3, 4, 5, 6]
role = "balanced"
min_khz = 499200
max_khz = 2803200
capacity = 811
freq_khz = [499200, 595200, 672000, 787200, 883200, 979200, 1113600, 1267200, 1382400, 1516800, 1651200, 1785600, 1920000, 2112000, 2227200, 2400000, 2553600, 2688000, 2803200]

[[cluster]]
cpus = [7]
role = "prime"
min_khz = 595200
max_khz = 3360000
capacity = 1024
freq_khz = [595200, 672000, 787200, 864000, 960000, 1056000, 1113600, 1209600, 1324800, 1420800, 1555200, 1670400, 1766400, 1881600, 1996800, 2131200, 2246400, 2361600, 2496000, 2611200, 2764800, 2880000, 2995200, 3134400, 3216000, 3360000]
"#;

const PROFILE_TWO: &str = r#"
schema = 1
platform = "test-two"
sdk = 36

[[cluster]]
cpus = [0, 1, 2, 3]
role = "efficiency"
min_khz = 300000
max_khz = 1800000
capacity = 300
freq_khz = [300000, 600000, 900000, 1200000, 1500000, 1800000]

[[cluster]]
cpus = [4, 5, 6, 7]
role = "balanced"
min_khz = 500000
max_khz = 2400000
capacity = 700
freq_khz = [500000, 800000, 1100000, 1400000, 1700000, 2000000, 2400000]
"#;

const PROFILE_FOUR: &str = r#"
schema = 1
platform = "test-four"
sdk = 36

[[cluster]]
cpus = [0, 1]
role = "efficiency"
min_khz = 300000
max_khz = 1500000
capacity = 200
freq_khz = [300000, 600000, 900000, 1200000, 1500000]

[[cluster]]
cpus = [2, 3]
role = "balanced"
min_khz = 400000
max_khz = 2000000
capacity = 500
freq_khz = [400000, 800000, 1200000, 1600000, 2000000]

[[cluster]]
cpus = [4, 5]
role = "balanced"
min_khz = 500000
max_khz = 2400000
capacity = 700
freq_khz = [500000, 900000, 1300000, 1700000, 2000000, 2200000, 2400000]

[[cluster]]
cpus = [6, 7]
role = "prime"
min_khz = 600000
max_khz = 3000000
capacity = 1000
freq_khz = [600000, 1000000, 1400000, 1800000, 2200000, 2600000, 3000000]
"#;

fn profile(toml_text: &str) -> DeviceProfile {
    let profile: DeviceProfile = toml::from_str(toml_text).expect("profile parses");
    profile.validate().expect("profile valid");
    profile
}

fn three() -> Controller {
    Controller::new(profile(PROFILE_THREE), Config::default())
}

fn demand(ts_ms: u64, busy: f32, fg: f32) -> DemandSample {
    DemandSample {
        ts_us: ts_ms * US,
        cluster_busy: vec![busy; 3],
        foreground_busy: vec![fg; 3],
        foreground_active: fg > 0.0,
    }
}

fn raised(decisions: &[Decision], controller: &Controller) -> Vec<Decision> {
    decisions
        .iter()
        .filter(|d| d.changed && d.ceiling_khz > controller.profile().clusters[d.cluster].min_khz)
        .cloned()
        .collect()
}

/// Fires every pending one-shot until the controller has nothing left to
/// schedule (staged lift stages, quiet entry and staged decay).
fn settle(controller: &mut Controller, mut now_us: u64) -> usize {
    let mut changes = 0usize;
    for _ in 0..64 {
        let Some(deadline) = controller.next_deadline(now_us) else {
            break;
        };
        now_us = deadline.at_us;
        let decisions = controller.on_deadline(now_us);
        changes += decisions.iter().filter(|decision| decision.changed).count();
    }
    changes
}

fn quiet_controller_at_150ms() -> Controller {
    let mut controller = three();
    controller.on_screen(0, true);
    controller.on_demand(demand(5, 0.60, 0.0));
    controller.on_deadline(150 * US);
    assert!(controller.is_quiet(), "quiet path must engage");
    settle(&mut controller, 150 * US);
    controller
}

#[test]
fn quiet_path_reaches_min_freq() {
    let mut controller = three();
    controller.on_screen(0, true);
    controller.on_demand(demand(5, 0.60, 0.0));

    let deadline = controller.next_deadline(5 * US).expect("ramp deadline");
    assert_eq!(deadline.kind, DeadlineKind::Ramp);
    assert_eq!(deadline.at_us, 20 * US, "first ramp stage after 20 ms");

    let mut now = deadline.at_us;
    let mut at_lift_end = Vec::new();
    loop {
        let next = controller.next_deadline(now).expect("pending window");
        now = next.at_us;
        let decisions = controller.on_deadline(now);
        if next.kind == DeadlineKind::LiftEnd {
            at_lift_end = decisions;
            break;
        }
    }
    assert!(controller.is_quiet());
    assert_eq!(at_lift_end[0].reason, DecisionReason::Quiet);

    let changes = settle(&mut controller, now);
    assert!(
        changes > 3,
        "the release must walk down in stages, not slam to min"
    );
    for (index, cluster) in controller.profile().clusters.iter().enumerate() {
        assert_eq!(controller.ceiling_khz(index), cluster.min_khz);
    }
    assert!(
        controller.next_deadline(now).is_none(),
        "once the floor is reached no timer stays armed"
    );
}

#[test]
fn quiet_path_no_raise_for_ten_minutes() {
    let mut controller = quiet_controller_at_150ms();
    let minima: Vec<u32> = controller
        .profile()
        .clusters
        .iter()
        .map(|cluster| cluster.min_khz)
        .collect();

    let mut raised_count = 0usize;
    for ts_ms in (150..600_000).step_by(20) {
        let decisions = controller.on_demand(demand(ts_ms, 0.60, 0.0));
        for decision in &decisions {
            if decision.changed && decision.ceiling_khz > minima[decision.cluster] {
                raised_count += 1;
            }
        }
        assert!(
            controller.next_deadline(ts_ms * US).is_none(),
            "no deadline while quiet"
        );
    }
    assert_eq!(
        raised_count, 0,
        "static background load must not raise ceilings"
    );
}

#[test]
fn static_micro_motion_enters_and_holds_the_floor() {
    let mut controller = three();
    controller.on_screen(0, true);
    // A static reading page shows a narrow 3-4% share on a 3-core cluster.
    controller.on_demand(demand(5, 0.04, 0.04));
    controller.on_deadline(150 * US);
    assert!(controller.is_quiet(), "micro-motion must still reach the floor");
    settle(&mut controller, 150 * US);

    let minima: Vec<u32> = controller
        .profile()
        .clusters
        .iter()
        .map(|cluster| cluster.min_khz)
        .collect();
    for step in 0..600u64 {
        let ts_ms = 200 + step * 20;
        let decisions = controller.on_demand(demand(ts_ms, 0.04, 0.04));
        assert!(controller.is_quiet(), "micro-motion must not break quiet");
        assert!(
            controller.next_deadline(ts_ms * US).is_none(),
            "a quiet floor must arm no timer"
        );
        for decision in &decisions {
            assert_eq!(decisions.len(), 3);
            assert_eq!(decision.ceiling_khz, minima[decision.cluster]);
        }
    }
}

#[test]
fn real_burst_leaves_the_floor_after_the_exit_hold() {
    let mut controller = quiet_controller_at_150ms();

    let burst = controller.on_demand(demand(1_000, 0.30, 0.30));
    assert!(
        controller.is_quiet(),
        "one above-exit sample must not yank the floor immediately"
    );

    let deadline = controller.next_deadline(1_000 * US).expect("floor exit");
    assert_eq!(deadline.kind, DeadlineKind::FloorExit);
    assert_eq!(deadline.at_us, 1_060 * US);

    let decisions = controller.on_deadline(1_060 * US);
    assert!(!controller.is_quiet(), "sustained burst must leave the floor");
    assert!(!raised(&decisions, &controller).is_empty());
}

#[test]
fn isolated_spike_is_filtered_while_micro_motion_continues() {
    let mut controller = quiet_controller_at_150ms();

    controller.on_demand(demand(1_000, 0.30, 0.30));
    assert!(controller.is_quiet(), "one spike must not break quiet");
    let deadline = controller.next_deadline(1_000 * US).expect("exit candidate");
    assert_eq!(deadline.kind, DeadlineKind::FloorExit);

    controller.on_demand(demand(1_010, 0.04, 0.04));
    controller.on_demand(demand(1_030, 0.04, 0.04));
    let decisions = controller.on_deadline(1_060 * US);
    assert!(
        controller.is_quiet(),
        "the latest observation returned to micro-motion"
    );
    assert!(controller.next_deadline(1_060 * US).is_none());
    for decision in &decisions {
        assert_eq!(
            decision.ceiling_khz,
            controller.profile().clusters[decision.cluster].min_khz
        );
    }

    for step in 0..100u64 {
        let ts_ms = 1_100 + step * 20;
        let calm = controller.on_demand(demand(ts_ms, 0.04, 0.04));
        assert!(controller.is_quiet());
        assert!(calm
            .iter()
            .all(|decision| decision.ceiling_khz
                == controller.profile().clusters[decision.cluster].min_khz));
    }
}

#[test]
fn touch_immediately_leaves_the_floor() {
    let mut controller = quiet_controller_at_150ms();
    let lift = controller.on_touch(1_000 * US);
    assert!(!controller.is_quiet());
    assert_eq!(raised(&lift, &controller).len(), 2);
    let deadline = controller
        .next_deadline(1_000 * US)
        .expect("staged lift must arm a timer");
    assert_eq!(deadline.kind, DeadlineKind::Ramp);
    assert_eq!(deadline.at_us, 1_020 * US);
}

#[test]
fn touch_lift_ramps_then_settles_stage_by_stage() {
    let mut controller = quiet_controller_at_150ms();

    let lift = controller.on_touch(1_000 * US);
    assert!(!controller.is_quiet());
    assert_eq!(
        raised(&lift, &controller).len(),
        2,
        "touch lift applies to efficiency/balanced only"
    );
    assert_eq!(
        controller.ceiling_khz(2),
        595_200,
        "prime stays at min on touch"
    );
    // First stage is a third of the target, not the top of the cluster.
    let stage1 = controller.ceiling_khz(1);
    assert!(
        stage1 > 499_200 && stage1 < 1_516_800,
        "first stage must stay in the middle of the band, got {stage1}"
    );

    // Climb one stage every 20 ms and reach the target after 60 ms.
    controller.on_deadline(1_020 * US);
    let stage2 = controller.ceiling_khz(1);
    assert!(stage2 > stage1, "stage1={stage1} stage2={stage2}");
    controller.on_deadline(1_040 * US);
    assert_eq!(controller.ceiling_khz(1), 1_516_800, "target after 60 ms");

    let deadline = controller.next_deadline(1_040 * US).expect("lift end");
    assert_eq!(deadline.kind, DeadlineKind::LiftEnd);
    assert_eq!(deadline.at_us, 1_150 * US);

    controller.on_deadline(1_150 * US);
    assert!(controller.is_quiet());
    let changes = settle(&mut controller, 1_150 * US);
    assert!(
        changes >= 4,
        "release must fall in several steps, got {changes} changes"
    );
    for (index, cluster) in controller.profile().clusters.iter().enumerate() {
        assert_eq!(controller.ceiling_khz(index), cluster.min_khz);
    }
}

#[test]
fn frame_overrun_triggers_burst_then_settles() {
    let mut controller = quiet_controller_at_150ms();
    // A frame overrun on a static page: no load-driven lift competes with the
    // burst, so the observed ceiling is exactly the staged burst ceiling.
    controller.on_demand(demand(1_900, 0.04, 0.04));

    let burst = controller.on_frame(FrameSample {
        ts_us: 2_000 * US,
        render_ms: 25.0,
        budget_ms: 16.7,
        foreground: true,
    });
    assert!(!controller.is_quiet());
    let lifted = raised(&burst, &controller);
    assert_eq!(lifted.len(), 3, "frame burst applies to every cluster");
    assert!(
        controller.ceiling_khz(2) < 2_880_000,
        "the first burst stage must not jump to the top"
    );

    let deadline = controller.next_deadline(2_000 * US).expect("burst ramp");
    assert_eq!(deadline.kind, DeadlineKind::Ramp);
    assert_eq!(deadline.at_us, 2_020 * US);
    controller.on_deadline(2_020 * US);
    controller.on_deadline(2_040 * US);
    assert_eq!(
        controller.ceiling_khz(2),
        2_496_000,
        "burst peak is 0.70 of the span, reached after 60 ms"
    );

    let end = controller.next_deadline(2_040 * US).expect("burst end");
    assert_eq!(end.kind, DeadlineKind::BurstEnd);
    assert_eq!(end.at_us, 2_150 * US);
    controller.on_deadline(2_150 * US);
    assert!(!controller.is_quiet());

    controller.on_demand(demand(2_200, 0.04, 0.04));
    let mut now = 2_200 * US;
    loop {
        let deadline = controller.next_deadline(now).expect("quiet entry");
        now = deadline.at_us;
        controller.on_deadline(now);
        if deadline.kind == DeadlineKind::QuietEntry {
            break;
        }
    }
    assert!(controller.is_quiet());

    settle(&mut controller, now);
    assert_eq!(controller.ceiling_khz(0), 307_200);
    assert_eq!(controller.ceiling_khz(2), 595_200);
}

#[test]
fn on_time_frame_does_not_raise_or_leave_quiet() {
    let mut controller = quiet_controller_at_150ms();
    let decisions = controller.on_frame(FrameSample {
        ts_us: 1_000 * US,
        render_ms: 9.0,
        budget_ms: 16.7,
        foreground: true,
    });
    assert!(decisions.iter().all(|d| !d.changed));
    assert!(controller.is_quiet());
}

#[test]
fn high_load_respects_role_caps_and_thermal_cap() {
    let mut controller = three();
    controller.on_screen(0, true);
    controller.on_demand(demand(200, 1.0, 1.0));

    assert_eq!(controller.ceiling_khz(0), 2_016_000);
    assert_eq!(controller.ceiling_khz(1), 2_803_200);
    assert_eq!(controller.ceiling_khz(2), 2_880_000);

    let thermal = controller.on_thermal_cap(300 * US, Some(0.50));
    assert_eq!(controller.ceiling_khz(0), 960_000);
    assert_eq!(controller.ceiling_khz(2), 1_881_600);
    assert!(thermal
        .iter()
        .any(|decision| decision.reason == DecisionReason::Thermal));

    controller.on_thermal_cap(400 * US, None);
    assert_eq!(controller.ceiling_khz(0), 2_016_000);
    assert_eq!(controller.ceiling_khz(2), 2_880_000);
}

#[test]
fn foreground_change_warm_starts_all_clusters() {
    let mut controller = quiet_controller_at_150ms();
    let decisions = controller.on_foreground_changed(1_000 * US);
    assert!(!controller.is_quiet());
    let lifted = raised(&decisions, &controller);
    assert_eq!(lifted.len(), 3, "every cluster takes part in the warm start");
    assert_eq!(
        controller.ceiling_khz(2),
        960_000,
        "first stage is a third of the target, not the top"
    );

    controller.on_deadline(1_020 * US);
    assert!(controller.ceiling_khz(2) > 960_000);
    controller.on_deadline(1_040 * US);
    assert_eq!(
        controller.ceiling_khz(2),
        1_766_400,
        "warm start reaches the 0.45 target within 60 ms"
    );
}

#[test]
fn screen_off_floors_immediately_and_has_no_deadline() {
    let mut controller = three();
    controller.on_screen(0, true);
    controller.on_demand(demand(200, 1.0, 1.0));
    assert!(!controller.is_quiet());

    let decisions = controller.on_screen(300 * US, false);
    assert!(controller.is_quiet());
    for (index, decision) in decisions.iter().enumerate() {
        assert_eq!(
            decision.ceiling_khz,
            controller.profile().clusters[index].min_khz
        );
        assert_eq!(decision.reason, DecisionReason::ScreenOff);
    }
    assert!(controller.next_deadline(400 * US).is_none());
}

#[test]
fn two_and_four_cluster_profiles_work() {
    for toml_text in [PROFILE_TWO, PROFILE_FOUR] {
        let mut controller = Controller::new(profile(toml_text), Config::default());
        let count = controller.cluster_count();
        controller.on_screen(0, true);
        let busy = vec![0.8_f32; count];
        let foreground = vec![0.8_f32; count];
        controller.on_demand(DemandSample {
            ts_us: 200 * US,
            cluster_busy: busy,
            foreground_busy: foreground,
            foreground_active: true,
        });
        for index in 0..count {
            assert!(controller.ceiling_khz(index) > controller.profile().clusters[index].min_khz);
        }

        controller.on_demand(DemandSample {
            ts_us: 400 * US,
            cluster_busy: vec![0.5; count],
            foreground_busy: vec![0.0; count],
            foreground_active: false,
        });
        controller.on_deadline(500 * US);
        assert!(
            controller.is_quiet(),
            "quiet path works for {count} clusters"
        );
        settle(&mut controller, 500 * US);
        for index in 0..count {
            assert_eq!(
                controller.ceiling_khz(index),
                controller.profile().clusters[index].min_khz
            );
        }
    }
}
