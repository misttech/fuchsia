// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common_utils::result_debug_panic::ResultDebugPanic;
use crate::error::CpuManagerError;
use crate::message::{Message, MessageResult, MessageReturn};
use crate::node::Node;
use crate::ok_or_default_err;
use crate::types::{NormPerfs, OperatingPoint, ThermalLoad, Watts};
use anyhow::{Error, anyhow, bail, format_err};
use async_trait::async_trait;
use async_utils::event::Event as AsyncEvent;
use cpu_manager_node_config::{
    ClusterConfig, CpuManagerMainConfig, CpuManagerMainDependencies, ThermalStateConfig,
};
use fidl::endpoints::{create_endpoints, create_request_stream};
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_cpu as fcpu;
use fidl_fuchsia_thermal as fthermal;
use fuchsia_async as fasync;
use fuchsia_inspect::{self as inspect, ArrayProperty as _, Property as _};
use futures::TryStreamExt as _;
use futures::lock::{Mutex, MutexGuard};
use serde_derive::Deserialize;
use serde_json as json;
use state_recorder::{EnumStateRecorder, RecorderOptions};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto as _;
use std::fmt::Debug;
use std::rc::Rc;
use strum_macros::{Display, EnumIter, EnumString, FromRepr};
use zx::sys;

/// Node: CpuManagerMain
///
/// Summary: Provides high-level management of all CPU domains in the system, coordinating both
///          driver- and kernel-level activity. Currently only administers CPU throttling, but in
///          the longer term is meant to become a standalone component that provides a FIDL
///          interface for managing DVFS.
///
/// Handles Messages:
///     - GetDomainInfos
///     - SetBoost
///     - GetMaxFrequency
///     - SetMaxFrequency
///     - LeaseMaxFrequency
///     - GetMinFrequency
///     - LeaseMinFrequency
///     - UpdateThermalLoad
///
/// Sends Messages:
///     - GetCpuLoads
///     - GetCpuOperatingPoints
///     - GetOperatingPoint
///     - SetCpuPerformanceInfo
///     - SetOperatingPoint
///     - SetMinimumOperatingPointLimit
///
/// FIDL dependencies:
///     - fuchsia.power.broker.Topology (optional)

// NOTE(https://fxbug.dev/42166756): The thermal state configuration for Sherlock is generated using
// a process described in this bug.
// TODO(https://fxbug/dev/85813): Move this comment to the sherlock node config.

fn opp_to_min_freq_level(opp_index: usize, opp_count: usize) -> fbroker::PowerLevel {
    ((opp_count - 1) - opp_index) as fbroker::PowerLevel
}

fn min_freq_level_to_opp(level: fbroker::PowerLevel, opp_count: usize) -> usize {
    (opp_count - 1) - (level as usize)
}

fn opp_to_max_freq_cap_level(opp_index: usize) -> fbroker::PowerLevel {
    opp_index as fbroker::PowerLevel
}

fn is_lease_open(token: &fbroker::LeaseToken) -> bool {
    token.wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST).is_err()
}

async fn register_power_element(
    topology: &fbroker::TopologyProxy,
    element_name: &str,
    initial_current_level: fbroker::PowerLevel,
    valid_levels: Vec<fbroker::PowerLevel>,
    runner_client: fidl::endpoints::ClientEnd<fbroker::ElementRunnerMarker>,
    initial_lease_token: Option<fbroker::LeaseToken>,
) -> Result<(fbroker::ElementControlProxy, fbroker::DependencyToken), Error> {
    let (element_control_client, element_control_server) =
        create_endpoints::<fbroker::ElementControlMarker>();
    // TODO(https://fxbug.dev/574569750): Name initial leases in Power Broker once ElementSchema
    // supports it.
    topology
        .add_element(fbroker::ElementSchema {
            element_name: Some(element_name.to_string()),
            initial_current_level: Some(initial_current_level),
            valid_levels: Some(valid_levels),
            element_control: Some(element_control_server),
            element_runner: Some(runner_client),
            initial_lease_token,
            ..Default::default()
        })
        .await?
        .map_err(|e| anyhow!("Topology.AddElement({element_name}) failed: {e:?}"))?;

    let element_control = element_control_client.into_proxy();
    let assertive_token = fbroker::DependencyToken::create();
    let registered_token = assertive_token
        .duplicate_handle(zx::Rights::SAME_RIGHTS)
        .map_err(|s| anyhow!("duplicate_handle failed: {s}"))?;
    element_control
        .register_dependency_token(registered_token)
        .await?
        .map_err(|e| anyhow!("RegisterDependencyToken({element_name}) failed: {e:?}"))?;

    Ok((element_control, assertive_token))
}

/// Initiates a Power Broker lease on `dependency_token` at `level`.
///
/// Creates a `LeaseToken` eventpair, duplicates the server end for `Topology.Lease`, and returns
/// the client `LeaseToken`, a retained server `LeaseToken` (for peer-closed monitoring), and a
/// `'static` future that resolves the `Topology.Lease` FIDL call so callers can drop internal
/// locks before awaiting it.
fn start_broker_lease(
    topology: &fbroker::TopologyProxy,
    dependency_token: fbroker::DependencyToken,
    lease_name: &str,
    level: fbroker::PowerLevel,
) -> Result<
    (
        fbroker::LeaseToken,
        fbroker::LeaseToken,
        impl futures::Future<Output = Result<(), CpuManagerError>> + 'static,
    ),
    CpuManagerError,
> {
    let (client_token, server_token) = fbroker::LeaseToken::create();
    let broker_server_token = server_token
        .duplicate_handle(zx::Rights::SAME_RIGHTS)
        .map_err(|s| CpuManagerError::GenericError(anyhow!("duplicate_handle: {s}")))?;
    let lease_fut = topology.lease(fbroker::LeaseSchema {
        lease_token: Some(broker_server_token),
        lease_name: Some(lease_name.to_string()),
        dependencies: Some(vec![fbroker::LeaseDependency {
            requires_token: Some(dependency_token),
            requires_level: Some(level),
            ..Default::default()
        }]),
        ..Default::default()
    });
    Ok((client_token, server_token, async move {
        lease_fut
            .await
            .map_err(|e| CpuManagerError::GenericError(e.into()))?
            .map_err(|e| CpuManagerError::GenericError(anyhow!("Topology.Lease failed: {e:?}")))
    }))
}

#[derive(Clone, Copy, Debug)]
struct Range<T: Clone + Copy + Debug> {
    upper: T,
    lower: T,
}

/// Describes a value that, if not known exactly, can be confined to a range.
#[derive(Clone, Copy, Debug)]
enum RangedValue<T: Clone + Copy + Debug> {
    Known(T),
    InRange(Range<T>),
}

// The `lower()` and `upper()` methods allow a Known value to be treated as a singleton range
// without adding extra complexity at the call site.
impl<T: Clone + Copy + Debug> RangedValue<T> {
    fn lower(&self) -> T {
        match self {
            &Self::Known(value) => value,
            &Self::InRange(range) => range.lower,
        }
    }

    fn upper(&self) -> T {
        match self {
            &Self::Known(value) => value,
            &Self::InRange(range) => range.upper,
        }
    }
}

/// Runtime representation of a CPU cluster.
struct CpuCluster {
    /// Name of the cluster, for logging purposes only.
    name: String,

    /// This cluster's index in CpuManagerMain's ordering of all clusters.
    cluster_index: usize,

    /// Handler that manages the corresponding CPU driver. Must respond to:
    ///  - GetOperatingPoint
    ///  - SetOperatingPoint
    ///  - GetCpuOperatingPoints
    handler: Rc<dyn Node>,

    /// Logical CPU numbers of the CPUs in this cluster.
    logical_cpu_numbers: Vec<u32>,

    /// Normalized performance of this cluster per GHz of CPU speed.
    performance_per_ghz: NormPerfs,

    /// All opps supported by this cluster. The handler guarantees that they are sorted
    /// primairly by frequency and secondarily by voltage.
    opps: Vec<OperatingPoint>,

    /// Index of this cluster's current opp. If an update to the opp fails, we will assume
    /// that it is between the previous and desired states (inclusive), so that pessimistic guesses
    /// of the opp may be used accordingly.
    // TODO(https://fxbug.dev/42165500): Look into richer specification of failure modes in the CPU
    // device protocols.
    current_opp: Cell<RangedValue<usize>>,

    /// The max frequency limit OPP that this cluster can run at.
    max_frequency_limit_opp_index: Option<u64>,

    /// Optional initial minimum OPP floor held from startup until the first boost cycle ends.
    /// On headless builds (such as `minimal`) where nothing requests a boost, this keeps the
    /// cluster pinned at a fixed floor for predictable microbenchmark performance.
    boot_min_opp: Option<usize>,

    /// The minimum OPP limit index to apply when CPU boost is enabled.
    boost_opp: usize,

    /// Retained Power Broker `ElementControl` proxies for this cluster's power elements.
    element_controls: Vec<fbroker::ElementControlProxy>,

    /// Dependency token for leasing this cluster's minimum frequency floor element (`cpu_cluster_{i}_min_freq`).
    min_freq_dep_token: Option<fbroker::DependencyToken>,

    /// Dependency token for leasing this cluster's maximum frequency ceiling element (`cpu_cluster_{i}_max_freq_cap`).
    max_freq_cap_dep_token: Option<fbroker::DependencyToken>,

    /// Active Power Broker lease for CPU boost on `cpu_cluster_{i}_min_freq`.
    boost_lease: Option<fbroker::LeaseToken>,

    /// Active legacy `SetMaxFrequency` lease token held by `cpu-manager`.
    legacy_max_freq_cap_lease: Option<fbroker::LeaseToken>,

    /// Active DomainController floor leases `(opp_index, server_token)` on this cluster.
    min_freq_leases: Vec<(u64, fbroker::LeaseToken)>,

    /// Active DomainController ceiling leases `(opp_index, server_token)` on this cluster.
    max_freq_cap_leases: Vec<(u64, fbroker::LeaseToken)>,
}

impl CpuCluster {
    /// Removes any floor or ceiling leases whose client `LeaseToken` peer has closed.
    ///
    /// Lease drops are signaled via `EVENTPAIR_PEER_CLOSED` (either when an external client closes
    /// its `LeaseToken` or when a legacy `SetMaxFrequency`/`ClearMaxFrequency` call drops
    /// `legacy_max_freq_cap_lease`). Synchronously pruning closed tokens in `GetMaxFrequency` and
    /// `GetMinFrequency` (and after dropping `legacy_max_freq_cap_lease`) guarantees set-read and
    /// drop-read consistency even before asynchronous peer-closed watcher tasks run.
    fn prune_closed_leases(&mut self) {
        self.min_freq_leases.retain(|(_, token)| is_lease_open(token));
        self.max_freq_cap_leases.retain(|(_, token)| is_lease_open(token));
    }

    /// Acquires or releases this cluster's Power Broker boost or `boot_min_opp` floor lease on
    /// `cpu_cluster_{i}_min_freq`. Returns `Ok(true)` if the element is registered with Power
    /// Broker, or `Ok(false)` if the caller should fall back to updating the driver directly.
    async fn update_boost_lease(
        &mut self,
        topology: &fbroker::TopologyProxy,
        boost_enabled: bool,
    ) -> Result<bool, CpuManagerError> {
        let Some(dep_token) = self
            .min_freq_dep_token
            .as_ref()
            .and_then(|t| t.duplicate_handle(zx::Rights::SAME_RIGHTS).ok())
        else {
            self.boost_lease = None;
            return Ok(false);
        };
        let target = match (boost_enabled, self.boot_min_opp) {
            (true, Some(boot_min)) => Some(("boost", self.boost_opp.min(boot_min))),
            (true, None) => Some(("boost", self.boost_opp)),
            (false, Some(boot_min)) => Some(("boot_min_opp", boot_min)),
            (false, None) => None,
        }
        // Operating points are ordered from highest frequency (index 0) to lowest
        // (index `self.opps.len() - 1`). Only acquire a floor lease when `min_opp`
        // raises the minimum frequency above the lowest-frequency operating point.
        .filter(|&(_, min_opp)| min_opp < self.opps.len() - 1);
        if let Some((lease_name, min_opp)) = target {
            let level = opp_to_min_freq_level(min_opp, self.opps.len());
            let (lease_token, _server_token, lease_fut) =
                start_broker_lease(topology, dep_token, lease_name, level)?;
            let _old_lease = self.boost_lease.replace(lease_token);
            lease_fut.await?;
        } else {
            self.boost_lease = None;
        }
        Ok(true)
    }

    fn current_max_freq_cap_opp(&self) -> Option<u64> {
        self.max_freq_cap_leases.iter().map(|(opp, _)| *opp).max()
    }

    fn configured_domain_min_opp(&self) -> u64 {
        let default_min_opp = self.boot_min_opp.unwrap_or(self.opps.len() - 1) as u64;
        self.min_freq_leases
            .iter()
            .map(|(opp, _)| *opp)
            .min()
            .map_or(default_min_opp, |opp| opp.min(default_min_opp))
    }

    /// Returns the effective minimum operating point limit index for this cluster, reconciling any
    /// active floor leases with `boost_opp` when boost is enabled. Operating points are sorted from
    /// highest frequency (index 0) to lowest, so the higher-performance floor is the lower OPP
    /// index.
    fn min_opp(&self, boost_enabled: bool) -> usize {
        let domain_min = self.configured_domain_min_opp() as usize;
        if boost_enabled { self.boost_opp.min(domain_min) } else { domain_min }
    }

    /// Given fractional loads for all system CPUs, gives this cluster's corresponding load and its
    /// estimated normalized performance. If the current opp is unknown, the highest-possible
    /// frequency will be used to ensure that performance (and thus contribution to thermals) is
    /// not underestimated.
    fn process_fractional_loads(&self, all_cpu_loads: &Vec<f32>) -> (f32, NormPerfs) {
        let cluster_load: f32 =
            self.logical_cpu_numbers.iter().map(|i| all_cpu_loads[*i as usize]).sum();

        // opps are sorted with frequency as primary key, so the lowest-possible index has the
        // highest-possible frequency.
        let opp_index = self.current_opp.get().lower();
        let frequency = self.opps[opp_index].frequency;

        let performance =
            self.performance_per_ghz.mul_scalar(frequency.0 / 1e9 * cluster_load as f64);
        (cluster_load, performance)
    }

    /// Gets the performance capacity of the indicated opp.
    fn get_performance_capacity(&self, opp_index: usize) -> NormPerfs {
        let opp = &self.opps[opp_index];
        let num_cores = self.logical_cpu_numbers.len() as f64;
        self.performance_per_ghz.mul_scalar(num_cores * opp.frequency.0 / 1e9)
    }

    // Updates the kernel's CPU performance info to match the provided opp.
    async fn update_kernel_performance_info(
        &self,
        syscall_handler: &Rc<dyn Node>,
        target_opp: &OperatingPoint,
    ) -> Result<(), CpuManagerError> {
        let performance_scale: sys::zx_cpu_performance_scale_t =
            self.performance_per_ghz.mul_scalar(target_opp.frequency.0 / 1e9).try_into()?;

        let performance_info = self
            .logical_cpu_numbers
            .iter()
            .map(|n| sys::zx_cpu_performance_info_t { logical_cpu_number: *n, performance_scale })
            .collect::<Vec<_>>();

        let msg = Message::SetCpuPerformanceInfo(performance_info);
        match syscall_handler.handle_message(&msg).await {
            Ok(MessageReturn::SetCpuPerformanceInfo) => Ok(()),
            Ok(other) => panic!("Unexpected SetCpuPerformanceInfo result: {:?}", other),
            Err(e) => Err(e),
        }
    }

    // Carries out a opp change for this cluster.
    async fn update_opp(
        &self,
        syscall_handler: &Rc<dyn Node>,
        mut index: usize,
    ) -> Result<(), CpuManagerError> {
        // Constrain the index if it indicates an OPP higher than the maximum. OPPs are sorted from
        // highest frequency to lowest, so a lower index indicates a higher frequency.
        if let Some(ref opp_limit_index) = self.max_frequency_limit_opp_index {
            index = index.max(*opp_limit_index as usize);
        }

        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain opp",
            self.cluster_index as u64,
            &self.name => index as u32
        );

        // If the current opp is known and equal to the new one, no update is needed.
        if let RangedValue::Known(current) = self.current_opp.get() {
            if current == index {
                return Ok(());
            }
        }

        // If the opp is unknown, the lowest-possible frequency (highest-possible opp index)
        // is what was used to inform the last `update_kernel_performance_info` call.
        let current_frequency = self.opps[self.current_opp.get().upper()].frequency;

        let target_opp = &self.opps[index];

        // If lowering frequency, we update the kernel before changing opps. Otherwise, the
        // kernel will be updated after the change.
        let kernel_updated = if target_opp.frequency < current_frequency {
            self.update_kernel_performance_info(&syscall_handler, target_opp).await?;
            true
        } else {
            false
        };

        // If the current opp is unknown or not equal to the new one, attempt an update.
        match self.handler.handle_message(&Message::SetOperatingPoint(index as u32)).await {
            Ok(MessageReturn::SetOperatingPoint) => {
                self.current_opp.set(RangedValue::Known(index));

                if !kernel_updated {
                    self.update_kernel_performance_info(&syscall_handler, target_opp).await?;
                }
                Ok(())
            }
            Ok(r) => {
                // Programming error
                panic!("Wrong response type for SetOperatingPoint: {:?}", r);
            }
            Err(e) => {
                log::error!("SetOperatingPoint failed: {:?}", e);

                // If the update failed, query the value, so at least the current state is known. If
                // that fails, too, record a range of possible values based on the previous and
                // desired values. Regardless, propagate the error from SetOperatingPoint.
                match self.handler.handle_message(&Message::GetOperatingPoint).await {
                    Ok(MessageReturn::GetOperatingPoint(i)) => {
                        self.current_opp.set(RangedValue::Known(i as usize));
                    }
                    result => {
                        log::error!("Unexpected result from GetOperatingPoint: {:?}", result);
                        let range = Range {
                            lower: std::cmp::min(self.current_opp.get().lower(), index),
                            upper: std::cmp::max(self.current_opp.get().upper(), index),
                        };
                        self.current_opp.set(RangedValue::InRange(range));
                    }
                }

                // If we already updated the kernel, make a new update using the lowest-possible
                // CPU frequency (highest-possbile opp index) to provide a pessimistic estimate
                // of CPU performance.
                if kernel_updated {
                    let opp = &self.opps[self.current_opp.get().upper()];
                    self.update_kernel_performance_info(&syscall_handler, opp).await?;
                }

                Err(e)
            }
        }
    }
}

/// Cross-cluster CPU thermal state
///
/// The power of a thermal state for a given performance in NormPerfs is modeled as
//      power = static_power + dynamic_power_per_normperf * performance.
#[derive(Clone, Debug)]
struct ThermalState {
    /// Index of the opp to be used for each CPU cluster.
    cluster_opps: Vec<usize>,

    /// Minimum performance at which this thermal state will be used. At low performance values,
    /// it is common for different thermal states to have very similar power requirements. The
    /// minimum performance is used to force a particular choice between such states.
    min_performance: NormPerfs,

    /// Static (fixed) power draw of this thermal state.
    static_power: Watts,

    /// Power draw per unit of normalized performance, assuming that load is perfectly balanced
    /// across CPUs.
    ///
    /// If there is only one cluster, and it is modeled using the standard switching power model
    ///     switching_power = capacitance * voltage**2 * operation_rate,
    /// then dynamic_power_per_normperf would be
    ///     operation_rate * performance_per_ghz * capacitance * voltage**2.
    /// The multi-cluster case is somewhat more complicated, and we furthermore don't require use
    /// of the switching power model. But the voltage term captures a typical way that this value
    /// depends on the underlying opps.
    dynamic_power_per_normperf: Watts,

    /// Maximum performance that this thermal state can provide, i.e. the performance that will be
    /// achieved when all CPUs are saturated. This value is derived directly from the opps
    /// specified by this thermal state.
    ///
    /// The term "capacity" is used in agreement with the kernel scheduler.
    performance_capacity: NormPerfs,
}

struct PerfAndPower {
    performance: NormPerfs,
    power: Watts,
}

impl ThermalState {
    /// Estimates the performance and power of this thermal state for the given model of desired
    /// performance.
    fn estimate_perf_and_power(&self, performance_model: PerformanceModel) -> PerfAndPower {
        let performance = match performance_model {
            Saturated => self.performance_capacity,
            FixedValue(p) => NormPerfs::min(self.performance_capacity, p),
        };
        let dynamic_power = self.dynamic_power_per_normperf.mul_scalar(performance.0);

        PerfAndPower { performance, power: self.static_power + dynamic_power }
    }

    /// Like `estimate_perf_and_power`, but returns only the power estimate.
    fn estimate_power(&self, performance_model: PerformanceModel) -> Watts {
        self.estimate_perf_and_power(performance_model).power
    }
}

/// Validates that a list of thermal states are valid, meeting the criteria:
///  - State 0 has min_performance of 0.0;
///  - For any input `desired_performance`, the admissible states (states for which
///    `state.min_performance <= desired_performance`) are in order of strictly decreasing modeled
///    power.
///
/// To address the power criterion, two further definitions are necessary:
///  - Power is modeled as
///
///    ```
///      power = static_power
///              + dynamic_power_per_normperf * min(desired_performance, performance_capacity).
///    ```
///
///  - Two states are *adjacent* at `desired_performance` if and only if they are both admissible at
///    `desired_performance`, and no state between them in the full list of states is admissible.
///
/// Power is strictly decreasing if the following conditions are met:
///  - dynamic_power_per_normperf is non-increasing;
///  - performance_capacity is non-increasing;
///  - When any two states are adjacent, the lower-index state draws more power at the first
///    performance value at which they are adjacent.
///
/// The first two conditions guarantee that the power delta between a lower-index state and a
/// higher-index state is non-decreasing with respect to performance. The third condition implies
/// that the power delta between any two adjacent states is initially positive; since the delta is
/// non-decreasing by the first two conditions, it will continue to be positive for all subsequent
/// performance values.
fn validate_thermal_states(states: &Vec<ThermalState>) -> Result<(), Error> {
    anyhow::ensure!(
        states[0].min_performance == NormPerfs(0.0),
        "State 0 ({:?}) must have min_performance == 0.0.",
        states[0]
    );

    for i in 0..states.len() - 1 {
        let state = &states[i];
        let next_state = &states[i + 1];

        anyhow::ensure!(
            next_state.dynamic_power_per_normperf <= state.dynamic_power_per_normperf,
            "Thermal states' dynamic_power_per_normperf must be non-increasing; violated by \
            {:?} and {:?}",
            state,
            next_state
        );
        anyhow::ensure!(
            next_state.performance_capacity <= state.performance_capacity,
            "Thermal states' performance_capacity must be non-increasing; violated by {:?} and \
            {:?}",
            state,
            next_state
        );

        // Compare `state` to each state further down the list that will be adjacent to it at some
        // performance value. Verify that the adjacent state has lower power at the first shared
        // performance.
        for adjacent_state in &states[i + 1..] {
            let first_shared_performance =
                FixedValue(NormPerfs::max(state.min_performance, adjacent_state.min_performance));
            let power = state.estimate_power(first_shared_performance);
            let adjacent_power = adjacent_state.estimate_power(first_shared_performance);

            anyhow::ensure!(
                adjacent_power < power,
                "Power is not strictly decreasing at performance {:?}; state {:?} consumes {:?}, \
                while state {:?} consumes {:?}.",
                first_shared_performance,
                state,
                power,
                adjacent_state,
                adjacent_power
            );

            if adjacent_state.min_performance <= state.min_performance {
                // `adjacent_state` is always between `state` and any state later in the list.
                break;
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct JsonData {
    config: CpuManagerMainConfig,
    dependencies: CpuManagerMainDependencies,
}

/// Builder for `CpuManagerMain`
pub struct CpuManagerMainBuilder<'a> {
    sustainable_power: Watts,
    power_gain: Watts,
    cluster_configs: Vec<ClusterConfig>,

    /// Parallel to `cluster_configs`; contains one `CpuDeviceHandler` node (or equivalent) for each
    /// CPU cluster.
    cluster_handlers: Vec<Rc<dyn Node>>,

    thermal_state_configs: Vec<ThermalStateConfig>,
    syscall_handler: Rc<dyn Node>,
    cpu_stats_handler: Rc<dyn Node>,
    topology: Option<fbroker::TopologyProxy>,
    inspect_root: Option<&'a inspect::Node>,
}

impl<'a> CpuManagerMainBuilder<'a> {
    pub fn new_from_json(json_data: json::Value, nodes: &HashMap<String, Rc<dyn Node>>) -> Self {
        let data: JsonData = json::from_value(json_data).unwrap();
        assert_eq!(
            data.config.clusters.iter().map(|c| &c.handler).collect::<Vec<_>>(),
            data.dependencies.cpu_device_handlers.iter().collect::<Vec<_>>()
        );

        let cluster_handlers =
            data.config.clusters.iter().map(|c| nodes[&c.handler].clone()).collect();
        let cpu_stats_handler = nodes[&data.dependencies.cpu_stats_handler].clone();
        let syscall_handler = nodes[&data.dependencies.syscall_handler].clone();
        let topology =
            fuchsia_component::client::connect_to_protocol::<fbroker::TopologyMarker>().ok();

        let mut builder = Self::new(
            Watts(data.config.sustainable_power),
            Watts(data.config.power_gain),
            data.config.clusters,
            cluster_handlers,
            data.config.thermal_states,
            syscall_handler,
            cpu_stats_handler,
        );
        builder.topology = topology;
        builder
    }

    fn new(
        sustainable_power: Watts,
        power_gain: Watts,
        cluster_configs: Vec<ClusterConfig>,
        cluster_handlers: Vec<Rc<dyn Node>>,
        thermal_state_configs: Vec<ThermalStateConfig>,
        syscall_handler: Rc<dyn Node>,
        cpu_stats_handler: Rc<dyn Node>,
    ) -> Self {
        Self {
            sustainable_power,
            power_gain,
            cluster_configs,
            cluster_handlers,
            thermal_state_configs,
            cpu_stats_handler,
            syscall_handler,
            topology: None,
            inspect_root: None,
        }
    }

    #[cfg(test)]
    pub fn with_inspect_root(mut self, root: &'a inspect::Node) -> Self {
        self.inspect_root = Some(root);
        self
    }

    #[cfg(test)]
    pub fn with_topology(mut self, topology: fbroker::TopologyProxy) -> Self {
        self.topology = Some(topology);
        self
    }

    pub fn build(self) -> Result<Rc<CpuManagerMain>, Error> {
        // Optionally use the default inspect root node
        let inspect_root =
            self.inspect_root.unwrap_or_else(|| inspect::component::inspector().root());

        let cluster_names = self.cluster_configs.iter().map(|c| c.name.as_str()).collect();
        let inspect = InspectData::new(inspect_root, "CpuManagerMain", cluster_names);

        let mut throttling_state_recorder = EnumStateRecorder::new(
            "cpu_throttling_state".into(),
            c"thermal",
            RecorderOptions {
                capacity: 50,
                lazy_record: true,
                manager: None,
                ..Default::default()
            },
        )?;
        throttling_state_recorder.record(ThrottlingState::Inactive);

        let mutable_inner = MutableInner {
            num_cpus: 0,
            current_thermal_state: None,
            cluster_configs: Some(self.cluster_configs),
            cluster_handlers: Some(self.cluster_handlers),
            clusters: Vec::new(),
            thermal_state_configs: Some(self.thermal_state_configs),
            thermal_states: Vec::new(),
            throttling_state_recorder,
            active_boosts: HashSet::new(),
            boot_min_opp_active: true,
        };

        Ok(Rc::new(CpuManagerMain {
            sustainable_power: self.sustainable_power,
            power_gain: self.power_gain,
            init_done: AsyncEvent::new(),
            cpu_stats_handler: self.cpu_stats_handler,
            syscall_handler: self.syscall_handler,
            topology: self.topology,
            inspect,
            mutable_inner: Rc::new(Mutex::new(mutable_inner)),
            scope: fasync::Scope::new_with_name("cpu_manager_main"),
        }))
    }

    #[cfg(test)]
    pub async fn build_and_init(self) -> Rc<CpuManagerMain> {
        let node = self.build().unwrap();
        node.init().await.unwrap();
        node
    }
}

const BOOST_OPP: usize = 0;

pub struct CpuManagerMain {
    /// The available power when thermal load is 0.
    sustainable_power: Watts,

    /// A scale factor that maps thermal load to available power.
    power_gain: Watts,

    /// Signalled after `init()` has completed. Used to ensure node doesn't process messages until
    /// its `init()` has completed.
    init_done: AsyncEvent,

    /// Must service GetNumCpus and SetCpuPerformanceInfo messages.
    syscall_handler: Rc<dyn Node>,

    /// The node that will provide CPU load information. It is expected that this node responds to
    /// the GetCpuLoads message.
    cpu_stats_handler: Rc<dyn Node>,

    /// Optional connection to Power Broker's Topology protocol for registering domain elements.
    topology: Option<fbroker::TopologyProxy>,

    inspect: InspectData,

    /// Mutable inner state. This must be guarded with a Mutex rather than a RefCell because CPU
    /// boost requests can overlap with thermal updates.
    mutable_inner: Rc<Mutex<MutableInner>>,

    /// Scope owning background ElementRunner and lease watcher tasks.
    scope: fasync::Scope,
}

/// A performance model for a future time interval.
#[derive(Copy, Clone, Debug)]
enum PerformanceModel {
    /// Models performance to be a fixed value.
    FixedValue(NormPerfs),

    /// Models the CPUs to be saturated regardless of frequency.
    Saturated,
}
use PerformanceModel::{FixedValue, Saturated};

impl PerformanceModel {
    /// Indicates whether a fractional CPU load is considered saturated.
    fn cpu_load_is_saturated(load: f32) -> bool {
        debug_assert!(load <= 1.0);
        const CPU_SATURATION_FRACTION: f32 = 0.99;
        load > CPU_SATURATION_FRACTION
    }
}

impl CpuManagerMain {
    // Returns a Vec of all CPU loads as fractional utilizations.
    async fn get_cpu_loads(&self) -> Result<Vec<f32>, Error> {
        fuchsia_trace::duration!("cpu_manager", "CpuManagerMain::get_cpu_loads");

        // Get load for all CPUs in the system
        match self.send_message(&self.cpu_stats_handler, &Message::GetCpuLoads).await {
            Ok(MessageReturn::GetCpuLoads(loads)) => Ok(loads),
            Ok(r) => Err(format_err!("GetCpuLoads had unexpected return value: {:?}", r)),
            Err(e) => Err(format_err!("GetCpuLoads failed: {:?}", e)),
        }
    }

    // Determines the thermal state that should be used for the given available power and
    // performance model, as well as its estimated performance and power.
    fn select_thermal_state(
        thermal_states: &Vec<ThermalState>,
        available_power: Watts,
        performance_model: PerformanceModel,
    ) -> (usize, PerfAndPower) {
        // State 0 is guaranteed to be admissible. If it meets the power criterion, we return it.
        // Otherwise, we use it to initialize the fallback -- the lowest-index admissible state,
        // which will be used if no states meet the power criterion.
        debug_assert_eq!(thermal_states[0].min_performance, NormPerfs(0.0));
        let mut fallback = (0, thermal_states[0].estimate_perf_and_power(performance_model));
        if fallback.1.power < available_power {
            return fallback;
        }

        for (i, thermal_state) in thermal_states.iter().enumerate().skip(1) {
            if let FixedValue(performance) = performance_model {
                if thermal_state.min_performance > performance {
                    continue;
                }
            }

            let estimate = thermal_state.estimate_perf_and_power(performance_model);
            if estimate.power < available_power {
                return (i, estimate);
            }

            fallback = (i, estimate);
        }

        fallback
    }

    // Updates the current thermal state.
    async fn update_thermal_state(&self, index: usize) -> Result<(), CpuManagerError> {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::update_thermal_state",
            "index" => index as u32
        );

        let mut inner = self.mutable_inner.lock().await;

        if let Some(current_index) = inner.current_thermal_state {
            // Return early if no update is required. We're assuming that opps have not changed.
            if current_index == index {
                return Ok(());
            }
            // A positive index indicates a throttled state. So the first condition describes
            // the transition from unthrottled to throttled, while the second describes the
            // transition from throttled to unthrottled. (There is some redundant logic in the
            // conditions to improve clarity.)
            if current_index == 0 && index > 0 {
                inner.throttling_state_recorder.record(ThrottlingState::Active);
                log::info!("Starting CPU throttling because thermal state is {index}")
            }
            if current_index > 0 && index == 0 {
                inner.throttling_state_recorder.record(ThrottlingState::Inactive);
                log::info!("Ending CPU throttling");
            }
        }

        // Update the thermal state index.
        inner.current_thermal_state = Some(index);
        match self.update_cluster_opps(&inner, index).await {
            Ok(_) => {
                self.inspect.thermal_state_index.set(&index.to_string());
                Ok(())
            }
            Err(e) => {
                inner.current_thermal_state = None;

                self.inspect.thermal_state_index.set(&format!("Unknown; {:?}", e));
                Err(e)
            }
        }
    }

    /// Handles an `UpdateThermalLoad` message.
    ///
    /// The new thermal load is checked for validity then used to calculate the available power.
    async fn handle_update_thermal_load(
        &self,
        thermal_load: ThermalLoad,
    ) -> Result<MessageReturn, CpuManagerError> {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::handle_update_thermal_load",
            "thermal_load" => thermal_load.0
        );

        if thermal_load > ThermalLoad(fthermal::MAX_THERMAL_LOAD) {
            return Err(CpuManagerError::InvalidArgument(format!(
                "Thermal load {:?} exceeds max {}",
                thermal_load,
                fthermal::MAX_THERMAL_LOAD
            )));
        }

        let available_power = self.calculate_available_power(thermal_load);
        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain available_power",
            0,
            "available_power" => available_power.0
        );
        self.set_max_power_consumption(available_power).await;

        Ok(MessageReturn::UpdateThermalLoad)
    }

    /// Calculate available power based on thermal load.
    fn calculate_available_power(&self, thermal_load: ThermalLoad) -> Watts {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::calculate_available_power",
            "thermal_load" => thermal_load.0
        );

        let power_available =
            f64::max(0.0, self.sustainable_power.0 - thermal_load.0 as f64 * self.power_gain.0);

        Watts(power_available)
    }

    /// If an error is encountered in the execution of this method, CpuManagerMain will keep itself in
    /// a usable state by using pessimistic estimates of any value that it cannot determine and then
    /// log the error.
    async fn set_max_power_consumption(&self, available_power: Watts) {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::set_max_power_consumption",
            "available_power" => available_power.0
        );

        self.init_done.wait().await;

        self.inspect.available_power.set(available_power.0);

        // Gather CPU loads over the last time interval. In the unlikely event of an error, use the
        // worst-case CPU load of 1.0 for all CPUs and throttle accordingly.
        let cpu_loads = match self.get_cpu_loads().await {
            Ok(loads) => loads,
            Err(e) => {
                log::error!(
                    "Error querying CPU loads: {}\nWill throttle assuming maximal load.",
                    e
                );
                self.inspect.last_error.set(&e.to_string());
                vec![1.0; self.mutable_inner.lock().await.num_cpus as usize]
            }
        };

        // Determine the normalized performance over the last interval.
        let mut last_performance = NormPerfs(0.0);
        for cluster in self.mutable_inner.lock().await.clusters.iter() {
            let (load, performance) = cluster.process_fractional_loads(&cpu_loads);
            last_performance += performance;

            fuchsia_trace::counter!(
                c"cpu_manager",
                c"CpuManagerMain cluster_load",
                cluster.cluster_index as u64,
                &cluster.name => load as f64
            );
            self.inspect.last_cluster_loads[cluster.cluster_index].set(load as f64);
        }
        self.inspect.last_performance.set(last_performance.0);
        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain last_performance",
            0,
            "value (NormPerfs)" => last_performance.0
        );

        let cpus_saturated = cpu_loads.iter().all(|l| PerformanceModel::cpu_load_is_saturated(*l));
        let performance_model =
            if cpus_saturated { Saturated } else { FixedValue(last_performance) };

        // Determine the next thermal state, updating if needed. We use the performance over the
        // last interval as an estimate of performance over the next interval; in principle a more
        // sophisticated estimate could be used.
        let (new_thermal_state_index, estimate) = Self::select_thermal_state(
            &self.mutable_inner.lock().await.thermal_states,
            available_power,
            performance_model,
        );
        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain new_thermal_state_index",
            0,
            "value" => new_thermal_state_index as u32
        );

        if let Err(e) = self.update_thermal_state(new_thermal_state_index).await {
            self.inspect.last_error.set(&e.to_string());
            log::error!("Error updating thermal states: {}", e);
            return;
        }

        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain projected_performance",
            0,
            "value (NormPerfs)" => estimate.performance.0
        );
        self.inspect.projected_performance.set(estimate.performance.0);

        fuchsia_trace::counter!(
            c"cpu_manager",
            c"CpuManagerMain projected_power",
            0,
            "value (W)" => estimate.power.0
        );
        self.inspect.projected_power.set(estimate.power.0);
    }

    async fn handle_set_boost(
        &self,
        enable: bool,
        id: u64,
    ) -> Result<MessageReturn, CpuManagerError> {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::handle_set_boost",
            "enable" => enable,
            "id" => id
        );

        let mut inner = self.mutable_inner.lock().await;
        let was_enabled = !inner.active_boosts.is_empty();

        if enable {
            inner.active_boosts.insert(id);
        } else {
            inner.active_boosts.remove(&id);
        }

        let should_be_enabled = !inner.active_boosts.is_empty();
        self.inspect.boost_enabled.set(should_be_enabled);

        if should_be_enabled == was_enabled {
            return Ok(MessageReturn::SetBoost);
        }

        if !should_be_enabled && std::mem::take(&mut inner.boot_min_opp_active) {
            for cluster in inner.clusters.iter_mut() {
                cluster.boot_min_opp = None;
            }
        }

        if let Some(topology) = &self.topology {
            let mut handled_by_broker = false;
            for cluster in &mut inner.clusters {
                handled_by_broker |=
                    cluster.update_boost_lease(topology, should_be_enabled).await?;
            }
            if handled_by_broker {
                return Ok(MessageReturn::SetBoost);
            }
        }

        let messages: Vec<_> = inner
            .clusters
            .iter()
            .map(|cluster| {
                let min_opp = cluster.min_opp(should_be_enabled);
                Message::SetMinimumOperatingPointLimit(min_opp as u32)
            })
            .collect();

        let futures: Vec<_> = inner
            .clusters
            .iter()
            .zip(messages.iter())
            .map(|(cluster, msg)| cluster.handler.handle_message(msg))
            .collect();

        let results = futures::future::join_all(futures).await;
        let errors: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();

        if !errors.is_empty() {
            return Err(CpuManagerError::GenericError(anyhow!(
                "Failed to set minimum operating points for boost={:?}: {:?}",
                should_be_enabled,
                errors
            )));
        }

        Ok(MessageReturn::SetBoost)
    }

    async fn handle_get_domain_infos(&self) -> Result<MessageReturn, CpuManagerError> {
        self.init_done.wait().await;

        let inner = self.mutable_inner.lock().await;

        let cpu_cluster_infos = inner
            .clusters
            .iter()
            .map(|c| fcpu::DomainInfo {
                id: Some(c.cluster_index as u64),
                core_ids: Some(c.logical_cpu_numbers.iter().map(|n| *n as u64).collect()),
                available_frequencies_hz: Some(
                    c.opps.iter().map(|o| o.frequency.0 as u64).collect(),
                ),
                name: Some(c.name.clone()),
                ..Default::default()
            })
            .collect();

        Ok(MessageReturn::GetDomainInfos(cpu_cluster_infos))
    }

    async fn handle_get_max_frequency(
        &self,
        cluster_index: u64,
    ) -> Result<MessageReturn, CpuManagerError> {
        self.init_done.wait().await;
        let mut inner = self.mutable_inner.lock().await;
        let Some(cluster) =
            inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_index as usize)
        else {
            return Err(CpuManagerError::InvalidArgument(format!(
                "cluster with index {} not found",
                cluster_index
            )));
        };
        cluster.prune_closed_leases();

        Ok(MessageReturn::GetMaxFrequency(cluster.current_max_freq_cap_opp().unwrap_or(0)))
    }

    /// Handles legacy `SetMaxFrequency` and `ClearMaxFrequency` requests (`frequency_index` is
    /// `None` for clear). Among legacy calls this is last-one-wins: each call drops the previous
    /// `legacy_max_freq_cap_lease` before acquiring the new one, while still arbitrating against
    /// any active `LeaseMaxFrequency` client tokens.
    async fn handle_set_max_frequency(
        &self,
        cluster_index: u64,
        frequency_index: Option<u64>,
    ) -> Result<MessageReturn, CpuManagerError> {
        self.set_or_lease_max_frequency(
            cluster_index,
            frequency_index,
            "domain_controller_max_freq",
            true,
        )
        .await?;
        Ok(MessageReturn::SetMaxFrequency)
    }

    async fn handle_lease_max_frequency(
        &self,
        cluster_index: u64,
        frequency_index: u64,
        lease_name: &str,
    ) -> Result<MessageReturn, CpuManagerError> {
        let token = self
            .set_or_lease_max_frequency(cluster_index, Some(frequency_index), lease_name, false)
            .await?
            .expect("lease token must be present when frequency_index is Some");
        Ok(MessageReturn::LeaseMaxFrequency(token))
    }

    async fn set_or_lease_max_frequency(
        &self,
        cluster_index: u64,
        frequency_index: Option<u64>,
        lease_name: &str,
        is_legacy_set: bool,
    ) -> Result<Option<fbroker::LeaseToken>, CpuManagerError> {
        self.init_done.wait().await;
        let mut inner = self.mutable_inner.lock().await;
        let thermal_limited_opps = inner.thermal_states
            [inner.current_thermal_state.unwrap_or_default()]
        .cluster_opps
        .clone();

        let Some(cluster) =
            inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_index as usize)
        else {
            return Err(CpuManagerError::InvalidArgument(format!(
                "cluster with index {} not found",
                cluster_index
            )));
        };

        let Some(frequency_index) = frequency_index else {
            log::info!("Removing max frequency constraint for cluster {}", cluster_index);
            if is_legacy_set {
                cluster.legacy_max_freq_cap_lease = None;
                cluster.prune_closed_leases();
            }
            cluster.max_frequency_limit_opp_index = cluster.current_max_freq_cap_opp();
            if cluster.max_freq_cap_dep_token.is_none() {
                let max_thermal_opp = thermal_limited_opps[cluster_index as usize];
                cluster.update_opp(&self.syscall_handler, max_thermal_opp).await?;
            }
            return Ok(None);
        };

        if frequency_index >= cluster.opps.len() as u64 {
            return Err(CpuManagerError::InvalidArgument(format!(
                "frequency_index {} is out of bounds for cluster {} (opp count {})",
                frequency_index,
                cluster_index,
                cluster.opps.len()
            )));
        }
        log::info!(
            "Setting max frequency constraint to OPP index {} for cluster {}",
            frequency_index,
            cluster_index
        );
        if is_legacy_set {
            cluster.legacy_max_freq_cap_lease = None;
            cluster.prune_closed_leases();
        }

        if let (Some(topology), Some(dep_token)) = (
            &self.topology,
            cluster
                .max_freq_cap_dep_token
                .as_ref()
                .and_then(|t| t.duplicate_handle(zx::Rights::SAME_RIGHTS).ok()),
        ) {
            let level = opp_to_max_freq_cap_level(frequency_index as usize);
            let (client_token, server_token, lease_fut) =
                start_broker_lease(topology, dep_token, lease_name, level)?;
            let watcher_token =
                if !is_legacy_set {
                    Some(server_token.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(|s| {
                        CpuManagerError::GenericError(anyhow!("duplicate_handle: {s}"))
                    })?)
                } else {
                    None
                };
            cluster.max_freq_cap_leases.push((frequency_index, server_token));
            let ret_token = if is_legacy_set {
                cluster.legacy_max_freq_cap_lease = Some(client_token);
                None
            } else {
                Some(client_token)
            };
            cluster.max_frequency_limit_opp_index = cluster.current_max_freq_cap_opp();
            // Drop `inner` before awaiting `Topology.Lease` so the `ElementRunner` task can
            // lock `mutable_inner` when Power Broker updates the element's level.
            drop(inner);
            if let Some(watcher_token) = watcher_token {
                self.spawn_lease_prune_watcher(cluster_index as usize, watcher_token);
            }
            lease_fut.await?;
            return Ok(ret_token);
        }

        let (client_token, server_token) = fbroker::LeaseToken::create();
        let watcher_token = server_token
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .map_err(|s| CpuManagerError::GenericError(anyhow!("duplicate_handle: {s}")))?;
        cluster.max_freq_cap_leases.push((frequency_index, server_token));
        let ret_token = if is_legacy_set {
            cluster.legacy_max_freq_cap_lease = Some(client_token);
            None
        } else {
            Some(client_token)
        };
        cluster.max_frequency_limit_opp_index = cluster.current_max_freq_cap_opp();
        let max_thermal_opp = thermal_limited_opps[cluster_index as usize];
        cluster.update_opp(&self.syscall_handler, max_thermal_opp).await?;
        drop(inner);

        if !is_legacy_set {
            self.spawn_fallback_max_lease_watcher(cluster_index as usize, watcher_token);
        }
        Ok(ret_token)
    }

    fn spawn_lease_prune_watcher(&self, cluster_idx: usize, watcher_token: fbroker::LeaseToken) {
        let mutable_inner = Rc::downgrade(&self.mutable_inner);
        self.scope.spawn_local(async move {
            let _ =
                fasync::OnSignals::new(&watcher_token, zx::Signals::EVENTPAIR_PEER_CLOSED).await;
            if let Some(mutable_inner) = mutable_inner.upgrade() {
                let mut inner = mutable_inner.lock().await;
                if let Some(cluster) =
                    inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_idx)
                {
                    cluster.prune_closed_leases();
                }
            }
        });
    }

    fn spawn_fallback_max_lease_watcher(
        &self,
        cluster_idx: usize,
        watcher_token: fbroker::LeaseToken,
    ) {
        let mutable_inner = Rc::downgrade(&self.mutable_inner);
        let syscall_handler = Rc::downgrade(&self.syscall_handler);
        self.scope.spawn_local(async move {
            let _ =
                fasync::OnSignals::new(&watcher_token, zx::Signals::EVENTPAIR_PEER_CLOSED).await;
            if let (Some(mutable_inner), Some(syscall_handler)) =
                (mutable_inner.upgrade(), syscall_handler.upgrade())
            {
                let mut inner = mutable_inner.lock().await;
                let max_thermal_opp = inner.thermal_states
                    [inner.current_thermal_state.unwrap_or_default()]
                .cluster_opps[cluster_idx];
                if let Some(cluster) =
                    inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_idx)
                {
                    cluster.prune_closed_leases();
                    cluster.max_frequency_limit_opp_index = cluster.current_max_freq_cap_opp();
                    let _ = cluster.update_opp(&syscall_handler, max_thermal_opp).await;
                }
            }
        });
    }

    async fn handle_get_min_frequency(
        &self,
        cluster_index: u64,
    ) -> Result<MessageReturn, CpuManagerError> {
        self.init_done.wait().await;
        let mut inner = self.mutable_inner.lock().await;
        let Some(cluster) =
            inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_index as usize)
        else {
            return Err(CpuManagerError::InvalidArgument(format!(
                "cluster with index {} not found",
                cluster_index
            )));
        };
        cluster.prune_closed_leases();

        Ok(MessageReturn::GetMinFrequency(cluster.configured_domain_min_opp()))
    }

    async fn handle_lease_min_frequency(
        &self,
        cluster_index: u64,
        frequency_index: u64,
        lease_name: &str,
    ) -> Result<MessageReturn, CpuManagerError> {
        self.init_done.wait().await;
        let mut inner = self.mutable_inner.lock().await;
        let boost_enabled = !inner.active_boosts.is_empty();

        let Some(cluster) =
            inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_index as usize)
        else {
            return Err(CpuManagerError::InvalidArgument(format!(
                "cluster with index {} not found",
                cluster_index
            )));
        };

        if frequency_index >= cluster.opps.len() as u64 {
            return Err(CpuManagerError::InvalidArgument(format!(
                "frequency_index {} is out of bounds for cluster {} (opp count {})",
                frequency_index,
                cluster_index,
                cluster.opps.len()
            )));
        }
        log::info!(
            "Leasing min frequency constraint at OPP index {} ({}) for cluster {}",
            frequency_index,
            lease_name,
            cluster_index
        );

        if let (Some(topology), Some(dep_token)) = (
            &self.topology,
            cluster
                .min_freq_dep_token
                .as_ref()
                .and_then(|t| t.duplicate_handle(zx::Rights::SAME_RIGHTS).ok()),
        ) {
            let level = opp_to_min_freq_level(frequency_index as usize, cluster.opps.len());
            let (client_token, server_token, lease_fut) =
                start_broker_lease(topology, dep_token, lease_name, level)?;
            let watcher_token = server_token
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .map_err(|s| CpuManagerError::GenericError(anyhow!("duplicate_handle: {s}")))?;
            cluster.min_freq_leases.push((frequency_index, server_token));
            drop(inner);
            self.spawn_lease_prune_watcher(cluster_index as usize, watcher_token);
            lease_fut.await?;
            return Ok(MessageReturn::LeaseMinFrequency(client_token));
        }

        let (client_token, server_token) = fbroker::LeaseToken::create();
        let watcher_token = server_token
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .map_err(|s| CpuManagerError::GenericError(anyhow!("duplicate_handle: {s}")))?;
        cluster.min_freq_leases.push((frequency_index, server_token));
        let min_opp = cluster.min_opp(boost_enabled);
        match cluster
            .handler
            .handle_message(&Message::SetMinimumOperatingPointLimit(min_opp as u32))
            .await
        {
            Ok(MessageReturn::SetMinimumOperatingPointLimit) => {}
            Ok(other) => panic!("Unexpected SetMinimumOperatingPointLimit result: {:?}", other),
            Err(e) => return Err(e),
        }
        drop(inner);

        self.spawn_fallback_min_lease_watcher(cluster_index as usize, watcher_token);
        Ok(MessageReturn::LeaseMinFrequency(client_token))
    }

    fn spawn_fallback_min_lease_watcher(
        &self,
        cluster_idx: usize,
        watcher_token: fbroker::LeaseToken,
    ) {
        let mutable_inner = Rc::downgrade(&self.mutable_inner);
        self.scope.spawn_local(async move {
            let _ =
                fasync::OnSignals::new(&watcher_token, zx::Signals::EVENTPAIR_PEER_CLOSED).await;
            if let Some(mutable_inner) = mutable_inner.upgrade() {
                let mut inner = mutable_inner.lock().await;
                let boost_enabled = !inner.active_boosts.is_empty();
                if let Some(cluster) =
                    inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_idx)
                {
                    cluster.prune_closed_leases();
                    let min_opp = cluster.min_opp(boost_enabled);
                    let _ = cluster
                        .handler
                        .handle_message(&Message::SetMinimumOperatingPointLimit(min_opp as u32))
                        .await;
                }
            }
        });
    }

    /// Registers per-domain floor (`cpu_cluster_{i}_min_freq`) and ceiling
    /// (`cpu_cluster_{i}_max_freq_cap`) power elements with Power Broker and spawns their
    /// `ElementRunner` tasks.
    async fn register_power_elements(&self) {
        let Some(topology) = &self.topology else {
            return;
        };
        let clusters_to_register: Vec<_> = {
            let inner = self.mutable_inner.lock().await;
            let boost_enabled = !inner.active_boosts.is_empty();
            inner
                .clusters
                .iter()
                .map(|cluster| {
                    let max_server_token = cluster
                        .max_freq_cap_leases
                        .first()
                        .and_then(|(_, t)| t.duplicate_handle(zx::Rights::SAME_RIGHTS).ok());
                    (
                        cluster.cluster_index,
                        cluster.name.clone(),
                        cluster.opps.len(),
                        Rc::downgrade(&cluster.handler),
                        cluster.min_opp(boost_enabled),
                        cluster.max_frequency_limit_opp_index.filter(|&opp| opp > 0),
                        max_server_token,
                    )
                })
                .collect()
        };

        for (
            cluster_idx,
            cluster_name,
            opp_count,
            handler,
            initial_min_opp,
            initial_max_opp,
            max_server_token,
        ) in clusters_to_register
        {
            let valid_levels: Vec<fbroker::PowerLevel> =
                (0..opp_count as fbroker::PowerLevel).collect();
            let min_name = format!("cpu_cluster_{cluster_idx}_min_freq");
            let max_name = format!("cpu_cluster_{cluster_idx}_max_freq_cap");

            // When a cluster starts with an initial floor (`boot_min_opp` or an early boost) or
            // ceiling (`boot_max_opp`), pass `initial_current_level` and `initial_lease_token` to
            // `Topology.AddElement` so Power Broker suppresses the initial `SetLevel(0)` and
            // starts the element directly at its leased boot level.
            let initial_min_level = opp_to_min_freq_level(initial_min_opp, opp_count);
            let (min_client_token, min_server_token) = if initial_min_level > 0 {
                let (client, server) = fbroker::LeaseToken::create();
                (Some(client), Some(server))
            } else {
                (None, None)
            };

            let initial_max_level =
                initial_max_opp.map_or(0, |opp| opp_to_max_freq_cap_level(opp as usize));

            let (min_runner_client, min_runner_stream) =
                create_request_stream::<fbroker::ElementRunnerMarker>();
            let (max_runner_client, max_runner_stream) =
                create_request_stream::<fbroker::ElementRunnerMarker>();
            self.spawn_min_freq_runner(cluster_idx, opp_count, handler, min_runner_stream);
            self.spawn_max_freq_cap_runner(cluster_idx, max_runner_stream);

            let min_res = register_power_element(
                topology,
                &min_name,
                initial_min_level,
                valid_levels.clone(),
                min_runner_client,
                min_server_token,
            )
            .await;
            let max_res = register_power_element(
                topology,
                &max_name,
                initial_max_level,
                valid_levels,
                max_runner_client,
                max_server_token,
            )
            .await;

            match (min_res, max_res) {
                (Ok((min_control, min_token)), Ok((max_control, max_token))) => {
                    let mut inner = self.mutable_inner.lock().await;
                    if let Some(cluster) =
                        inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_idx)
                    {
                        cluster.element_controls = vec![min_control, max_control];
                        cluster.min_freq_dep_token = Some(min_token);
                        cluster.max_freq_cap_dep_token = Some(max_token);
                        cluster.boost_lease = min_client_token;
                    }
                }
                (min_err, max_err) => {
                    log::info!(
                        "Power Broker unavailable for cluster '{}' (min={:?}, max={:?}); using local fallback",
                        cluster_name,
                        min_err.err(),
                        max_err.err()
                    );
                }
            }
        }
    }

    fn spawn_min_freq_runner(
        &self,
        cluster_idx: usize,
        opp_count: usize,
        handler: std::rc::Weak<dyn Node>,
        mut min_runner_stream: fbroker::ElementRunnerRequestStream,
    ) {
        self.scope.spawn_local(async move {
            while let Ok(Some(fbroker::ElementRunnerRequest::SetLevel { level, responder })) =
                min_runner_stream.try_next().await
            {
                if let Some(handler) = handler.upgrade() {
                    let opp_index = min_freq_level_to_opp(level, opp_count);
                    match handler
                        .handle_message(&Message::SetMinimumOperatingPointLimit(opp_index as u32))
                        .await
                    {
                        Ok(MessageReturn::SetMinimumOperatingPointLimit)
                        | Err(CpuManagerError::Unsupported) => {}
                        other => {
                            log::warn!(
                                "SetMinimumOperatingPointLimit failed for cluster {}: {:?}",
                                cluster_idx,
                                other
                            );
                        }
                    }
                }
                let _ = responder.send();
            }
        });
    }

    fn spawn_max_freq_cap_runner(
        &self,
        cluster_idx: usize,
        mut max_runner_stream: fbroker::ElementRunnerRequestStream,
    ) {
        let mutable_inner = Rc::downgrade(&self.mutable_inner);
        let syscall_handler = Rc::downgrade(&self.syscall_handler);
        self.scope.spawn_local(async move {
            while let Ok(Some(fbroker::ElementRunnerRequest::SetLevel { level, responder })) =
                max_runner_stream.try_next().await
            {
                if let (Some(mutable_inner), Some(syscall_handler)) =
                    (mutable_inner.upgrade(), syscall_handler.upgrade())
                {
                    let mut inner = mutable_inner.lock().await;
                    let max_thermal_opp = inner.thermal_states
                        [inner.current_thermal_state.unwrap_or_default()]
                    .cluster_opps[cluster_idx];
                    if let Some(cluster) =
                        inner.clusters.iter_mut().find(|c| c.cluster_index == cluster_idx)
                    {
                        cluster.max_frequency_limit_opp_index =
                            if level == 0 { None } else { Some(level as u64) };
                        if let Err(e) = cluster.update_opp(&syscall_handler, max_thermal_opp).await
                        {
                            log::error!(
                                "Failed to update max freq cap on cluster {}: {:?}",
                                cluster_idx,
                                e
                            );
                        }
                    }
                }
                let _ = responder.send();
            }
        });
    }

    async fn update_cluster_opps(
        &self,
        inner: &MutexGuard<'_, MutableInner>,
        thermal_state: usize,
    ) -> Result<(), CpuManagerError> {
        fuchsia_trace::duration!(
            c"cpu_manager",
            c"CpuManagerMain::update_cluster_opps",
            "thermal_state" => thermal_state as u32,
        );

        let opp_indices = inner.thermal_states[thermal_state].cluster_opps.clone();

        let cluster_update_futures: Vec<_> = inner
            .clusters
            .iter()
            .map(|cluster| {
                cluster.update_opp(&self.syscall_handler, opp_indices[cluster.cluster_index])
            })
            .collect();

        // Aggregate any errors that may have occurred when setting opps.
        let errors: Vec<_> = futures::future::join_all(cluster_update_futures)
            .await
            .into_iter()
            .filter_map(|r| r.err())
            .collect();

        if !errors.is_empty() {
            return Err(CpuManagerError::GenericError(anyhow!(
                "Failed to update opp(s) ({:?}): {:?}",
                opp_indices,
                errors
            )));
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Display, EnumIter, EnumString, Eq, PartialEq, Hash, FromRepr)]
#[repr(u8)]
enum ThrottlingState {
    Inactive = 0,
    Active = 1,
}

impl From<ThrottlingState> for u64 {
    fn from(value: ThrottlingState) -> Self {
        value as Self
    }
}

struct MutableInner {
    /// Number of CPUs in the system; confirmed to be greater than the max logical CPU number of any
    /// cluster. Populated by querying the `syscall_handler` node.
    num_cpus: u32,

    /// Cluster configs as supplied in the node config. Wrapped in an Option because they are
    /// consumed to generate `clusters` in `init()`.
    cluster_configs: Option<Vec<ClusterConfig>>,

    /// Parallel to `cluster_configs`; contains one `CpuDeviceHandler` node (or equivalent) for each
    /// CPU cluster.
    cluster_handlers: Option<Vec<Rc<dyn Node>>>,

    /// All CPU clusters governed by the `CpuManagerMain`.
    clusters: Vec<CpuCluster>,

    /// Thermal state configs as supplied in the node config. Wrapped in an Option because they are
    /// consumed to generate `thermal_states` in `init()`.
    thermal_state_configs: Option<Vec<ThermalStateConfig>>,

    /// All supported thermal states for the CPU subsystem.
    thermal_states: Vec<ThermalState>,

    /// The current thermal state of the CPU subsystem. The CPU will be put into its highest-power
    /// state during `init()`.
    current_thermal_state: Option<usize>,

    /// Records throttling state to Inspect and trace
    throttling_state_recorder: state_recorder::EnumStateRecorder<ThrottlingState>,

    /// Set of active boost sources.
    active_boosts: HashSet<u64>,

    /// Whether the `boot_min_opp` cluster floors remain active. Cleared when boost transitions to
    /// disabled, including before `init()` populates `clusters`.
    boot_min_opp_active: bool,
}

#[async_trait(?Send)]
impl Node for CpuManagerMain {
    fn name(&self) -> String {
        "CpuManagerMain".to_string()
    }

    /// Initializes internal state.
    async fn init(&self) -> Result<(), Error> {
        fuchsia_trace::duration!("cpu_manager", "CpuManagerMain::init");

        let cluster_configs =
            ok_or_default_err!(self.mutable_inner.lock().await.cluster_configs.take())
                .or_debug_panic()?;

        let cluster_handlers =
            ok_or_default_err!(self.mutable_inner.lock().await.cluster_handlers.take())
                .or_debug_panic()?;

        let thermal_state_configs =
            ok_or_default_err!(self.mutable_inner.lock().await.thermal_state_configs.take())
                .or_debug_panic()?;

        // Retrieve the total number of CPUs, and ensure that clusters' logical CPU numbers exactly
        // span 0..num_cpus.
        let num_cpus = match self.syscall_handler.handle_message(&Message::GetNumCpus).await {
            Ok(MessageReturn::GetNumCpus(n)) => n,
            other => bail!("Unexpected GetNumCpus response: {:?}", other),
        };
        let mut covered_cpu_numbers = cluster_configs
            .iter()
            .map(|c| c.logical_cpu_numbers.iter())
            .flatten()
            .map(|v| *v)
            .collect::<Vec<_>>();
        covered_cpu_numbers.sort();
        anyhow::ensure!(
            covered_cpu_numbers == (0..num_cpus).collect::<Vec<_>>(),
            "Clusters' logical CPU numbers must exactly span 0..{}",
            num_cpus
        );

        let mut clusters = Vec::new();
        for (cluster_config, handler) in
            cluster_configs.into_iter().zip(cluster_handlers.into_iter())
        {
            let opps = match handler.handle_message(&Message::GetCpuOperatingPoints).await {
                Ok(MessageReturn::GetCpuOperatingPoints(opps)) => opps,
                Ok(r) => {
                    bail!("GetCpuOperatingPoints returned unexpected value: {:?}", r)
                }
                Err(e) => bail!("Error fetching operating points: {}", e),
            };

            let boost_opp = cluster_config.boost_opp.unwrap_or(BOOST_OPP);
            anyhow::ensure!(
                boost_opp < opps.len(),
                "Cluster '{}' boost_opp ({}) must be less than opp count ({})",
                cluster_config.name,
                boost_opp,
                opps.len()
            );

            if let Some(max_opp) = cluster_config.boot_max_opp {
                anyhow::ensure!(
                    (max_opp as usize) < opps.len(),
                    "Cluster {} boot_max_opp ({}) must be less than number of OPPs ({})",
                    cluster_config.name,
                    max_opp,
                    opps.len()
                );
                let state_0_opp = *thermal_state_configs
                    .first()
                    .and_then(|state| state.cluster_opps.get(cluster_config.cluster_index))
                    .ok_or_else(|| {
                        format_err!(
                            "Missing thermal state 0 OPP for cluster {} (index {})",
                            cluster_config.name,
                            cluster_config.cluster_index
                        )
                    })?;
                anyhow::ensure!(
                    (max_opp as usize) >= state_0_opp,
                    "Cluster {} boot_max_opp ({}) must be at least as restrictive as thermal \
                    state 0 OPP ({})",
                    cluster_config.name,
                    max_opp,
                    state_0_opp
                );
            }

            if let Some(min_opp) = cluster_config.boot_min_opp {
                anyhow::ensure!(
                    min_opp < opps.len(),
                    "Cluster {} boot_min_opp ({}) must be less than number of OPPs ({})",
                    cluster_config.name,
                    min_opp,
                    opps.len()
                );
                let state_0_opp = *thermal_state_configs
                    .first()
                    .and_then(|state| state.cluster_opps.get(cluster_config.cluster_index))
                    .ok_or_else(|| {
                        format_err!(
                            "Missing thermal state 0 OPP for cluster {} (index {})",
                            cluster_config.name,
                            cluster_config.cluster_index
                        )
                    })?;
                anyhow::ensure!(
                    min_opp >= state_0_opp,
                    "Cluster {} boot_min_opp ({}) must not be higher frequency than thermal \
                    state 0 OPP ({})",
                    cluster_config.name,
                    min_opp,
                    state_0_opp
                );
            }

            // The current opp will be set when CpuManagerMain's thermal state is initialized below,
            // so initialize it to a range of all possible values for now.
            let opp_range = Range { lower: 0, upper: opps.len() - 1 };
            let current_opp = Cell::new(RangedValue::InRange(opp_range));

            let boot_max_opp = cluster_config.boot_max_opp.filter(|&opp| opp > 0);
            let (legacy_max_freq_cap_lease, max_freq_cap_leases) =
                if let Some(max_opp) = boot_max_opp {
                    let (client_token, server_token) = fbroker::LeaseToken::create();
                    (Some(client_token), vec![(max_opp, server_token)])
                } else {
                    (None, Vec::new())
                };

            clusters.push(CpuCluster {
                name: cluster_config.name,
                cluster_index: cluster_config.cluster_index,
                handler,
                logical_cpu_numbers: cluster_config.logical_cpu_numbers,
                performance_per_ghz: NormPerfs(cluster_config.normperfs_per_ghz),
                opps,
                current_opp,
                max_frequency_limit_opp_index: boot_max_opp,
                boot_min_opp: cluster_config.boot_min_opp,
                boost_opp,
                element_controls: Vec::new(),
                min_freq_dep_token: None,
                max_freq_cap_dep_token: None,
                boost_lease: None,
                legacy_max_freq_cap_lease,
                min_freq_leases: Vec::new(),
                max_freq_cap_leases,
            });
        }

        let get_performance_capacity = |thermal_state_config: &ThermalStateConfig| {
            clusters
                .iter()
                .map(|cluster| {
                    let opp_index = thermal_state_config.cluster_opps[cluster.cluster_index];
                    cluster.get_performance_capacity(opp_index)
                })
                .sum()
        };

        let thermal_states = thermal_state_configs
            .into_iter()
            .map(|t| {
                let performance_capacity = get_performance_capacity(&t);
                ThermalState {
                    cluster_opps: t.cluster_opps,
                    min_performance: NormPerfs(t.min_performance_normperfs),
                    static_power: Watts(t.static_power_w),
                    dynamic_power_per_normperf: Watts(t.dynamic_power_per_normperf_w),
                    performance_capacity,
                }
            })
            .collect();

        validate_thermal_states(&thermal_states)?;
        self.inspect.set_thermal_states(&thermal_states);

        {
            let mut inner = self.mutable_inner.lock().await;
            if !inner.boot_min_opp_active {
                for cluster in clusters.iter_mut() {
                    cluster.boot_min_opp = None;
                }
            }
            inner.num_cpus = num_cpus;
            inner.clusters = clusters;
            inner.thermal_states = thermal_states;
        }

        // Update cluster opps to match the normal power operating condition.
        self.update_thermal_state(0).await?;

        self.register_power_elements().await;

        // Initialize the minimum operating point limit on each cluster to match the initial boost
        // state, releasing any early-boot minimum frequency lock once userspace takes over. This is
        // best-effort: non-RPPM drivers that do not support operating point limits return
        // Unsupported, which is ignored, and any other error is logged without failing init.
        {
            // When Power Broker is active, `AddElement`'s initial `SetLevel` on
            // `cpu_cluster_{i}_min_freq` (backed by `initial_lease_token` when a boot floor or
            // early boost is present) already initializes `SetMinimumOperatingPointLimit`. Only
            // send `SetMinimumOperatingPointLimit` directly for clusters where Power Broker is
            // unavailable.
            let inner = self.mutable_inner.lock().await;
            let boost_enabled = !inner.active_boosts.is_empty();
            let unleased_clusters: Vec<_> = inner
                .clusters
                .iter()
                .filter(|cluster| cluster.min_freq_dep_token.is_none())
                .map(|cluster| {
                    (
                        cluster.name.clone(),
                        cluster.handler.clone(),
                        Message::SetMinimumOperatingPointLimit(
                            cluster.min_opp(boost_enabled) as u32
                        ),
                    )
                })
                .collect();
            let futures: Vec<_> = unleased_clusters
                .iter()
                .map(|(_, handler, msg)| handler.handle_message(msg))
                .collect();
            let results = futures::future::join_all(futures).await;
            for ((cluster_name, _, _), result) in unleased_clusters.iter().zip(results) {
                match result {
                    Ok(MessageReturn::SetMinimumOperatingPointLimit)
                    | Err(CpuManagerError::Unsupported) => {}
                    Ok(other) => {
                        bail!("Unexpected SetMinimumOperatingPointLimit response: {:?}", other)
                    }
                    Err(e) => log::warn!(
                        "Failed to initialize minimum operating point limit for cluster '{}': {:?}",
                        cluster_name,
                        e
                    ),
                }
            }
        }

        self.init_done.signal();

        Ok(())
    }

    async fn handle_message(&self, msg: &Message) -> MessageResult {
        match msg {
            Message::GetDomainInfos => self.handle_get_domain_infos().await,
            Message::SetBoost(enable, id) => self.handle_set_boost(*enable, *id).await,
            Message::GetMaxFrequency(cluster_index) => {
                self.handle_get_max_frequency(*cluster_index).await
            }
            Message::SetMaxFrequency(cluster_index, frequency_index) => {
                self.handle_set_max_frequency(*cluster_index, *frequency_index).await
            }
            Message::GetMinFrequency(cluster_index) => {
                self.handle_get_min_frequency(*cluster_index).await
            }
            Message::LeaseMaxFrequency(cluster_index, frequency_index, lease_name) => {
                self.handle_lease_max_frequency(*cluster_index, *frequency_index, lease_name).await
            }
            Message::LeaseMinFrequency(cluster_index, frequency_index, lease_name) => {
                self.handle_lease_min_frequency(*cluster_index, *frequency_index, lease_name).await
            }
            Message::UpdateThermalLoad(thermal_load) => {
                self.handle_update_thermal_load(*thermal_load).await
            }
            _ => Err(CpuManagerError::Unsupported),
        }
    }
}

// TODO(https://fxbug.dev/42165547): Determine whether it would be useful to track histories of any
// of these signals.
struct InspectData {
    root_node: inspect::Node,

    // Properties
    thermal_state_index: inspect::StringProperty,
    last_performance: inspect::DoubleProperty,
    last_cluster_loads: Vec<inspect::DoubleProperty>,
    available_power: inspect::DoubleProperty,
    projected_performance: inspect::DoubleProperty,
    projected_power: inspect::DoubleProperty,
    last_error: inspect::StringProperty,
    boost_enabled: inspect::BoolProperty,
}

impl InspectData {
    fn new(parent: &inspect::Node, node_name: &str, cluster_names: Vec<&str>) -> Self {
        // Create a local root node and properties
        let root_node = parent.create_child(node_name);

        let state_node = root_node.create_child("state");
        let thermal_state_index = state_node.create_string("thermal_state_index", "initializing");
        let last_performance = state_node.create_double("last_performance (NormPerfs)", 0.0);

        let mut last_cluster_loads = Vec::new();
        cluster_names.into_iter().for_each(|name| {
            state_node.record_child(name, |cluster_node| {
                last_cluster_loads.push(cluster_node.create_double("last_load (#cores)", 0.0));
            })
        });

        let available_power = state_node.create_double("available_power (W)", 0.0);
        let projected_performance =
            state_node.create_double("projected_performance (NormPerfs)", 0.0);
        let projected_power = state_node.create_double("projected_power (W)", 0.0);

        let last_error = state_node.create_string("last_error", "<None>");

        let boost_enabled = state_node.create_bool("boost_enabled", false);

        root_node.record(state_node);

        Self {
            root_node,
            thermal_state_index,
            last_performance,
            last_cluster_loads,
            available_power,
            projected_performance,
            projected_power,
            last_error,
            boost_enabled,
        }
    }

    fn set_thermal_states(&self, states: &Vec<ThermalState>) {
        let states_node = self.root_node.create_child("thermal_states");

        // Iterate over `states` in reverse order so that the Inspect nodes appear in the same
        // order as the vector (`create_child` inserts nodes at the head).
        for (i, state) in states.iter().enumerate().rev() {
            let node = states_node.create_child(format!("thermal_state_{:02}", i));

            let opps = node.create_uint_array("cluster_opps", state.cluster_opps.len());
            state.cluster_opps.iter().enumerate().for_each(|(i, p)| opps.set(i, *p as u64));
            node.record(opps);

            node.record_double("min_performance (NormPerfs)", state.min_performance.0);
            node.record_double("static_power (W)", state.static_power.0);
            node.record_double(
                "dynamic_power_per_normperf (W)",
                state.dynamic_power_per_normperf.0,
            );
            node.record_double("performance_capacity (NormPerfs)", state.performance_capacity.0);

            // Pass ownership of the new node to the parent.
            states_node.record(node);
        }

        // Pass ownership of the new `states_node` to the root node
        self.root_node.record(states_node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::mock_node::{MessageMatcher, MockNode, MockNodeMaker, create_dummy_node};
    use crate::types::{Hertz, Volts};
    use crate::{msg_eq, msg_ok_return};
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use futures::TryStreamExt;
    use std::cell::RefCell;
    use test_util::assert_lt;

    // Common test configurations for big and little clusters.
    static BIG_CPU_NUMBERS: [u32; 2] = [0, 1];
    static BIG_OPPS: [OperatingPoint; 3] = [
        OperatingPoint { frequency: Hertz(2.0e9), voltage: Volts(1.0) },
        OperatingPoint { frequency: Hertz(1.9e9), voltage: Volts(0.9) },
        OperatingPoint { frequency: Hertz(1.8e9), voltage: Volts(0.8) },
    ];
    static BIG_PERFORMANCE_PER_GHZ: NormPerfs = NormPerfs(1.0);
    static BIG_CLUSTER_INDEX: usize = 0;

    static LITTLE_CPU_NUMBERS: [u32; 2] = [2, 3];
    static LITTLE_OPPS: [OperatingPoint; 3] = [
        OperatingPoint { frequency: Hertz(1.0e9), voltage: Volts(0.5) },
        OperatingPoint { frequency: Hertz(0.9e9), voltage: Volts(0.4) },
        OperatingPoint { frequency: Hertz(0.8e9), voltage: Volts(0.3) },
    ];
    static LITTLE_PERFORMANCE_PER_GHZ: NormPerfs = NormPerfs(0.5);
    static LITTLE_CLUSTER_INDEX: usize = 1;

    // Select sensible random numbers here.
    // They determine the correlation between thermal load and available power.
    static DEFAULT_SUSTAINABLE_POWER: Watts = Watts(6.22);
    static DEFAULT_POWER_GAIN: Watts = Watts(0.0622046);

    fn make_default_cluster_configs() -> Vec<ClusterConfig> {
        vec![
            ClusterConfig {
                name: "big_cluster".to_string(),
                cluster_index: BIG_CLUSTER_INDEX,
                handler: "<unused>".to_string(),
                logical_cpu_numbers: BIG_CPU_NUMBERS[..].to_vec(),
                normperfs_per_ghz: BIG_PERFORMANCE_PER_GHZ.0,
                boost_opp: None,
                boot_max_opp: None,
                boot_min_opp: None,
            },
            ClusterConfig {
                name: "little_cluster".to_string(),
                cluster_index: LITTLE_CLUSTER_INDEX,
                handler: "<unused>".to_string(),
                logical_cpu_numbers: LITTLE_CPU_NUMBERS[..].to_vec(),
                normperfs_per_ghz: LITTLE_PERFORMANCE_PER_GHZ.0,
                boost_opp: None,
                boot_max_opp: None,
                boot_min_opp: None,
            },
        ]
    }

    // Convenience struct to manage mocks of the handlers on which CpuManagerMain depends.
    struct Handlers {
        big_cluster: Rc<MockNode>,
        little_cluster: Rc<MockNode>,
        syscall: Rc<MockNode>,
        cpu_stats: Rc<MockNode>,

        // The MockMaker comes last, so it is dropped after the MockNodes.
        _mock_maker: MockNodeMaker,
    }

    impl Handlers {
        fn new() -> Self {
            Handlers::new_with_highest_opp([0, 0])
        }

        fn new_with_highest_opp(highest_opps: [u32; 2]) -> Self {
            let handlers = Handlers::new_base();

            // During initialization, CpuManagerMain configures the highest-power thermal state,
            // with both clusters at their respective opp, and initializes the minimum opp limit
            // to the lowest-frequency opp (the last index).
            handlers.expect_big_opp(highest_opps[0]);
            handlers.expect_little_opp(highest_opps[1]);
            handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
            handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);

            handlers
        }

        // Creates handlers that only expect the queries issued at the start of
        // `CpuManagerMain::init()`, before any thermal state or opp limits are applied.
        fn new_base() -> Self {
            let mut mock_maker = MockNodeMaker::new();

            // The big and little cluster handlers are initially queried for all operating points.
            let big_cluster = mock_maker.make(
                "big_cluster_handler",
                vec![(
                    msg_eq!(GetCpuOperatingPoints),
                    msg_ok_return!(GetCpuOperatingPoints(Vec::from(&BIG_OPPS[..]))),
                )],
            );
            let little_cluster = mock_maker.make(
                "little_cluster_handler",
                vec![(
                    msg_eq!(GetCpuOperatingPoints),
                    msg_ok_return!(GetCpuOperatingPoints(Vec::from(&LITTLE_OPPS[..]))),
                )],
            );

            // The syscall handler provides the number of CPUs during initialization.
            let num_cpus = BIG_CPU_NUMBERS.len() + LITTLE_CPU_NUMBERS.len();
            let syscall = mock_maker.make(
                "syscall_handler",
                vec![(msg_eq!(GetNumCpus), msg_ok_return!(GetNumCpus(num_cpus as u32)))],
            );

            let cpu_stats = mock_maker.make("cpu_stats_handler", Vec::new());

            Self { big_cluster, little_cluster, syscall, cpu_stats, _mock_maker: mock_maker }
        }

        // Creates a builder for the given configs, wired to these handlers.
        fn builder(
            &self,
            cluster_configs: Vec<ClusterConfig>,
            thermal_state_configs: Vec<ThermalStateConfig>,
        ) -> CpuManagerMainBuilder<'static> {
            CpuManagerMainBuilder::new(
                DEFAULT_SUSTAINABLE_POWER,
                DEFAULT_POWER_GAIN,
                cluster_configs,
                vec![self.big_cluster.clone(), self.little_cluster.clone()],
                thermal_state_configs,
                self.syscall.clone(),
                self.cpu_stats.clone(),
            )
        }

        // Tells the syscall handler to expect a SetCpuPerformanceInfo call for the provided
        // collection of CPUs and performance scale.
        fn expect_performance_scale(&self, logical_cpu_numbers: &[u32], float_scale: f64) {
            let scale = NormPerfs(float_scale).try_into().unwrap();
            let info = logical_cpu_numbers
                .iter()
                .map(|n| sys::zx_cpu_performance_info_t {
                    logical_cpu_number: *n,
                    performance_scale: scale,
                })
                .collect::<Vec<_>>();
            self.syscall.add_msg_response_pair((
                msg_eq!(SetCpuPerformanceInfo(info)),
                msg_ok_return!(SetCpuPerformanceInfo),
            ));
        }

        // Updates the handlers with expectations for a big cluster opp change.
        fn expect_big_opp(&self, opp_index: u32) {
            self.big_cluster.add_msg_response_pair((
                msg_eq!(SetOperatingPoint(opp_index)),
                msg_ok_return!(SetOperatingPoint),
            ));
            let frequency = &BIG_OPPS[opp_index as usize].frequency;
            let float_scale = BIG_PERFORMANCE_PER_GHZ.0 * frequency.0 / 1e9;
            self.expect_performance_scale(&BIG_CPU_NUMBERS, float_scale);
        }

        // Updates the handlers with expectations for a little cluster opp change.
        fn expect_little_opp(&self, opp_index: u32) {
            self.little_cluster.add_msg_response_pair((
                msg_eq!(SetOperatingPoint(opp_index)),
                msg_ok_return!(SetOperatingPoint),
            ));
            let frequency = &LITTLE_OPPS[opp_index as usize].frequency;
            let float_scale = LITTLE_PERFORMANCE_PER_GHZ.0 * frequency.0 / 1e9;
            self.expect_performance_scale(&LITTLE_CPU_NUMBERS, float_scale);
        }

        // Updates the big cluster handler with expectations for a minimum opp limit change.
        fn expect_big_min_opp(&self, opp_index: u32) {
            self.expect_big_min_opp_result(
                opp_index,
                Ok(MessageReturn::SetMinimumOperatingPointLimit),
            );
        }

        // Updates the little cluster handler with expectations for a minimum opp limit change.
        fn expect_little_min_opp(&self, opp_index: u32) {
            self.expect_little_min_opp_result(
                opp_index,
                Ok(MessageReturn::SetMinimumOperatingPointLimit),
            );
        }

        // Like `expect_big_min_opp`, but responds with the provided result.
        fn expect_big_min_opp_result(&self, opp_index: u32, result: MessageResult) {
            self.big_cluster
                .add_msg_response_pair((msg_eq!(SetMinimumOperatingPointLimit(opp_index)), result));
        }

        // Like `expect_little_min_opp`, but responds with the provided result.
        fn expect_little_min_opp_result(&self, opp_index: u32, result: MessageResult) {
            self.little_cluster
                .add_msg_response_pair((msg_eq!(SetMinimumOperatingPointLimit(opp_index)), result));
        }

        // Prepares the stats handler for a CPU load query.
        fn enqueue_cpu_loads(&self, loads: Vec<f32>) {
            self.cpu_stats
                .add_msg_response_pair((msg_eq!(GetCpuLoads), msg_ok_return!(GetCpuLoads(loads))));
        }
    }

    // Verify that a node is successfully constructed from JSON configuration.
    #[fuchsia::test]
    async fn test_new_from_json() {
        let handlers = Handlers::new();

        let mut nodes = HashMap::<String, Rc<dyn Node>>::new();
        nodes.insert("big_cluster_node".to_string(), handlers.big_cluster.clone());
        nodes.insert("little_cluster_node".to_string(), handlers.little_cluster.clone());
        nodes.insert("syscall_handler_node".to_string(), handlers.syscall.clone());
        nodes.insert("stats_handler_node".to_string(), handlers.cpu_stats.clone());

        let json_data = json::json!({
            "type": "CpuManagerMain",
            "name": "cpu_manager",
            "config": {
                "sustainable_power": DEFAULT_SUSTAINABLE_POWER.0,
                "power_gain": DEFAULT_POWER_GAIN.0,
                "clusters": [
                      {
                          "name": "big_cluster",
                          "cluster_index": 0,
                          "handler": "big_cluster_node",
                          "logical_cpu_numbers": [0, 1],
                          "normperfs_per_ghz": BIG_PERFORMANCE_PER_GHZ.0
                      },
                      {
                          "name": "little_cluster",
                          "cluster_index": 1,
                          "handler": "little_cluster_node",
                          "logical_cpu_numbers": [2, 3],
                          "normperfs_per_ghz": LITTLE_PERFORMANCE_PER_GHZ.0
                      }
                ],
                "thermal_states": [
                    {
                      "cluster_opps": [0, 0],
                      "min_performance_normperfs": 0.0,
                      "static_power_w": 0.9,
                      "dynamic_power_per_normperf_w": 0.6
                    }
                ]
            },
            "dependencies": {
                "cpu_device_handlers": [
                    "big_cluster_node",
                    "little_cluster_node"
                ],
                "cpu_stats_handler": "stats_handler_node",
                "syscall_handler": "syscall_handler_node"
            }
        });

        let builder = CpuManagerMainBuilder::new_from_json(json_data, &nodes);
        builder.build_and_init().await;
    }

    // Verifies that thermal states are properly validated.
    #[fuchsia::test]
    async fn test_thermal_state_validation() {
        // Since CpuManagerMainBuilder::build() exits early, use handlers that omit expectations
        // for messages that are never sent.
        let new_handlers_for_failed_validation = Handlers::new_base;

        let cluster_configs = make_default_cluster_configs();

        // Not allowed: Increasing dynamic power.
        let handlers = new_handlers_for_failed_validation();
        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![2, 2],
                min_performance_normperfs: 0.0,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 1.1,
            },
        ];
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            cluster_configs.clone(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        );
        assert!(builder.build().unwrap().init().await.is_err());

        // Not allowed: Increasing performance capacity.
        let handlers = new_handlers_for_failed_validation();
        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![2, 2],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![2, 1],
                min_performance_normperfs: 0.0,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 0.8,
            },
        ];
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            cluster_configs.clone(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs,
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        );
        assert!(builder.build().unwrap().init().await.is_err());

        // Allowed: The second state has higher static power, but its power draw is less than the
        // first state at the first performance at which both states are admissible.
        let handlers = Handlers::new();
        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![2, 2],
                min_performance_normperfs: 1.0,
                static_power_w: 2.1,
                dynamic_power_per_normperf_w: 0.8,
            },
        ];
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            cluster_configs.clone(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        );
        builder.build_and_init().await;

        // Not allowed: The second state has higher power draw at the first performance at which
        // both states are admissible.
        let handlers = new_handlers_for_failed_validation();
        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![2, 2],
                min_performance_normperfs: 1.0,
                static_power_w: 2.5,
                dynamic_power_per_normperf_w: 0.8,
            },
        ];
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            cluster_configs.clone(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        );
        assert!(builder.build().unwrap().init().await.is_err());
    }

    // Tests that CpuManagerMainBuilder requires that clusters are configured to exactly span the space
    // of all logical CPU numbers.
    #[fuchsia::test]
    async fn test_validate_all_cpus_spanned() {
        let mut mock_maker = MockNodeMaker::new();

        // The big and little cluster handlers are initially queried for all operating points.
        let big_cluster_handler = create_dummy_node();
        let little_cluster_handler = create_dummy_node();

        // Configure the syscall handler to report one more CPU than is spanned by the clusters.
        let num_cpus = BIG_CPU_NUMBERS.len() + LITTLE_CPU_NUMBERS.len() + 1;
        let syscall_handler = mock_maker.make(
            "syscall_handler",
            vec![(msg_eq!(GetNumCpus), msg_ok_return!(GetNumCpus(num_cpus as u32)))],
        );

        let cpu_stats_handler = mock_maker.make("cpu_stats_handler", Vec::new());

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![big_cluster_handler, little_cluster_handler],
            thermal_state_configs,
            syscall_handler,
            cpu_stats_handler,
        );
        assert!(builder.build().unwrap().init().await.is_err());
    }

    // Verify that CpuManagerMain responds as expected to UpdateThermalLoad messages.
    #[fuchsia::test]
    async fn test_update_thermal_load() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![0, 1],
                min_performance_normperfs: 0.2,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 0.8,
            },
            ThermalStateConfig {
                cluster_opps: vec![1, 2],
                min_performance_normperfs: 0.4,
                static_power_w: 1.0,
                dynamic_power_per_normperf_w: 0.6,
            },
        ];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs,
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // The thermal load is 52, we have max power consumption of 2.985W.
        // The current opp is 0, so with 0.1 fractional utililzation per core, we have:
        //   Big cluster: 0.2 cores load -> 0.4GHz utilized -> 0.4 NormPerfs
        //   Little cluster: 0.2 cores load -> 0.2GHz utilized -> 0.1 NormPerfs
        // At thermal state 0, the projected power use at 0.5 NormPerfs is
        //   2W static + 0.5W dynamic = 2.5W
        // This is within the 2.985W budget, so there are no opp changes.
        handlers.enqueue_cpu_loads(vec![0.1; 4]);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(52))).await;
        result.unwrap();

        // The thermal load is 52, we have max power consumption of 2.985W.
        // The current opp is 0, so with 0.25 fractional utililzation per core, we have:
        //   Big cluster: 0.5 cores load -> 1GHz utilized -> 1 NormPerfs
        //   Little cluster: 0.5 cores load -> 0.5GHz utilized -> 0.25 NormPerfs
        // Projected power usage at 1.25 NormPerfs is:
        //   Thermal state 0: 2W static + 1.25 dynamic = 3.25W => over 2.985W budget
        //   Thermal state 1: 1.5W static + 1W dynamic = 2.5W => within 2.985W budget
        // So the new thermal state is 1, for which the little cluster changes to opp 1.
        handlers.enqueue_cpu_loads(vec![0.25; 4]);
        handlers.expect_little_opp(1);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(52))).await;
        result.unwrap();

        // The thermal load is 61, we have max power consumption of 2.426W.
        // CPU load stays the same, but the power budget drops to 2.426W, below allocation for thermal
        // state 1. This pushes us to thermal state 2, with big opp 1 and little opp 2.
        handlers.enqueue_cpu_loads(vec![0.25; 4]);
        handlers.expect_big_opp(1);
        handlers.expect_little_opp(2);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(61))).await;
        result.unwrap();

        // The thermal load is 77, we have max power consumption of 1.43W.
        // The power budget is 1.43W, which is below the static power for thermal state 1. However,
        // at 0.05 fractional utilization per core, the projected performance is 0.25 Perfs, which
        // makes thermal state 2 inadmissible. Thus, we fall back to thermal state 1.
        handlers.enqueue_cpu_loads(vec![0.05; 4]);
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(1);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(77))).await;
        result.unwrap();

        // The thermal load is 84, we have max power consumption of 0.9948W.
        // At 0.01 fractional utilization per core, the projected performance is 0.05 Perfs, so now
        // thermal state 1 is inadmissible. This drives us to thermal state 0.
        handlers.enqueue_cpu_loads(vec![0.01; 4]);
        handlers.expect_little_opp(0);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(84))).await;
        result.unwrap();
    }

    // Verify that CPU saturation is handled as expected.
    #[fuchsia::test]
    async fn test_cpu_saturation() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![1, 1],
                min_performance_normperfs: 4.5,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 0.8,
            },
            ThermalStateConfig {
                cluster_opps: vec![2, 2],
                min_performance_normperfs: 0.0,
                static_power_w: 1.0,
                dynamic_power_per_normperf_w: 0.6,
            },
        ];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // Start with low power to force thermal state 2.
        handlers.enqueue_cpu_loads(vec![0.1; 4]);
        handlers.expect_big_opp(2);
        handlers.expect_little_opp(2);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(85))).await;
        result.unwrap();

        // Now saturate the CPUs. This corresponds to 4.4 NormPerfs.
        handlers.enqueue_cpu_loads(vec![1.0; 4]);

        // We choose a max power above state 1's max and below state 0's.
        let thermal_load = {
            let inner = node.mutable_inner.lock().await;
            let state = &inner.thermal_states[1];
            let state_1_max_power = state.estimate_power(Saturated);
            let max_power = state_1_max_power + Watts(0.1);
            assert_lt!(max_power, inner.thermal_states[0].estimate_power(Saturated));
            ThermalLoad(((DEFAULT_SUSTAINABLE_POWER.0 - max_power.0) / DEFAULT_POWER_GAIN.0) as u32)
        };

        // Now we confirm that:
        //  - State 1 is selected, verifying that its min performance was disregarded due to CPU
        //    saturation.
        handlers.expect_big_opp(1);
        handlers.expect_little_opp(1);
        let result = node.handle_message(&Message::UpdateThermalLoad(thermal_load)).await;
        result.unwrap();
    }

    // Verify that Inspect data is populated as expected.
    #[fuchsia::test]
    async fn test_inspect_data() {
        let handlers = Handlers::new();

        let thermal_states = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![1, 2],
                min_performance_normperfs: 0.2,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 0.8,
            },
        ];

        let inspector = inspect::Inspector::default();
        let builder = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_states,
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .with_inspect_root(inspector.root());
        let node = builder.build_and_init().await;

        // The power budget of 1W exceeds the static power of thermal state 0, so we are pushed
        // to thermal state 1.
        handlers.enqueue_cpu_loads(vec![1.0; 4]);
        handlers.expect_big_opp(1);
        handlers.expect_little_opp(2);
        let result = node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(84))).await;
        assert_matches!(result, Ok(_));

        let estimate =
            node.mutable_inner.lock().await.thermal_states[1].estimate_perf_and_power(Saturated);

        assert_data_tree!(
            inspector,
            root: {
                "CpuManagerMain": {
                    "state": {
                        "thermal_state_index": "1",
                        "last_performance (NormPerfs)": 5.0,
                        "boost_enabled": false,
                        "big_cluster": {
                            "last_load (#cores)": 2.0,
                        },
                        "little_cluster": {
                            "last_load (#cores)": 2.0,
                        },
                        "available_power (W)": 0.9948135999999996,
                        "projected_performance (NormPerfs)": estimate.performance.0,
                        "projected_power (W)": estimate.power.0,
                        "last_error": "<None>",
                    },
                    "thermal_states": {
                        "thermal_state_00": {
                            "cluster_opps": vec![0u64, 0],
                            "min_performance (NormPerfs)": 0.0,
                            "static_power (W)": 2.0,
                            "dynamic_power_per_normperf (W)": 1.0,
                            "performance_capacity (NormPerfs)": 5.0,
                        },
                        "thermal_state_01": {
                            "cluster_opps": vec![1u64, 2],
                            "min_performance (NormPerfs)": 0.2,
                            "static_power (W)": 1.5,
                            "dynamic_power_per_normperf (W)": 0.8,
                            "performance_capacity (NormPerfs)": 4.6,
                        },
                    }
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_set_boost() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 1.0,
                dynamic_power_per_normperf_w: 0.5,
            },
            ThermalStateConfig {
                cluster_opps: vec![1, 1],
                min_performance_normperfs: 0.0,
                static_power_w: 0.5,
                dynamic_power_per_normperf_w: 0.1,
            },
        ];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // Enabling boost should take effect immediately
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let mut result = node.handle_message(&Message::SetBoost(true, 0)).await;
        result.unwrap();

        // Trigger thermal throttling.
        // available_power = 6.22 - 50 * 0.0622046 = 3.11W
        // state 0 power at saturation: 1.0 + 0.5 * 5.0 = 3.5W
        // state 1 power at saturation: 0.5 + 0.1 * 4.7 = 0.97W
        // Since state 0 power (3.5W) > available_power (3.11W), it should pick state 1.
        handlers.enqueue_cpu_loads(vec![1.0; 4]);
        handlers.expect_big_opp(1);
        handlers.expect_little_opp(1);
        node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(50))).await.unwrap();

        // Disabling boost should also take effect immediately
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        result = node.handle_message(&Message::SetBoost(false, 0)).await;
        result.unwrap();
    }

    #[fuchsia::test]
    async fn test_set_and_clear_max_frequency() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs,
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        handlers.expect_big_opp(1);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, Some(1)))
            .await
            .unwrap();

        // Now throttle the little cluster.
        handlers.expect_little_opp(2);
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();

        // Now, clear the max frequency constraint on one cluster at a time.
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
    }

    #[fuchsia::test]
    async fn test_set_and_clear_max_frequency_is_affected_by_thermal_throttling() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![
            ThermalStateConfig {
                cluster_opps: vec![0, 0],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            },
            ThermalStateConfig {
                cluster_opps: vec![0, 1],
                min_performance_normperfs: 0.2,
                static_power_w: 1.5,
                dynamic_power_per_normperf_w: 0.8,
            },
            ThermalStateConfig {
                cluster_opps: vec![1, 2],
                min_performance_normperfs: 0.4,
                static_power_w: 1.0,
                dynamic_power_per_normperf_w: 0.6,
            },
        ];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // The thermal load is 61, we have max power consumption of 2.426W.
        // CPU load stays the same, but the power budget drops to 2.426W, below allocation for
        // thermal state 1. This pushes us to thermal state 2, with big opp 1 and little opp 2.
        handlers.enqueue_cpu_loads(vec![0.25; 4]);
        handlers.expect_big_opp(1);
        handlers.expect_little_opp(2);
        node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(61))).await.unwrap();

        // Now cap the max frequency for both clusters, but only the big cluster
        // should change since the little cluster is already thermal throttled
        // to the max frequency.
        handlers.expect_big_opp(2);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();

        // And reset to power up to the thermal limit.
        // Big cluster should not change since it is still thermal throttled.
        handlers.expect_big_opp(1);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();

        // Reset thermal load.
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.enqueue_cpu_loads(vec![0.0; 4]);
        node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(0))).await.unwrap();

        // Now do the opposite: cap max frequency then thermal load.
        // Cap max frequency for both clusters to opp 2.
        handlers.expect_big_opp(2);
        handlers.expect_little_opp(2);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();

        // The thermal load is 52, we have max power consumption of 2.985W.
        // The current opp is 0, so with 0.25 fractional utililzation per core, we have:
        //   Big cluster: 0.5 cores load -> 1GHz utilized -> 1 NormPerfs
        //   Little cluster: 0.5 cores load -> 0.5GHz utilized -> 0.25 NormPerfs
        // Projected power usage at 1.25 NormPerfs is:
        //   Thermal state 0: 2W static + 1.25 dynamic = 3.25W => over 2.985W budget
        //   Thermal state 1: 1.5W static + 1W dynamic = 2.5W => within 2.985W budget
        // So the new thermal state is 1, for which the little cluster changes to opp 1;
        // however, the max frequency caps the big opp at 2 and the little opp at 2.
        handlers.enqueue_cpu_loads(vec![0.25; 4]);
        node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(52))).await.unwrap();

        // Remove the limit. With the limit removed, thermal throttling is still in place.
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(1);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();

        // Reset thermal load.
        handlers.expect_little_opp(0);
        handlers.enqueue_cpu_loads(vec![0.0; 4]);
        node.handle_message(&Message::UpdateThermalLoad(ThermalLoad(0))).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_set_boost_multiple_sources() {
        let handlers = Handlers::new_with_highest_opp([1, 1]);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![1, 1],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // 1. Enable boost with ID 0. Should set min limit to 0.
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let result = node.handle_message(&Message::SetBoost(true, 0)).await;
        result.unwrap();

        // 2. Enable boost with ID 1. Should be no-op (already enabled), but still track it.
        // No new messages expected.
        let result = node.handle_message(&Message::SetBoost(true, 1)).await;
        result.unwrap();

        // 3. Disable boost with ID 0. Should be no-op (still enabled by ID 1).
        // No new messages expected.
        let result = node.handle_message(&Message::SetBoost(false, 0)).await;
        result.unwrap();

        // 4. Disable boost with ID 1. Should set min limit back to 2 (disabled).
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let result = node.handle_message(&Message::SetBoost(false, 1)).await;
        result.unwrap();
    }

    #[fuchsia::test]
    async fn test_set_boost_idempotency_same_id() {
        let handlers = Handlers::new_with_highest_opp([1, 1]);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![1, 1],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs.clone(),
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .build_and_init()
        .await;

        // 1. Enable boost with ID 0. Should set min limit to 0.
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let result = node.handle_message(&Message::SetBoost(true, 0)).await;
        result.unwrap();

        // 2. Enable boost with ID 0 again. Should be no-op.
        // No new messages expected.
        let result = node.handle_message(&Message::SetBoost(true, 0)).await;
        result.unwrap();

        // 3. Disable boost with ID 0. Should set min limit back to 2 (disabled).
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        handlers.little_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(2)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let result = node.handle_message(&Message::SetBoost(false, 0)).await;
        result.unwrap();
    }

    #[fuchsia::test]
    async fn test_set_boost_custom_boost_opp() {
        let handlers = Handlers::new_with_highest_opp([1, 1]);

        // The little cluster's boost_opp is its last valid index, which also covers the upper
        // bound of boost_opp validation.
        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boost_opp = Some(1);
        cluster_configs[1].boost_opp = Some(LITTLE_OPPS.len() - 1);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![1, 1],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build_and_init().await;

        // Enabling boost should use each cluster's configured boost_opp.
        handlers.expect_big_min_opp(1);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();

        // Disabling boost should restore each cluster's min limit to its lowest-frequency opp.
        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_boost_opp_validation() {
        // Every message before boost_opp validation succeeds, so init can only fail because
        // the last cluster's boost_opp is out of range.
        let handlers = Handlers::new_base();

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[1].boost_opp = Some(LITTLE_OPPS.len());

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build().unwrap();
        let err = node.init().await.unwrap_err();
        assert!(format!("{err:#}").contains("boost_opp"), "Unexpected error: {err:#}");
    }

    #[fuchsia::test]
    async fn test_init_min_opp_tolerates_unsupported() {
        let handlers = Handlers::new_base();
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.expect_big_min_opp_result(
            (BIG_OPPS.len() - 1) as u32,
            Err(CpuManagerError::Unsupported),
        );
        handlers.expect_little_min_opp_result(
            (LITTLE_OPPS.len() - 1) as u32,
            Err(CpuManagerError::Unsupported),
        );

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        handlers
            .builder(make_default_cluster_configs(), thermal_state_configs)
            .build_and_init()
            .await;
    }

    // Initializing the minimum opp limit is best-effort, so a driver error on one cluster is
    // logged without failing init or skipping the other clusters.
    #[fuchsia::test]
    async fn test_init_min_opp_continues_on_error() {
        let handlers = Handlers::new_base();
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.expect_big_min_opp_result(
            (BIG_OPPS.len() - 1) as u32,
            Err(CpuManagerError::GenericError(anyhow!("driver error"))),
        );
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        handlers
            .builder(make_default_cluster_configs(), thermal_state_configs)
            .build_and_init()
            .await;
    }

    // A boost requested before init completes is recorded and applied when init sets the initial
    // minimum opp limits.
    #[fuchsia::test]
    async fn test_set_boost_before_init() {
        let handlers = Handlers::new_base();

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boost_opp = Some(1);
        cluster_configs[1].boost_opp = Some(2);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build().unwrap();

        // No clusters exist yet, so no messages are sent to the cluster handlers.
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();

        // Init applies the boosted minimum opp limits instead of the lowest-frequency opp.
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.expect_big_min_opp(1);
        handlers.expect_little_min_opp(2);
        node.init().await.unwrap();

        // Disabling boost restores the unboosted minimum opp limits.
        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_boot_max_opp() {
        // Expect init() to apply boot_max_opp ([1, 2]) even though thermal_states[0] is [0, 0].
        let handlers = Handlers::new_with_highest_opp([1, 2]);

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boot_max_opp = Some(1);
        cluster_configs[1].boot_max_opp = Some(2);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build_and_init().await;

        // Verify GetMaxFrequency reports the boot_max_opp values.
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(1))
        );
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(2))
        );

        // Setting an explicit max frequency limit (whether stricter or less restrictive than
        // `boot_max_opp`) replaces `boot_max_opp` rather than being clamped to it.
        handlers.expect_big_opp(2);
        handlers.expect_little_opp(1);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, Some(1)))
            .await
            .unwrap();
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(2))
        );
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(1))
        );

        // Clearing the max frequency limit on each cluster should release the cap to OPP 0.
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
        node.handle_message(&Message::SetMaxFrequency(LITTLE_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();

        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(0))
        );
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(0))
        );
    }

    #[fuchsia::test]
    async fn test_boot_max_opp_validation() {
        // Invalid cases on the last cluster (so both cluster handlers consume
        // `GetCpuOperatingPoints` before validation fails):
        // 1. `boot_max_opp` out of bounds (`LITTLE_OPPS.len()`).
        // 2. `boot_max_opp` less restrictive (lower index) than thermal state 0 (`1 < 2`).
        for (boot_max_opp, state_0_opp) in [(LITTLE_OPPS.len() as u64, 0), (1, 2)] {
            let handlers = Handlers::new_base();

            let mut cluster_configs = make_default_cluster_configs();
            cluster_configs[1].boot_max_opp = Some(boot_max_opp);

            let thermal_state_configs = vec![ThermalStateConfig {
                cluster_opps: vec![0, state_0_opp],
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            }];

            let node = handlers.builder(cluster_configs, thermal_state_configs).build().unwrap();
            assert!(node.init().await.is_err());
        }
    }

    #[fuchsia::test]
    async fn test_boot_min_opp() {
        let handlers = Handlers::new_base();
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        // Expect init() to apply boot_min_opp ([1, 2]) instead of opps.len() - 1 ([2, 3]).
        handlers.expect_big_min_opp(1);
        handlers.expect_little_min_opp(2);

        let mut cluster_configs = make_default_cluster_configs();
        // Big cluster: boost_opp (2) is lower frequency than boot_min_opp (1), so boost must not
        // drop below boot_min_opp (min(2, 1) == 1).
        cluster_configs[0].boot_min_opp = Some(1);
        cluster_configs[0].boost_opp = Some(2);
        // Little cluster: boost_opp (1) is higher frequency than boot_min_opp (2), so boost raises
        // floor to 1, and disabling boost releases boot_min_opp to opps.len() - 1.
        cluster_configs[1].boot_min_opp = Some(2);
        cluster_configs[1].boost_opp = Some(1);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build_and_init().await;

        handlers.expect_big_min_opp(1);
        handlers.expect_little_min_opp(1);
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();

        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_set_boost_enabled_and_disabled_before_init_releases_boot_min_opp() {
        let handlers = Handlers::new_base();

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boot_min_opp = Some(1);
        cluster_configs[1].boot_min_opp = Some(2);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 1.5,
            dynamic_power_per_normperf_w: 0.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build().unwrap();

        // A boost starts and ends before `init()` populates `inner.clusters`.
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();

        // Since an active boost already ended, `init()` should release `boot_min_opp` and set the
        // minimum OPP limit to `opps.len() - 1` rather than holding `boot_min_opp` ([1, 2]).
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.init().await.unwrap();
    }

    #[fuchsia::test]
    async fn test_boot_min_opp_validation() {
        // Invalid cases on the last cluster (so both cluster handlers consume
        // `GetCpuOperatingPoints` before validation fails):
        // 1. `boot_min_opp` out of bounds (`LITTLE_OPPS.len()`).
        // 2. Missing thermal state 0 `cluster_opps` entry for the cluster.
        // 3. `boot_min_opp` higher frequency (lower index) than thermal state 0 (`1 < 2`).
        for (boot_min_opp, cluster_opps, expected_err) in [
            (LITTLE_OPPS.len(), vec![0, 0], "must be less than number of OPPs"),
            (1, vec![0], "Missing thermal state 0 OPP"),
            (1, vec![0, 2], "must not be higher frequency than thermal state 0 OPP"),
        ] {
            let handlers = Handlers::new_base();

            let mut cluster_configs = make_default_cluster_configs();
            cluster_configs[1].boot_min_opp = Some(boot_min_opp);

            let thermal_state_configs = vec![ThermalStateConfig {
                cluster_opps,
                min_performance_normperfs: 0.0,
                static_power_w: 2.0,
                dynamic_power_per_normperf_w: 1.0,
            }];

            let node = handlers.builder(cluster_configs, thermal_state_configs).build().unwrap();
            let err = node.init().await.unwrap_err();
            assert!(format!("{err:#}").contains(expected_err), "Unexpected error: {err:#}");
        }
    }

    #[fuchsia::test]
    async fn test_lease_min_frequency() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers
            .builder(make_default_cluster_configs(), thermal_state_configs)
            .build_and_init()
            .await;

        let default_big_min = (BIG_OPPS.len() - 1) as u64;
        let default_little_min = (LITTLE_OPPS.len() - 1) as u64;

        // Before LeaseMinFrequency is called, GetMinFrequency returns the lowest-frequency opp
        // index.
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == default_big_min
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == default_little_min
        );

        // Lease floor on big cluster only; little cluster is unaffected.
        handlers.expect_big_min_opp(1);
        let big_lease = match node
            .handle_message(&Message::LeaseMinFrequency(
                BIG_CLUSTER_INDEX as u64,
                1,
                "test_big_min".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(1))
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == default_little_min
        );

        // Lease floor on little cluster only; big cluster is unaffected.
        handlers.expect_little_min_opp(0);
        let little_lease = match node
            .handle_message(&Message::LeaseMinFrequency(
                LITTLE_CLUSTER_INDEX as u64,
                0,
                "test_little_min".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(0))
        );

        // Drop floor lease on each cluster independently, restoring the lowest-frequency opp index.
        handlers.expect_big_min_opp(default_big_min as u32);
        drop(big_lease);
        fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == default_big_min
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(0))
        );

        handlers.expect_little_min_opp(default_little_min as u32);
        drop(little_lease);
        fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == default_little_min
        );
    }

    #[fuchsia::test]
    async fn test_min_frequency_and_boost_interaction() {
        let handlers = Handlers::new();

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boost_opp = Some(1);
        cluster_configs[1].boost_opp = Some(1);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers.builder(cluster_configs, thermal_state_configs).build_and_init().await;

        // Lease big cluster floor at 0 (stricter than boost_opp=1) and little cluster floor at 2
        // (weaker than boost_opp=1).
        handlers.expect_big_min_opp(0);
        let big_lease = match node
            .handle_message(&Message::LeaseMinFrequency(
                BIG_CLUSTER_INDEX as u64,
                0,
                "test_big_min".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };
        handlers.expect_little_min_opp(2);
        let _little_lease_weak = match node
            .handle_message(&Message::LeaseMinFrequency(
                LITTLE_CLUSTER_INDEX as u64,
                2,
                "test_little_min_weak".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        // Enabling boost selects the stricter (lower OPP index) floor per cluster:
        // big stays at min(0, 1) = 0, little moves to min(2, 1) = 1.
        handlers.expect_big_min_opp(0);
        handlers.expect_little_min_opp(1);
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();

        // GetMinFrequency still reports the domain's configured floor.
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(0))
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(2))
        );

        // Dropping the big cluster's domain floor lease while boost is still active falls back to
        // boost_opp (1), not the unboosted default (2).
        handlers.expect_big_min_opp(1);
        drop(big_lease);
        fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;

        // Leasing the little cluster's domain floor at 0 while boost is active raises it above
        // boost_opp (min(0, 1) = 0).
        handlers.expect_little_min_opp(0);
        let _little_lease_strict = match node
            .handle_message(&Message::LeaseMinFrequency(
                LITTLE_CLUSTER_INDEX as u64,
                0,
                "test_little_min_strict".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        // Disabling boost restores the unboosted floor on big (2) and keeps the remaining domain
        // floor on little (0).
        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp(0);
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_max_and_min_frequency_invalid_arguments() {
        let handlers = Handlers::new();

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers
            .builder(make_default_cluster_configs(), thermal_state_configs)
            .build_and_init()
            .await;

        let invalid_cluster_index = 99;
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(invalid_cluster_index)).await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::SetMaxFrequency(invalid_cluster_index, Some(0))).await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::SetMaxFrequency(invalid_cluster_index, None)).await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::LeaseMaxFrequency(
                invalid_cluster_index,
                0,
                "test_max".to_string(),
            ))
            .await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(invalid_cluster_index)).await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::LeaseMinFrequency(
                invalid_cluster_index,
                0,
                "test_min".to_string(),
            ))
            .await,
            Err(CpuManagerError::InvalidArgument(_))
        );

        // Out-of-bounds frequency_index must return InvalidArgument without panicking or mutating
        // the stored limit.
        assert_matches!(
            node.handle_message(&Message::SetMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                Some(BIG_OPPS.len() as u64),
            ))
            .await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::LeaseMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                BIG_OPPS.len() as u64,
                "test_max".to_string(),
            ))
            .await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(0))
        );

        assert_matches!(
            node.handle_message(&Message::LeaseMinFrequency(
                LITTLE_CLUSTER_INDEX as u64,
                LITTLE_OPPS.len() as u64,
                "test_min".to_string(),
            ))
            .await,
            Err(CpuManagerError::InvalidArgument(_))
        );
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(LITTLE_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == (LITTLE_OPPS.len() - 1) as u64
        );
    }

    struct RegisteredElement {
        valid_levels: Vec<fbroker::PowerLevel>,
        initial_level: fbroker::PowerLevel,
        current_level: fbroker::PowerLevel,
        dep_token_koid: zx::Koid,
        runner: fbroker::ElementRunnerProxy,
        active_leases: Vec<(String, fbroker::PowerLevel, fbroker::LeaseToken)>,
        _control_stream: fbroker::ElementControlRequestStream,
    }

    struct FakeTopology {
        elements: Rc<RefCell<HashMap<String, RegisteredElement>>>,
        _scope: fasync::Scope,
    }

    impl FakeTopology {
        async fn wait_for_level(&self, elem_name: &str, expected_level: fbroker::PowerLevel) {
            while self.elements.borrow()[elem_name].current_level != expected_level {
                fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;
            }
        }

        fn spawn_lease_watcher(
            scope_handle: &fasync::ScopeHandle,
            elems_watch: Rc<RefCell<HashMap<String, RegisteredElement>>>,
            elem_name: String,
            peer_wait: fbroker::LeaseToken,
        ) {
            scope_handle.spawn_local(async move {
                let _ =
                    fasync::OnSignals::new(&peer_wait, zx::Signals::EVENTPAIR_PEER_CLOSED).await;
                let maybe_update = {
                    let mut elems = elems_watch.borrow_mut();
                    let Some(elem) = elems.get_mut(&elem_name) else {
                        return;
                    };
                    elem.active_leases.retain(|(_, _, t)| {
                        t.wait_one(
                            zx::Signals::EVENTPAIR_PEER_CLOSED,
                            zx::MonotonicInstant::INFINITE_PAST,
                        )
                        .is_err()
                    });
                    let max_level =
                        elem.active_leases.iter().map(|(_, l, _)| *l).max().unwrap_or(0);
                    (max_level != elem.current_level).then(|| (elem.runner.clone(), max_level))
                };
                if let Some((runner, max_level)) = maybe_update {
                    let _ = runner.set_level(max_level).await;
                    if let Some(elem) = elems_watch.borrow_mut().get_mut(&elem_name) {
                        elem.current_level = max_level;
                    }
                }
            });
        }

        fn spawn() -> (Self, fbroker::TopologyProxy) {
            let elements: Rc<RefCell<HashMap<String, RegisteredElement>>> =
                Rc::new(RefCell::new(HashMap::new()));
            let (proxy, mut stream) =
                fidl::endpoints::create_proxy_and_stream::<fbroker::TopologyMarker>();
            let scope = fasync::Scope::new();
            let scope_handle = scope.to_handle();
            let elements_clone = elements.clone();
            scope.spawn_local(async move {
                while let Ok(Some(req)) = stream.try_next().await {
                    match req {
                        fbroker::TopologyRequest::AddElement { payload, responder } => {
                            let name = payload.element_name.unwrap();
                            let valid_levels = payload.valid_levels.unwrap();
                            let initial_level = payload.initial_current_level.unwrap();
                            let initial_lease_token = payload.initial_lease_token;
                            let runner = payload.element_runner.unwrap().into_proxy();
                            let mut control_stream = payload.element_control.unwrap().into_stream();
                            let mut active_leases = Vec::new();
                            let mut peer_wait = None;
                            if let Some(lease_token) = initial_lease_token {
                                peer_wait = Some(
                                    lease_token.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                                );
                                active_leases.push((String::new(), initial_level, lease_token));
                            }
                            responder.send(Ok(())).unwrap();
                            if let Ok(Some(
                                fbroker::ElementControlRequest::RegisterDependencyToken {
                                    token,
                                    responder,
                                },
                            )) = control_stream.try_next().await
                            {
                                let koid = token.koid().unwrap();
                                runner.set_level(initial_level).await.unwrap();
                                elements_clone.borrow_mut().insert(
                                    name.clone(),
                                    RegisteredElement {
                                        valid_levels,
                                        initial_level,
                                        current_level: initial_level,
                                        dep_token_koid: koid,
                                        runner,
                                        active_leases,
                                        _control_stream: control_stream,
                                    },
                                );
                                responder.send(Ok(())).unwrap();
                                if let Some(peer_wait) = peer_wait {
                                    Self::spawn_lease_watcher(
                                        &scope_handle,
                                        elements_clone.clone(),
                                        name,
                                        peer_wait,
                                    );
                                }
                            }
                        }
                        fbroker::TopologyRequest::Lease { payload, responder } => {
                            let lease_name = payload.lease_name.unwrap();
                            let lease_token = payload.lease_token.unwrap();
                            let dep = &payload.dependencies.unwrap()[0];
                            let koid = dep.requires_token.as_ref().unwrap().koid().unwrap();
                            let level = dep.requires_level.unwrap();

                            let (elem_name, maybe_update) = {
                                let mut elems = elements_clone.borrow_mut();
                                let (elem_name, elem) = elems
                                    .iter_mut()
                                    .find(|(_, e)| e.dep_token_koid == koid)
                                    .expect("matching element for dependency token");
                                elem.active_leases.retain(|(_, _, t)| {
                                    t.wait_one(
                                        zx::Signals::EVENTPAIR_PEER_CLOSED,
                                        zx::MonotonicInstant::INFINITE_PAST,
                                    )
                                    .is_err()
                                });
                                elem.active_leases.push((lease_name, level, lease_token));
                                let max_level = elem
                                    .active_leases
                                    .iter()
                                    .map(|(_, l, _)| *l)
                                    .max()
                                    .unwrap_or(0);
                                let changed = max_level != elem.current_level;
                                elem.current_level = max_level;
                                (
                                    elem_name.clone(),
                                    changed.then(|| (elem.runner.clone(), max_level)),
                                )
                            };
                            if let Some((runner, max_level)) = maybe_update {
                                runner.set_level(max_level).await.unwrap();
                            }
                            responder.send(Ok(())).unwrap();

                            // Monitor lease closure so dropping a lease updates required level.
                            let peer_wait = {
                                let elems = elements_clone.borrow();
                                let elem = elems.get(&elem_name).unwrap();
                                elem.active_leases
                                    .last()
                                    .unwrap()
                                    .2
                                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                    .unwrap()
                            };
                            Self::spawn_lease_watcher(
                                &scope_handle,
                                elements_clone.clone(),
                                elem_name,
                                peer_wait,
                            );
                        }
                        fbroker::TopologyRequest::_UnknownMethod { .. } => unreachable!(),
                    }
                }
            });
            (Self { elements, _scope: scope }, proxy)
        }
    }

    #[fuchsia::test]
    async fn test_power_elements_registration_and_arbitration() {
        let (fake_topology, topology_proxy) = FakeTopology::spawn();
        let handlers = Handlers::new();

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers
            .builder(make_default_cluster_configs(), thermal_state_configs)
            .with_topology(topology_proxy)
            .build_and_init()
            .await;

        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(elems.len(), 4);
            assert_eq!(
                elems["cpu_cluster_0_min_freq"].valid_levels,
                (0..BIG_OPPS.len() as u8).collect::<Vec<_>>()
            );
            assert_eq!(elems["cpu_cluster_0_min_freq"].initial_level, 0);
            assert_eq!(
                elems["cpu_cluster_0_max_freq_cap"].valid_levels,
                (0..BIG_OPPS.len() as u8).collect::<Vec<_>>()
            );
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].initial_level, 0);
            assert_eq!(
                elems["cpu_cluster_1_min_freq"].valid_levels,
                (0..LITTLE_OPPS.len() as u8).collect::<Vec<_>>()
            );
            assert_eq!(
                elems["cpu_cluster_1_max_freq_cap"].valid_levels,
                (0..LITTLE_OPPS.len() as u8).collect::<Vec<_>>()
            );
        }

        // Enabling boost acquires a "boost" lease on each cluster's min_freq element at
        // boost_opp (0 -> highest level) and drives SetMinimumOperatingPointLimit(0) via
        // ElementRunner.
        handlers.expect_big_min_opp(0);
        handlers.expect_little_min_opp(0);
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();
        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(elems["cpu_cluster_0_min_freq"].active_leases.len(), 1);
            assert_eq!(elems["cpu_cluster_0_min_freq"].active_leases[0].0, "boost");
            assert_eq!(
                elems["cpu_cluster_0_min_freq"].active_leases[0].1,
                opp_to_min_freq_level(0, BIG_OPPS.len())
            );
        }

        // Setting max frequency on big cluster to OPP 2 acquires a named lease at level 2 and
        // drives SetOperatingPoint(2) via ElementRunner.
        handlers.expect_big_opp(2);
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, Some(2)))
            .await
            .unwrap();
        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].active_leases.len(), 1);
            assert_eq!(
                elems["cpu_cluster_0_max_freq_cap"].active_leases[0].0,
                "domain_controller_max_freq"
            );
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].active_leases[0].1, 2);
        }
    }

    #[fuchsia::test]
    async fn test_multi_client_named_frequency_leases() {
        let handlers = Handlers::new();
        let (fake_topology, topology_proxy) = FakeTopology::spawn();

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = CpuManagerMainBuilder::new(
            DEFAULT_SUSTAINABLE_POWER,
            DEFAULT_POWER_GAIN,
            make_default_cluster_configs(),
            vec![handlers.big_cluster.clone(), handlers.little_cluster.clone()],
            thermal_state_configs,
            handlers.syscall.clone(),
            handlers.cpu_stats.clone(),
        )
        .with_topology(topology_proxy)
        .build_and_init()
        .await;

        // Client A caps big cluster to OPP 1; Client B caps big cluster to stricter OPP 2.
        handlers.expect_big_opp(1);
        let max_lease_a = match node
            .handle_message(&Message::LeaseMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                1,
                "starnix:powerhint_max_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMaxFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        handlers.expect_big_opp(2);
        let max_lease_b = match node
            .handle_message(&Message::LeaseMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                2,
                "starnix:thermal_max_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMaxFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        // Effective ceiling must be OPP 2 while both leases are held.
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(2))
        );
        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].active_leases.len(), 2);
        }

        // Dropping Client B's stricter lease relaxes ceiling to Client A's OPP 1 (NOT uncapped 0).
        handlers.expect_big_opp(1);
        drop(max_lease_b);
        fake_topology.wait_for_level("cpu_cluster_0_max_freq_cap", 1).await;
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(1))
        );

        // Dropping Client A's lease restores uncapped OPP 0.
        handlers.expect_big_opp(0);
        drop(max_lease_a);
        fake_topology.wait_for_level("cpu_cluster_0_max_freq_cap", 0).await;
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(0))
        );

        // Concurrent floor leases: Client A requests floor OPP 1, Client B requests higher floor OPP 0.
        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(1)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let min_lease_a = match node
            .handle_message(&Message::LeaseMinFrequency(
                BIG_CLUSTER_INDEX as u64,
                1,
                "starnix:powerhint_min_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        handlers.big_cluster.add_msg_response_pair((
            msg_eq!(SetMinimumOperatingPointLimit(0)),
            msg_ok_return!(SetMinimumOperatingPointLimit),
        ));
        let min_lease_b = match node
            .handle_message(&Message::LeaseMinFrequency(
                BIG_CLUSTER_INDEX as u64,
                0,
                "starnix:debug_min_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMinFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };

        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(0))
        );

        // Dropping Client B relaxes floor to Client A's OPP 1.
        handlers.expect_big_min_opp(1);
        drop(min_lease_b);
        fake_topology
            .wait_for_level("cpu_cluster_0_min_freq", opp_to_min_freq_level(1, BIG_OPPS.len()))
            .await;
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(1))
        );

        // Dropping Client A restores lowest OPP 2.
        handlers.expect_big_min_opp(2);
        drop(min_lease_a);
        fake_topology.wait_for_level("cpu_cluster_0_min_freq", 0).await;
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(2))
        );
    }

    #[fuchsia::test]
    async fn test_boot_max_opp_lease_handoff() {
        let handlers = Handlers::new_with_highest_opp([1, 0]);
        let (fake_topology, topology_proxy) = FakeTopology::spawn();

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boot_max_opp = Some(1);
        cluster_configs[1].boot_max_opp = Some(0);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers
            .builder(cluster_configs, thermal_state_configs)
            .with_topology(topology_proxy)
            .build_and_init()
            .await;

        // Big cluster starts with an initial lease token at OPP 1 via `Topology.AddElement`;
        // little cluster has boot_max_opp = 0 (uncapped) and holds no ceiling lease.
        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].initial_level, 1);
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].active_leases.len(), 1);
            assert_eq!(elems["cpu_cluster_0_max_freq_cap"].active_leases[0].1, 1);
            assert_eq!(elems["cpu_cluster_1_max_freq_cap"].initial_level, 0);
            assert!(elems["cpu_cluster_1_max_freq_cap"].active_leases.is_empty());
        }

        // Simulate Starnix boot handoff: query GetMaxFrequency, acquire a named
        // "starnix:soft_max_freq" lease at OPP 1, then ClearMaxFrequency (SetMaxFrequency(None)).
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(1))
        );
        let soft_max_lease = match node
            .handle_message(&Message::LeaseMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                1,
                "starnix:soft_max_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMaxFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };
        node.handle_message(&Message::SetMaxFrequency(BIG_CLUSTER_INDEX as u64, None))
            .await
            .unwrap();
        while fake_topology.elements.borrow()["cpu_cluster_0_max_freq_cap"].active_leases.len() != 1
        {
            fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;
        }
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(1))
        );

        // A stricter thermal lease (OPP 2) lowers the ceiling to OPP 2.
        handlers.expect_big_opp(2);
        let thermal_lease = match node
            .handle_message(&Message::LeaseMaxFrequency(
                BIG_CLUSTER_INDEX as u64,
                2,
                "starnix:thermal_max_freq".to_string(),
            ))
            .await
            .unwrap()
        {
            MessageReturn::LeaseMaxFrequency(token) => token,
            other => panic!("unexpected return: {:?}", other),
        };
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(2))
        );

        // Releasing the boot cap (dropping `soft_max_lease` on `sys.boot_completed=1`) while
        // `starnix:thermal_max_freq` is active preserves the stricter thermal ceiling at OPP 2.
        drop(soft_max_lease);
        while fake_topology.elements.borrow()["cpu_cluster_0_max_freq_cap"].active_leases.len() != 1
        {
            fasync::Timer::new(zx::MonotonicDuration::from_millis(5)).await;
        }
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(2))
        );

        // Dropping the remaining thermal lease restores uncapped OPP 0.
        handlers.expect_big_opp(0);
        drop(thermal_lease);
        fake_topology.wait_for_level("cpu_cluster_0_max_freq_cap", 0).await;
        assert_matches!(
            node.handle_message(&Message::GetMaxFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMaxFrequency(0))
        );
    }

    #[fuchsia::test]
    async fn test_boot_min_opp_power_broker_lease() {
        let handlers = Handlers::new_base();
        let (fake_topology, topology_proxy) = FakeTopology::spawn();
        handlers.expect_big_opp(0);
        handlers.expect_little_opp(0);
        handlers.expect_big_min_opp(1);
        handlers.expect_little_min_opp(2);

        let mut cluster_configs = make_default_cluster_configs();
        cluster_configs[0].boot_min_opp = Some(1);
        cluster_configs[0].boost_opp = Some(2);
        cluster_configs[1].boot_min_opp = Some(2);
        cluster_configs[1].boost_opp = Some(1);

        let thermal_state_configs = vec![ThermalStateConfig {
            cluster_opps: vec![0, 0],
            min_performance_normperfs: 0.0,
            static_power_w: 2.0,
            dynamic_power_per_normperf_w: 1.0,
        }];

        let node = handlers
            .builder(cluster_configs, thermal_state_configs)
            .with_topology(topology_proxy)
            .build_and_init()
            .await;

        // Big cluster starts with an initial lease token at boot_min_opp (1) via
        // `Topology.AddElement`, and GetMinFrequency reports boot_min_opp.
        {
            let elems = fake_topology.elements.borrow();
            assert_eq!(
                elems["cpu_cluster_0_min_freq"].initial_level,
                opp_to_min_freq_level(1, BIG_OPPS.len())
            );
            assert_eq!(elems["cpu_cluster_0_min_freq"].active_leases.len(), 1);
            assert_eq!(
                elems["cpu_cluster_0_min_freq"].active_leases[0].1,
                opp_to_min_freq_level(1, BIG_OPPS.len())
            );
        }
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(1))
        );

        // Enabling boost transitions to a "boost" lease at boost_opp.min(boot_min_opp).
        // Big cluster stays at OPP 1 (no SetLevel change), while little cluster raises to OPP 1.
        handlers.expect_little_min_opp(1);
        node.handle_message(&Message::SetBoost(true, 0)).await.unwrap();

        // Disabling boost clears boot_min_opp and drops the floor lease, restoring opps.len() - 1.
        handlers.expect_big_min_opp((BIG_OPPS.len() - 1) as u32);
        handlers.expect_little_min_opp((LITTLE_OPPS.len() - 1) as u32);
        node.handle_message(&Message::SetBoost(false, 0)).await.unwrap();
        fake_topology.wait_for_level("cpu_cluster_0_min_freq", 0).await;
        fake_topology.wait_for_level("cpu_cluster_1_min_freq", 0).await;
        assert_matches!(
            node.handle_message(&Message::GetMinFrequency(BIG_CLUSTER_INDEX as u64)).await,
            Ok(MessageReturn::GetMinFrequency(opp)) if opp == (BIG_OPPS.len() - 1) as u64
        );
    }
}
