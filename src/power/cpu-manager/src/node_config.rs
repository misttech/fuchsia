// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde_derive::Deserialize;
use std::collections::HashMap;

/// Typed representation of a node entry in a CPU Manager node configuration JSON5 file.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum NodeConfig {
    ThermalWatcher {
        name: String,
        dependencies: ThermalWatcherDependencies,
    },
    PowercapWatcher {
        name: String,
        dependencies: PowercapWatcherDependencies,
    },
    TrippointWatcher {
        name: String,
        config: TrippointWatcherConfig,
        dependencies: TrippointWatcherDependencies,
    },
    CpuControlHandler {
        name: String,
        config: CpuControlHandlerConfig,
        dependencies: CpuControlHandlerDependencies,
    },
    CpuDeviceHandler {
        name: String,
        config: CpuDeviceHandlerConfig,
    },
    CpuManagerMain {
        name: String,
        config: CpuManagerMainConfig,
        dependencies: CpuManagerMainDependencies,
    },
    CpuStatsHandler {
        name: String,
        #[serde(default)]
        config: Option<CpuStatsHandlerConfig>,
    },
    CpuStatsRecorder {
        name: String,
        config: CpuStatsRecorderConfig,
    },
    DomainController {
        name: String,
        dependencies: DomainControllerDependencies,
    },
    SyscallHandler {
        name: String,
        #[serde(default)]
        config: Option<SyscallHandlerConfig>,
    },
}

impl NodeConfig {
    pub fn name(&self) -> &str {
        match self {
            NodeConfig::ThermalWatcher { name, .. }
            | NodeConfig::PowercapWatcher { name, .. }
            | NodeConfig::TrippointWatcher { name, .. }
            | NodeConfig::CpuControlHandler { name, .. }
            | NodeConfig::CpuDeviceHandler { name, .. }
            | NodeConfig::CpuManagerMain { name, .. }
            | NodeConfig::CpuStatsHandler { name, .. }
            | NodeConfig::CpuStatsRecorder { name, .. }
            | NodeConfig::DomainController { name, .. }
            | NodeConfig::SyscallHandler { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalWatcherDependencies {
    pub thermal_handler_node: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowercapWatcherDependencies {
    pub cpu_manager_main_node: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrippointWatcherConfig {
    pub index_to_opp_map: HashMap<String, u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrippointWatcherDependencies {
    pub cpu_handler_node: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuControlHandlerConfig {
    pub sustainable_power: f64,
    pub power_gain: f64,
    pub total_domain_count: u8,
    pub perf_rank: u8,
    pub capacitance: f64,
    pub min_cpu_clock_speed: f64,
    pub logical_cpu_numbers: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuControlHandlerDependencies {
    pub cpu_stats_handler_node: String,
    pub cpu_dev_handler_node: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuDeviceHandlerConfig {
    pub total_domain_count: u8,
    pub perf_rank: u8,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub name: String,
    pub cluster_index: usize,
    pub handler: String,
    pub logical_cpu_numbers: Vec<u32>,
    pub normperfs_per_ghz: f64,
    #[serde(default)]
    pub boost_opp: Option<usize>,
    #[serde(default)]
    pub boot_max_opp: Option<u64>,
    #[serde(default)]
    pub boot_min_opp: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalStateConfig {
    pub cluster_opps: Vec<usize>,
    pub min_performance_normperfs: f64,
    pub static_power_w: f64,
    pub dynamic_power_per_normperf_w: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuManagerMainConfig {
    pub sustainable_power: f64,
    pub power_gain: f64,
    pub clusters: Vec<ClusterConfig>,
    pub thermal_states: Vec<ThermalStateConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuManagerMainDependencies {
    pub cpu_device_handlers: Vec<String>,
    pub cpu_stats_handler: String,
    pub syscall_handler: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuStatsHandlerConfig {
    #[serde(default)]
    pub cpu_load_cache_duration_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuStatsRecorderConfig {
    pub poll_interval_s: f64,
    pub num_history_entries: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainControllerDependencies {
    pub cpu_manager_main_node: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyscallHandlerConfig {
    #[serde(default)]
    pub use_fake_cpu_resource: Option<bool>,
}
