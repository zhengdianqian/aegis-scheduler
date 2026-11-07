// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 默默的阿文
// Aegis is an independent implementation. No third-party module source is included.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{CoreError, SCHEMA_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Efficiency,
    Balanced,
    Prime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapacitySource {
    Sysfs,
    Estimated,
}

impl Default for CapacitySource {
    fn default() -> Self {
        Self::Sysfs
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterProfile {
    pub cpus: Vec<u32>,
    pub role: Role,
    pub min_khz: u32,
    pub max_khz: u32,
    pub capacity: u32,
    pub freq_khz: Vec<u32>,
    #[serde(default)]
    pub capacity_source: CapacitySource,
}

impl ClusterProfile {
    pub fn span_khz(&self) -> u32 {
        self.max_khz.saturating_sub(self.min_khz)
    }

    pub fn fraction_of(&self, khz: u32) -> f32 {
        if self.span_khz() == 0 {
            return 0.0;
        }
        ((khz.saturating_sub(self.min_khz)) as f32 / self.span_khz() as f32).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    pub schema: u32,
    pub platform: String,
    pub sdk: u32,
    #[serde(rename = "cluster")]
    pub clusters: Vec<ClusterProfile>,
}

impl DeviceProfile {
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(path)?;
        let profile: Self = toml::from_str(&text)?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        let mut bad = Vec::new();

        if self.schema != SCHEMA_VERSION {
            bad.push(format!(
                "schema must be {SCHEMA_VERSION}, got {}",
                self.schema
            ));
        }
        if self.platform.trim().is_empty() {
            bad.push("platform must not be empty".to_owned());
        }
        if self.clusters.is_empty() {
            bad.push("at least one cluster is required".to_owned());
        }

        let mut seen: HashSet<u32> = HashSet::new();
        let mut seen_roles_prime = 0usize;
        for (index, cluster) in self.clusters.iter().enumerate() {
            if cluster.cpus.is_empty() {
                bad.push(format!("cluster[{index}].cpus must not be empty"));
            }
            for cpu in &cluster.cpus {
                if !seen.insert(*cpu) {
                    bad.push(format!("cpu {cpu} appears in more than one cluster"));
                }
            }
            if cluster.min_khz == 0 || cluster.max_khz == 0 {
                bad.push(format!("cluster[{index}] min/max must be non-zero"));
            }
            if cluster.min_khz > cluster.max_khz {
                bad.push(format!("cluster[{index}] min_khz > max_khz"));
            }
            if cluster.capacity == 0 {
                bad.push(format!("cluster[{index}].capacity must be > 0"));
            }
            if cluster.freq_khz.is_empty() {
                bad.push(format!("cluster[{index}].freq_khz must not be empty"));
            } else {
                if !cluster.freq_khz.windows(2).all(|w| w[0] < w[1]) {
                    bad.push(format!(
                        "cluster[{index}].freq_khz must be strictly ascending"
                    ));
                }
                if cluster.freq_khz.first() != Some(&cluster.min_khz) {
                    bad.push(format!("cluster[{index}].freq_khz must start at min_khz"));
                }
                if cluster.freq_khz.last() != Some(&cluster.max_khz) {
                    bad.push(format!("cluster[{index}].freq_khz must end at max_khz"));
                }
            }
            if cluster.role == Role::Prime {
                seen_roles_prime += 1;
            }
        }
        if seen_roles_prime > 1 {
            bad.push("at most one prime cluster is allowed".to_owned());
        }

        if bad.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Profile(bad.join("; ")))
        }
    }

    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// Snap a target fraction of the cluster range down to an available frequency.
    pub fn snap_khz(&self, cluster_index: usize, fraction: f32) -> u32 {
        let cluster = &self.clusters[cluster_index];
        if fraction <= 0.0 || cluster.span_khz() == 0 {
            return cluster.min_khz;
        }
        let span = cluster.span_khz() as f64;
        let target = cluster.min_khz as f64 + span * fraction.clamp(0.0, 1.0) as f64;
        let target = target.min(cluster.max_khz as f64) as u32;
        let mut chosen = cluster.min_khz;
        for &khz in &cluster.freq_khz {
            if khz <= target {
                chosen = khz;
            } else {
                break;
            }
        }
        chosen
    }

    pub fn compare_live(&self, live: &LiveTopology) -> Result<(), String> {
        if live.clusters.len() != self.clusters.len() {
            return Err(format!(
                "cluster count mismatch: profile={} live={}",
                self.clusters.len(),
                live.clusters.len()
            ));
        }
        for (index, (p, l)) in self.clusters.iter().zip(live.clusters.iter()).enumerate() {
            let mut pc = p.cpus.clone();
            let mut lc = l.cpus.clone();
            pc.sort_unstable();
            lc.sort_unstable();
            if pc != lc {
                return Err(format!("cluster[{index}] cpu membership mismatch"));
            }
            if p.min_khz != l.min_khz || p.max_khz != l.max_khz {
                return Err(format!(
                    "cluster[{index}] frequency limits mismatch: profile={}..{} live={}..{}",
                    p.min_khz, p.max_khz, l.min_khz, l.max_khz
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCluster {
    pub cpus: Vec<u32>,
    pub min_khz: u32,
    pub max_khz: u32,
    pub capacity: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTopology {
    pub clusters: Vec<LiveCluster>,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const THREE_CLUSTER: &str = r#"
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
capacity_source = "sysfs"

[[cluster]]
cpus = [3, 4, 5, 6]
role = "balanced"
min_khz = 499200
max_khz = 2803200
capacity = 811
freq_khz = [499200, 595200, 672000, 787200, 883200, 979200, 1113600, 1267200, 1382400, 1516800, 1651200, 1785600, 1920000, 2112000, 2227200, 2400000, 2553600, 2688000, 2803200]
capacity_source = "sysfs"

[[cluster]]
cpus = [7]
role = "prime"
min_khz = 595200
max_khz = 3360000
capacity = 1024
freq_khz = [595200, 672000, 787200, 864000, 960000, 1056000, 1113600, 1209600, 1324800, 1420800, 1555200, 1670400, 1766400, 1881600, 1996800, 2131200, 2246400, 2361600, 2496000, 2611200, 2764800, 2880000, 2995200, 3134400, 3216000, 3360000]
capacity_source = "sysfs"
"#;

    #[test]
    fn profile_parses_three_clusters() {
        let profile: DeviceProfile = toml::from_str(THREE_CLUSTER).expect("parse");
        profile.validate().expect("valid profile");
        assert_eq!(profile.cluster_count(), 3);
        assert_eq!(profile.clusters[0].cpus, vec![0, 1, 2]);
        assert_eq!(profile.clusters[2].role, Role::Prime);
        assert_eq!(profile.snap_khz(2, 0.0), 595200);
        assert_eq!(profile.snap_khz(2, 1.0), 3360000);
        assert_eq!(profile.snap_khz(2, 0.85), 2880000);
    }

    #[test]
    fn profile_rejects_unsorted_freqs() {
        let mut profile: DeviceProfile = toml::from_str(THREE_CLUSTER).expect("parse");
        profile.clusters[0].freq_khz = vec![2016000, 307200];
        assert!(profile.validate().is_err());
    }

    #[test]
    fn profile_rejects_duplicate_cpu() {
        let mut profile: DeviceProfile = toml::from_str(THREE_CLUSTER).expect("parse");
        profile.clusters[1].cpus.push(0);
        assert!(profile.validate().is_err());
    }

    #[test]
    fn live_topology_compares() {
        let profile: DeviceProfile = toml::from_str(THREE_CLUSTER).expect("parse");
        let live = LiveTopology {
            clusters: profile
                .clusters
                .iter()
                .map(|c| LiveCluster {
                    cpus: c.cpus.clone(),
                    min_khz: c.min_khz,
                    max_khz: c.max_khz,
                    capacity: Some(c.capacity),
                })
                .collect(),
        };
        assert!(profile.compare_live(&live).is_ok());

        let mut wrong = live.clone();
        wrong.clusters[0].cpus = vec![0, 1];
        assert!(profile.compare_live(&wrong).is_err());
    }
}
