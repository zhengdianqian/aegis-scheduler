// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

//! Pure, replayable scheduling core: configuration, device profile, controller,
//! charge governor. No sysfs access, no clocks, no global state.

pub mod charge;
pub mod config;
pub mod controller;
pub mod profile;

pub use charge::{
    ChargeAction, ChargeDecision, ChargeGovernor, ChargePlan, ChargeReason, ChargeSample, PlanWindow,
};
pub use config::{Config, LogLevel, SCHEMA_VERSION};
pub use controller::{
    Controller, Deadline, DeadlineKind, Decision, DecisionReason, DemandSample, FrameSample, Scene,
};
pub use profile::{CapacitySource, ClusterProfile, DeviceProfile, LiveCluster, LiveTopology, Role};

/// Core error type shared by loaders and validators.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("config rejected: {0}")]
    Config(String),
    #[error("profile rejected: {0}")]
    Profile(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),
    #[error("toml encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),
}
