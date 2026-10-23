// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node::Node;
use anyhow::{Error, ensure, format_err};
use async_trait::async_trait;
use async_utils::event::Event as AsyncEvent;
use async_utils::hanging_get::server as hanging_get;
use fidl_fuchsia_power_powercap as fpowercap;
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceFsDir, ServiceObjLocal};
use fuchsia_inspect::{self as inspect, ArrayProperty as _, NumericProperty as _, Property as _};
use futures::prelude::*;
use log::{error, info, warn};
use serde_derive::Deserialize;
use serde_json as json;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use zx::BootDuration;

/// Node: PowercapController
///
/// Summary: Serves `fuchsia.power.powercap.Client` to expose a configurable powercap zone hierarchy
///          to Starnix (`/sys/class/powercap`) and translates per-zone microwatt power limits and
///          transient bypass timers into per-cluster CPU OPP ceilings published over
///          `fuchsia.power.powercap.ClientStateWatcher`.
///
/// Handles Messages: N/A
///
/// Sends Messages: N/A
///
/// FIDL dependencies:
///     - fuchsia.power.powercap.Client: served to Starnix for `/sys/class/powercap` reads/writes.
///     - fuchsia.power.powercap.ClientStateWatcher: hanging-get protocol served to downstream
///       watchers (such as `cpu_manager`'s `PowercapWatcher`) to subscribe to per-cluster OPP
///       ceiling changes (`domain_opps`). `domain_opps` has length `max(cluster_id) + 1` across
///       configured `domain_mappings` (or `0` when no `domain_mappings` exist, up to
///       `MAX_DOMAINS`), and any cluster with `cluster_index >= domain_opps.len()` has no powercap
///       ceiling (`None`).

#[derive(Clone, Debug, Deserialize)]
pub struct PowercapDomainMappingConfig {
    pub cluster_id: usize,
    #[serde(default)]
    pub opps: Option<Vec<u64>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PowercapZoneNodeConfig {
    pub node_id: u64,
    #[serde(default)]
    pub parent_node_id: Option<u64>,
    pub zone_path: String,
    pub name: String,
    #[serde(default)]
    pub constraint_name: Option<String>,
    #[serde(default)]
    pub max_power_uw: Option<u64>,
    #[serde(default)]
    pub min_power_uw: Option<u64>,
    #[serde(default)]
    pub power_levels_uw: Option<Vec<u64>>,
    #[serde(default)]
    pub supports_bypass: bool,
    #[serde(default)]
    pub domain_mappings: Vec<PowercapDomainMappingConfig>,
}

#[derive(Clone, Debug, Deserialize)]
struct Config {
    nodes: Vec<PowercapZoneNodeConfig>,
}

#[derive(Deserialize)]
struct JsonData {
    name: String,
    config: Config,
}

pub struct PowercapControllerBuilder<'a, 'b> {
    node_name: String,
    nodes: Vec<PowercapZoneNodeConfig>,
    outgoing_svc_dir: Option<ServiceFsDir<'a, ServiceObjLocal<'b, ()>>>,
    inspect_root: Option<&'a inspect::Node>,
}

impl<'a, 'b> PowercapControllerBuilder<'a, 'b> {
    pub fn new_from_json(
        json_data: json::Value,
        service_fs: &'a mut ServiceFs<ServiceObjLocal<'b, ()>>,
    ) -> Self {
        let data: JsonData = json::from_value(json_data).unwrap();
        Self {
            node_name: data.name,
            nodes: data.config.nodes,
            outgoing_svc_dir: Some(service_fs.dir("svc")),
            inspect_root: None,
        }
    }

    #[cfg(test)]
    pub fn new(nodes: Vec<PowercapZoneNodeConfig>) -> Self {
        Self {
            node_name: "PowercapController".to_string(),
            nodes,
            outgoing_svc_dir: None,
            inspect_root: None,
        }
    }

    #[cfg(test)]
    pub fn with_outgoing_svc_dir(
        mut self,
        outgoing_svc_dir: ServiceFsDir<'a, ServiceObjLocal<'b, ()>>,
    ) -> Self {
        self.outgoing_svc_dir = Some(outgoing_svc_dir);
        self
    }

    #[cfg(test)]
    pub fn with_inspect_root(mut self, root: &'a inspect::Node) -> Self {
        self.inspect_root = Some(root);
        self
    }

    pub fn build(self) -> Result<Rc<PowercapController>, Error> {
        let inspect_root =
            self.inspect_root.unwrap_or_else(|| inspect::component::inspector().root());
        let inspect_node = inspect_root.create_child(&self.node_name);
        let zones_inspect_node = inspect_node.create_child("zones");

        ensure!(
            self.nodes.len() <= fpowercap::MAX_NODES as usize,
            "Powercap config has {} nodes, exceeding MAX_NODES ({})",
            self.nodes.len(),
            fpowercap::MAX_NODES
        );

        let mut seen_ids = HashSet::new();
        let mut seen_zone_paths = HashSet::new();
        let mut parent_map = BTreeMap::new();
        let mut zones = BTreeMap::new();
        let mut tree_nodes = Vec::with_capacity(self.nodes.len());
        let mut num_clusters = 0usize;

        for cfg in self.nodes {
            ensure!(!cfg.zone_path.is_empty(), "Powercap node {} has empty zone_path", cfg.node_id);
            ensure!(
                cfg.zone_path.len() <= fpowercap::MAX_PATH_LEN as usize,
                "Zone '{}' (node_id {}) zone_path length ({}) exceeds MAX_PATH_LEN ({})",
                cfg.zone_path,
                cfg.node_id,
                cfg.zone_path.len(),
                fpowercap::MAX_PATH_LEN
            );
            ensure!(
                !cfg.name.is_empty(),
                "Zone '{}' (node_id {}) has empty name",
                cfg.zone_path,
                cfg.node_id
            );
            ensure!(
                cfg.name.len() <= fpowercap::MAX_NAME_LEN as usize,
                "Zone '{}' (node_id {}) name length ({}) exceeds MAX_NAME_LEN ({})",
                cfg.zone_path,
                cfg.node_id,
                cfg.name.len(),
                fpowercap::MAX_NAME_LEN
            );
            if let Some(ref constraint_name) = cfg.constraint_name {
                ensure!(
                    !constraint_name.is_empty(),
                    "Zone '{}' (node_id {}) has empty constraint_name",
                    cfg.zone_path,
                    cfg.node_id
                );
                ensure!(
                    constraint_name.len() <= fpowercap::MAX_NAME_LEN as usize,
                    "Zone '{}' (node_id {}) constraint_name length ({}) exceeds MAX_NAME_LEN ({})",
                    cfg.zone_path,
                    cfg.node_id,
                    constraint_name.len(),
                    fpowercap::MAX_NAME_LEN
                );
            }
            ensure!(
                seen_ids.insert(cfg.node_id),
                "Duplicate powercap node_id {} for zone '{}'",
                cfg.node_id,
                cfg.zone_path
            );
            ensure!(
                seen_zone_paths.insert(cfg.zone_path.clone()),
                "Duplicate powercap zone_path '{}' for node_id {}",
                cfg.zone_path,
                cfg.node_id
            );
            if let (Some(min_uw), Some(max_uw)) = (cfg.min_power_uw, cfg.max_power_uw) {
                ensure!(
                    min_uw <= max_uw,
                    "Zone '{}' min_power_uw ({}) must be <= max_power_uw ({})",
                    cfg.zone_path,
                    min_uw,
                    max_uw
                );
            }

            let power_levels = cfg.power_levels_uw.clone().unwrap_or_default();
            ensure!(
                power_levels.len() <= fpowercap::MAX_POWER_LEVELS as usize,
                "Zone '{}' power_levels_uw length ({}) exceeds MAX_POWER_LEVELS ({})",
                cfg.zone_path,
                power_levels.len(),
                fpowercap::MAX_POWER_LEVELS
            );
            for window in power_levels.windows(2) {
                ensure!(
                    window[0] > window[1],
                    "Zone '{}' power_levels_uw must be ordered from highest to lowest ({} <= {})",
                    cfg.zone_path,
                    window[0],
                    window[1]
                );
            }
            if let (Some(&first_level), Some(&last_level)) =
                (power_levels.first(), power_levels.last())
            {
                if let Some(max_uw) = cfg.max_power_uw {
                    ensure!(
                        max_uw == first_level,
                        "Zone '{}' max_power_uw ({}) must equal power_levels_uw[0] ({})",
                        cfg.zone_path,
                        max_uw,
                        first_level
                    );
                }
                if let Some(min_uw) = cfg.min_power_uw {
                    ensure!(
                        min_uw <= last_level,
                        "Zone '{}' min_power_uw ({}) must be <= lowest power_levels_uw ({})",
                        cfg.zone_path,
                        min_uw,
                        last_level
                    );
                }
            }

            let mut seen_clusters = HashSet::new();
            let mut resolved_mappings = Vec::with_capacity(cfg.domain_mappings.len());
            for mapping in cfg.domain_mappings {
                ensure!(
                    mapping.cluster_id < fpowercap::MAX_DOMAINS as usize,
                    "Zone '{}' domain_mapping cluster_id ({}) must be < MAX_DOMAINS ({})",
                    cfg.zone_path,
                    mapping.cluster_id,
                    fpowercap::MAX_DOMAINS
                );
                ensure!(
                    !power_levels.is_empty(),
                    "Zone '{}' defines domain_mappings for cluster {} but has no power_levels_uw",
                    cfg.zone_path,
                    mapping.cluster_id
                );
                ensure!(
                    seen_clusters.insert(mapping.cluster_id),
                    "Zone '{}' has duplicate domain_mapping for cluster {}",
                    cfg.zone_path,
                    mapping.cluster_id
                );
                let opps = match mapping.opps {
                    Some(opps) => {
                        ensure!(
                            opps.len() == power_levels.len(),
                            "Zone '{}' cluster {} opps length ({}) must match power_levels_uw length ({})",
                            cfg.zone_path,
                            mapping.cluster_id,
                            opps.len(),
                            power_levels.len()
                        );
                        for window in opps.windows(2) {
                            ensure!(
                                window[0] <= window[1],
                                "Zone '{}' cluster {} opps must be non-decreasing ({} > {})",
                                cfg.zone_path,
                                mapping.cluster_id,
                                window[0],
                                window[1]
                            );
                        }
                        opps
                    }
                    None => (0..power_levels.len() as u64).collect(),
                };
                // `domain_opps` is sized to `max(cluster_id) + 1` across all configured mappings
                // (up to `MAX_DOMAINS`); clusters with indices `>= domain_opps.len()` have no
                // powercap ceiling (`None`).
                num_clusters = num_clusters.max(mapping.cluster_id + 1);
                resolved_mappings
                    .push(ResolvedDomainMapping { cluster_id: mapping.cluster_id, opps });
            }

            parent_map.insert(cfg.node_id, cfg.parent_node_id);

            let has_constraint = cfg.constraint_name.is_some()
                || cfg.max_power_uw.is_some()
                || cfg.min_power_uw.is_some()
                || !power_levels.is_empty();
            let initial_power_limit_uw =
                cfg.max_power_uw.or_else(|| power_levels.first().copied()).unwrap_or(0);

            let zone_inspect = ZoneInspect::new(
                zones_inspect_node.create_child(&cfg.zone_path),
                &cfg.name,
                initial_power_limit_uw,
            );

            let fidl_info = fpowercap::PowercapNodeInfo {
                node_id: Some(cfg.node_id),
                parent_node_id: cfg.parent_node_id,
                zone_path: Some(cfg.zone_path),
                name: Some(cfg.name),
                constraint_name: cfg.constraint_name,
                max_power_uw: cfg.max_power_uw,
                min_power_uw: cfg.min_power_uw,
                power_levels_uw: cfg.power_levels_uw,
                supports_bypass: Some(cfg.supports_bypass),
                ..Default::default()
            };
            tree_nodes.push(fidl_info);

            zones.insert(
                cfg.node_id,
                PowercapZoneState {
                    has_constraint,
                    supports_bypass: cfg.supports_bypass,
                    power_levels_uw: power_levels,
                    domain_mappings: resolved_mappings,
                    power_limit_uw: initial_power_limit_uw,
                    bypass_deadline: None,
                    inspect: zone_inspect,
                },
            );
        }

        for info in &tree_nodes {
            let Some(start_id) = info.node_id else {
                continue;
            };
            let mut visited = HashSet::from([start_id]);
            let mut curr_parent = info.parent_node_id;
            while let Some(parent_id) = curr_parent {
                ensure!(
                    zones.contains_key(&parent_id),
                    "Zone '{:?}' references unknown parent_node_id {}",
                    info.zone_path,
                    parent_id
                );
                ensure!(
                    visited.insert(parent_id),
                    "Zone '{:?}' has cyclic parent_node_id chain involving {}",
                    info.zone_path,
                    parent_id
                );
                curr_parent = parent_map.get(&parent_id).copied().flatten();
            }
        }

        let initial_domain_opps = vec![0u64; num_clusters];
        let state_broker = new_state_broker(initial_domain_opps.clone());
        let state_publisher = state_broker.new_publisher();

        let domain_opps_inspect = inspect_node.create_uint_array("domain_opps", num_clusters);
        for (i, &opp) in initial_domain_opps.iter().enumerate() {
            domain_opps_inspect.set(i, opp);
        }
        let connect_count = inspect_node.create_uint("connect_count", 0);
        inspect_node.record(zones_inspect_node);

        let node = Rc::new(PowercapController {
            init_done: AsyncEvent::new(),
            tree_nodes,
            num_clusters,
            inner: RefCell::new(MutableInner {
                zones,
                domain_opps: initial_domain_opps,
                state_publisher,
                state_broker,
            }),
            inspect: InspectData {
                _root_node: inspect_node,
                domain_opps: domain_opps_inspect,
                connect_count,
            },
            scope: fasync::Scope::new(),
        });

        if let Some(mut outgoing_svc_dir) = self.outgoing_svc_dir {
            node.publish_services(&mut outgoing_svc_dir);
        }

        Ok(node)
    }
}

#[derive(Clone, Debug)]
struct ResolvedDomainMapping {
    cluster_id: usize,
    opps: Vec<u64>,
}

struct ZoneInspect {
    power_limit_uw: inspect::UintProperty,
    bypass_active: inspect::BoolProperty,
    _node: inspect::Node,
}

impl ZoneInspect {
    fn new(node: inspect::Node, name: &str, initial_power_limit_uw: u64) -> Self {
        node.record_string("name", name);
        let power_limit_uw = node.create_uint("power_limit_uw", initial_power_limit_uw);
        let bypass_active = node.create_bool("bypass_active", false);
        Self { power_limit_uw, bypass_active, _node: node }
    }
}

struct InspectData {
    _root_node: inspect::Node,
    domain_opps: inspect::UintArrayProperty,
    connect_count: inspect::UintProperty,
}

struct PowercapZoneState {
    has_constraint: bool,
    supports_bypass: bool,
    power_levels_uw: Vec<u64>,
    domain_mappings: Vec<ResolvedDomainMapping>,
    power_limit_uw: u64,
    bypass_deadline: Option<fasync::BootInstant>,
    inspect: ZoneInspect,
}

type WatchResponder = fpowercap::ClientStateWatcherWatchResponder;
type StateChangeFn = Box<dyn Fn(&Vec<u64>, WatchResponder) -> bool>;
type ClientStateBroker = hanging_get::HangingGet<Vec<u64>, WatchResponder, StateChangeFn>;
type ClientStatePublisher = hanging_get::Publisher<Vec<u64>, WatchResponder, StateChangeFn>;

fn new_state_broker(initial_state: Vec<u64>) -> ClientStateBroker {
    let notify_fn: StateChangeFn =
        Box::new(|domain_opps, responder| match responder.send(domain_opps) {
            Ok(()) => true,
            Err(e) => {
                error!("Failed to send powercap domain_opps to client: {}", e);
                false
            }
        });
    hanging_get::HangingGet::new(initial_state, notify_fn)
}

struct MutableInner {
    zones: BTreeMap<u64, PowercapZoneState>,
    domain_opps: Vec<u64>,
    state_publisher: ClientStatePublisher,
    state_broker: ClientStateBroker,
}

pub struct PowercapController {
    init_done: AsyncEvent,
    tree_nodes: Vec<fpowercap::PowercapNodeInfo>,
    num_clusters: usize,
    inner: RefCell<MutableInner>,
    inspect: InspectData,
    scope: fasync::Scope,
}

impl PowercapController {
    fn publish_services<'a, 'b>(
        self: &Rc<Self>,
        outgoing_svc_dir: &mut ServiceFsDir<'a, ServiceObjLocal<'b, ()>>,
    ) {
        let this = self.clone();
        let scope = self.scope.to_handle();
        outgoing_svc_dir.add_fidl_service(move |stream: fpowercap::ClientRequestStream| {
            let this = this.clone();
            scope.spawn_local(
                this.handle_client_stream(stream)
                    .unwrap_or_else(|e: anyhow::Error| error!("Powercap Client error: {:?}", e)),
            );
        });

        let this = self.clone();
        let scope = self.scope.to_handle();
        outgoing_svc_dir.add_fidl_service(
            move |stream: fpowercap::ClientStateWatcherRequestStream| {
                let this = this.clone();
                scope.spawn_local(this.handle_watcher_stream(stream).unwrap_or_else(
                    |e: anyhow::Error| error!("Powercap ClientStateWatcher error: {:?}", e),
                ));
            },
        );
    }

    async fn handle_client_stream(
        self: Rc<Self>,
        mut stream: fpowercap::ClientRequestStream,
    ) -> Result<(), Error> {
        self.init_done.wait().await;
        while let Some(req) = stream.try_next().await? {
            match req {
                fpowercap::ClientRequest::GetPowercapTree { responder } => {
                    let _ = responder.send(&self.tree_nodes);
                }
                fpowercap::ClientRequest::GetPowerLimit { node_id, responder } => {
                    let result = self.get_power_limit(node_id);
                    let _ = responder.send(result);
                }
                fpowercap::ClientRequest::SetPowerLimit { node_id, power_limit_uw, responder } => {
                    let result = self.set_power_limit(node_id, power_limit_uw);
                    let _ = responder.send(result);
                }
                fpowercap::ClientRequest::GetPowerLimitBypass { node_id, responder } => {
                    let result = self.get_power_limit_bypass(node_id);
                    let _ = responder.send(result);
                }
                fpowercap::ClientRequest::SetPowerLimitBypass {
                    node_id,
                    bypass_time_ms,
                    responder,
                } => {
                    let result = self.set_power_limit_bypass(node_id, bypass_time_ms);
                    let _ = responder.send(result);
                }
                fpowercap::ClientRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("Unknown fuchsia.power.powercap.Client method ordinal: {}", ordinal);
                }
            }
        }
        Ok(())
    }

    async fn handle_watcher_stream(
        self: Rc<Self>,
        mut stream: fpowercap::ClientStateWatcherRequestStream,
    ) -> Result<(), Error> {
        self.init_done.wait().await;
        info!("Powercap client watcher connected");
        self.inspect.connect_count.add(1);
        let subscriber = self.inner.borrow_mut().state_broker.new_subscriber();
        while let Some(req) = stream.try_next().await? {
            match req {
                fpowercap::ClientStateWatcherRequest::Watch { responder } => {
                    subscriber
                        .register(responder)
                        .map_err(|e| format_err!("Failed to register watcher: {:?}", e))?;
                }
                fpowercap::ClientStateWatcherRequest::_UnknownMethod { ordinal, .. } => {
                    warn!("Unknown fuchsia.power.powercap.ClientStateWatcher ordinal: {}", ordinal);
                }
            }
        }
        Ok(())
    }

    fn get_power_limit(&self, node_id: u64) -> Result<u64, fpowercap::PowercapError> {
        let inner = self.inner.borrow();
        let zone = inner.zones.get(&node_id).ok_or(fpowercap::PowercapError::InvalidArguments)?;
        if !zone.has_constraint {
            return Err(fpowercap::PowercapError::InvalidArguments);
        }
        Ok(zone.power_limit_uw)
    }

    /// Updates the power limit for `node_id` and recomputes per-cluster OPP ceilings.
    ///
    /// Returns `PowercapError::InvalidArguments` if `node_id` does not exist or is an
    /// unconstrained container zone (where `constraint_name`, `max_power_uw`/`min_power_uw`, and
    /// `power_levels_uw` are all absent, such as the root `gpc:0` zone).
    ///
    /// For constrained zones with `domain_mappings`, out-of-range budgets store the written value
    /// for sysfs readback and snap to the first `power_levels_uw <= power_limit_uw` (or the
    /// lowest-power OPP if below `min_power_uw`), matching Linux powercap semantics. For
    /// constrained zones without CPU `domain_mappings` (such as container zones or non-CPU devfreq
    /// zones), the written limit is stored for sysfs readback and Inspect without affecting CPU
    /// `domain_opps`.
    fn set_power_limit(
        &self,
        node_id: u64,
        power_limit_uw: u64,
    ) -> Result<(), fpowercap::PowercapError> {
        {
            let mut inner = self.inner.borrow_mut();
            let zone =
                inner.zones.get_mut(&node_id).ok_or(fpowercap::PowercapError::InvalidArguments)?;
            if !zone.has_constraint {
                return Err(fpowercap::PowercapError::InvalidArguments);
            }
            zone.power_limit_uw = power_limit_uw;
            zone.inspect.power_limit_uw.set(power_limit_uw);
        }
        self.recompute_and_publish_domain_opps();
        Ok(())
    }

    fn get_power_limit_bypass(&self, node_id: u64) -> Result<u64, fpowercap::PowercapError> {
        let inner = self.inner.borrow();
        let zone = inner.zones.get(&node_id).ok_or(fpowercap::PowercapError::InvalidArguments)?;
        if !zone.supports_bypass {
            return Err(fpowercap::PowercapError::InvalidArguments);
        }
        let Some(deadline) = zone.bypass_deadline else {
            return Ok(0);
        };
        let now = fasync::BootInstant::now();
        if deadline <= now {
            return Ok(0);
        }
        let remaining_ms = (deadline - now).into_millis();
        Ok(u64::try_from(remaining_ms).unwrap_or(0))
    }

    /// Sets or cancels a temporary boot-time power-limit bypass window for `node_id`.
    ///
    /// While `node_id`'s bypass is active, `node_id`'s own power limit constraint is lifted
    /// (`level_idx = 0`), while `domain_opps` still takes the strictest (maximum) OPP ceiling
    /// across all non-bypassed zones mapped to the same cluster (matching Linux powercap per-zone
    /// bypass semantics for multi-domain zones).
    fn set_power_limit_bypass(
        self: &Rc<Self>,
        node_id: u64,
        bypass_time_ms: u64,
    ) -> Result<(), fpowercap::PowercapError> {
        {
            let mut inner = self.inner.borrow_mut();
            let zone =
                inner.zones.get_mut(&node_id).ok_or(fpowercap::PowercapError::InvalidArguments)?;
            if !zone.supports_bypass {
                return Err(fpowercap::PowercapError::InvalidArguments);
            }

            if bypass_time_ms == 0 {
                zone.bypass_deadline = None;
                zone.inspect.bypass_active.set(false);
            } else {
                let duration_ms = i64::try_from(bypass_time_ms).unwrap_or(i64::MAX);
                let deadline = fasync::BootInstant::after(BootDuration::from_millis(duration_ms));
                zone.bypass_deadline = Some(deadline);
                zone.inspect.bypass_active.set(true);

                let weak_self = Rc::downgrade(self);
                self.scope.spawn_local(async move {
                    fasync::Timer::new(deadline).await;
                    if let Some(this) = weak_self.upgrade() {
                        this.expire_bypass(node_id, deadline);
                    }
                });
            }
        }
        self.recompute_and_publish_domain_opps();
        Ok(())
    }

    fn expire_bypass(&self, node_id: u64, expected_deadline: fasync::BootInstant) {
        let mut expired = false;
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(zone) = inner.zones.get_mut(&node_id) {
                if zone.bypass_deadline == Some(expected_deadline) {
                    zone.bypass_deadline = None;
                    zone.inspect.bypass_active.set(false);
                    expired = true;
                }
            }
        }
        if expired {
            self.recompute_and_publish_domain_opps();
        }
    }

    fn recompute_and_publish_domain_opps(&self) {
        if self.num_clusters == 0 {
            return;
        }

        let mut inner = self.inner.borrow_mut();
        let now = fasync::BootInstant::now();
        let mut new_opps = vec![0u64; self.num_clusters];

        for zone in inner.zones.values_mut() {
            let bypassed = match zone.bypass_deadline {
                Some(deadline) if deadline > now => true,
                Some(_) => {
                    zone.bypass_deadline = None;
                    zone.inspect.bypass_active.set(false);
                    false
                }
                None => false,
            };

            if zone.domain_mappings.is_empty() || zone.power_levels_uw.is_empty() {
                continue;
            }

            // Per-zone bypass lifts only this zone's constraint to level 0; other non-bypassed
            // zones mapping the same cluster still contribute their OPP ceilings via `.max()`.
            let level_idx = if bypassed {
                0
            } else {
                zone.power_levels_uw
                    .iter()
                    .position(|&level_uw| level_uw <= zone.power_limit_uw)
                    .unwrap_or(zone.power_levels_uw.len() - 1)
            };

            for mapping in &zone.domain_mappings {
                if let (Some(&opp), Some(cluster_opp)) =
                    (mapping.opps.get(level_idx), new_opps.get_mut(mapping.cluster_id))
                {
                    *cluster_opp = (*cluster_opp).max(opp);
                }
            }
        }

        if new_opps != inner.domain_opps {
            for (i, &opp) in new_opps.iter().enumerate() {
                self.inspect.domain_opps.set(i, opp);
            }
            inner.domain_opps = new_opps.clone();
            inner.state_publisher.set(new_opps);
        }
    }
}

#[async_trait(?Send)]
impl Node for PowercapController {
    fn name(&self) -> String {
        "PowercapController".to_string()
    }

    async fn init(&self) -> Result<(), Error> {
        self.init_done.signal();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use futures::task::Poll;

    fn make_test_zones() -> Vec<PowercapZoneNodeConfig> {
        vec![
            PowercapZoneNodeConfig {
                node_id: 0,
                parent_node_id: None,
                zone_path: "gpc:0".to_string(),
                name: "gpc".to_string(),
                constraint_name: None,
                max_power_uw: None,
                min_power_uw: None,
                power_levels_uw: None,
                supports_bypass: false,
                domain_mappings: vec![],
            },
            PowercapZoneNodeConfig {
                node_id: 1,
                parent_node_id: Some(0),
                zone_path: "gpc:0:0".to_string(),
                name: "soc".to_string(),
                constraint_name: Some("UW".to_string()),
                max_power_uw: Some(18240000),
                min_power_uw: Some(344000),
                power_levels_uw: None,
                supports_bypass: true,
                domain_mappings: vec![],
            },
            PowercapZoneNodeConfig {
                node_id: 2,
                parent_node_id: Some(1),
                zone_path: "gpc:0:0:0".to_string(),
                name: "cpufreq-cpu0".to_string(),
                constraint_name: Some("UW".to_string()),
                max_power_uw: Some(999000),
                min_power_uw: Some(67000),
                power_levels_uw: Some(vec![999000, 609000, 318000, 67000]),
                supports_bypass: true,
                domain_mappings: vec![PowercapDomainMappingConfig { cluster_id: 0, opps: None }],
            },
            PowercapZoneNodeConfig {
                node_id: 3,
                parent_node_id: Some(1),
                zone_path: "gpc:0:0:2".to_string(),
                name: "big_and_big_mid".to_string(),
                constraint_name: Some("UW".to_string()),
                max_power_uw: Some(5471000),
                min_power_uw: Some(165000),
                power_levels_uw: Some(vec![5471000, 3798000, 3424000, 165000]),
                supports_bypass: true,
                domain_mappings: vec![
                    PowercapDomainMappingConfig { cluster_id: 1, opps: Some(vec![0, 1, 2, 5]) },
                    PowercapDomainMappingConfig { cluster_id: 2, opps: Some(vec![0, 2, 3, 6]) },
                ],
            },
            PowercapZoneNodeConfig {
                node_id: 4,
                parent_node_id: Some(3),
                zone_path: "gpc:0:0:2:0".to_string(),
                name: "cpufreq-cpu5".to_string(),
                constraint_name: Some("UW".to_string()),
                max_power_uw: Some(1853000),
                min_power_uw: Some(50000),
                power_levels_uw: Some(vec![1853000, 1561000, 1345000, 50000]),
                supports_bypass: true,
                domain_mappings: vec![PowercapDomainMappingConfig {
                    cluster_id: 1,
                    opps: Some(vec![0, 1, 2, 5]),
                }],
            },
        ]
    }

    #[fuchsia::test]
    async fn test_new_from_json() {
        let json_data = json::json!({
            "type": "PowercapController",
            "name": "powercap_controller",
            "config": {
                "nodes": [
                    {
                        "node_id": 0,
                        "zone_path": "gpc:0",
                        "name": "gpc"
                    },
                    {
                        "node_id": 1,
                        "parent_node_id": 0,
                        "zone_path": "gpc:0:0:0",
                        "name": "cpufreq-cpu0",
                        "constraint_name": "UW",
                        "max_power_uw": 999000,
                        "min_power_uw": 67000,
                        "power_levels_uw": [999000, 609000, 67000],
                        "supports_bypass": true,
                        "domain_mappings": [
                            {
                                "cluster_id": 0
                            }
                        ]
                    }
                ]
            }
        });

        let mut service_fs = ServiceFs::new_local();
        let node = PowercapControllerBuilder::new_from_json(json_data, &mut service_fs)
            .build()
            .expect("PowercapController build should succeed");
        assert_eq!(node.name(), "PowercapController");
    }

    #[fuchsia::test]
    async fn test_power_limits_and_watcher_updates() {
        let inspector = inspect::Inspector::default();
        let mut service_fs = ServiceFs::new_local();
        let controller = PowercapControllerBuilder::new(make_test_zones())
            .with_outgoing_svc_dir(service_fs.root_dir())
            .with_inspect_root(inspector.root())
            .build()
            .unwrap();
        controller.init().await.unwrap();

        let connector = service_fs.create_protocol_connector().unwrap();
        let _service_fs_task = fasync::Task::local(service_fs.collect::<()>());

        let client_proxy = connector.connect_to_protocol::<fpowercap::ClientMarker>().unwrap();
        let watcher_proxy =
            connector.connect_to_protocol::<fpowercap::ClientStateWatcherMarker>().unwrap();

        // Initial hanging-get returns unthrottled [0, 0, 0].
        let initial_opps = watcher_proxy.watch().await.unwrap();
        assert_eq!(initial_opps, vec![0, 0, 0]);

        let tree = client_proxy.get_powercap_tree().await.unwrap();
        assert_eq!(tree.len(), 5);
        assert_eq!(client_proxy.get_power_limit(2).await.unwrap().unwrap(), 999000);
        assert_eq!(client_proxy.get_power_limit(3).await.unwrap().unwrap(), 5471000);

        // Throttle big_and_big_mid (node 3) to 3798000 uW -> level 1 -> cluster 1 OPP 1, cluster 2 OPP 2.
        client_proxy.set_power_limit(3, 3798000).await.unwrap().unwrap();
        assert_eq!(client_proxy.get_power_limit(3).await.unwrap().unwrap(), 3798000);
        assert_eq!(watcher_proxy.watch().await.unwrap(), vec![0, 1, 2]);

        // Also throttle cpufreq-cpu5 (node 4, cluster 1) more aggressively to 1345000 uW -> level 2 (OPP 2).
        // Cluster 1 takes max(1, 2) = 2, while cluster 2 stays at 2.
        client_proxy.set_power_limit(4, 1345000).await.unwrap().unwrap();
        assert_eq!(watcher_proxy.watch().await.unwrap(), vec![0, 2, 2]);

        // Throttle cpufreq-cpu0 (node 2, cluster 0) to 500000 uW (between 609000 and 318000) -> snaps to 318000 (level 2, OPP 2).
        client_proxy.set_power_limit(2, 500000).await.unwrap().unwrap();
        assert_eq!(watcher_proxy.watch().await.unwrap(), vec![2, 2, 2]);

        // Budget below min_power_uw (165000 uW) on mapped zone 3 stores the written value and
        // snaps to the lowest-power OPP (level 3 -> cluster 1 OPP 5, cluster 2 OPP 6).
        client_proxy.set_power_limit(3, 100000).await.unwrap().unwrap();
        assert_eq!(client_proxy.get_power_limit(3).await.unwrap().unwrap(), 100000);
        assert_eq!(watcher_proxy.watch().await.unwrap(), vec![2, 5, 6]);

        // Constrained unmapped zone (node 1 "soc") stores the written power limit for readback.
        client_proxy.set_power_limit(1, 9000000).await.unwrap().unwrap();
        assert_eq!(client_proxy.get_power_limit(1).await.unwrap().unwrap(), 9000000);

        // Unconstrained container zone (node 0 "gpc") returns InvalidArguments.
        assert_eq!(
            client_proxy.get_power_limit(0).await.unwrap(),
            Err(fpowercap::PowercapError::InvalidArguments)
        );
        assert_eq!(
            client_proxy.set_power_limit(0, 1000).await.unwrap(),
            Err(fpowercap::PowercapError::InvalidArguments)
        );

        // Unknown node_id returns InvalidArguments.
        assert_eq!(
            client_proxy.get_power_limit(999).await.unwrap(),
            Err(fpowercap::PowercapError::InvalidArguments)
        );
        assert_eq!(
            client_proxy.set_power_limit(999, 1000).await.unwrap(),
            Err(fpowercap::PowercapError::InvalidArguments)
        );
    }

    #[fuchsia::test]
    fn test_config_validation() {
        let base_root = PowercapZoneNodeConfig {
            node_id: 0,
            parent_node_id: None,
            zone_path: "gpc:0".to_string(),
            name: "gpc".to_string(),
            constraint_name: None,
            max_power_uw: Some(1_000_000),
            min_power_uw: Some(100_000),
            power_levels_uw: Some(vec![1_000_000, 500_000, 100_000]),
            supports_bypass: true,
            domain_mappings: vec![PowercapDomainMappingConfig {
                cluster_id: 0,
                opps: Some(vec![0, 1, 2]),
            }],
        };

        let cases: Vec<(Vec<PowercapZoneNodeConfig>, &str)> = vec![
            (
                (0..=fpowercap::MAX_NODES as u64)
                    .map(|id| PowercapZoneNodeConfig {
                        node_id: id,
                        zone_path: format!("gpc:{id}"),
                        domain_mappings: vec![],
                        ..base_root.clone()
                    })
                    .collect(),
                "exceeding MAX_NODES (64)",
            ),
            (
                vec![PowercapZoneNodeConfig { zone_path: String::new(), ..base_root.clone() }],
                "empty zone_path",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    zone_path: "a".repeat(fpowercap::MAX_PATH_LEN as usize + 1),
                    ..base_root.clone()
                }],
                "zone_path length (129) exceeds MAX_PATH_LEN (128)",
            ),
            (
                vec![PowercapZoneNodeConfig { name: String::new(), ..base_root.clone() }],
                "empty name",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    name: "n".repeat(fpowercap::MAX_NAME_LEN as usize + 1),
                    ..base_root.clone()
                }],
                "name length (65) exceeds MAX_NAME_LEN (64)",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    constraint_name: Some(String::new()),
                    ..base_root.clone()
                }],
                "empty constraint_name",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    constraint_name: Some("c".repeat(fpowercap::MAX_NAME_LEN as usize + 1)),
                    ..base_root.clone()
                }],
                "constraint_name length (65) exceeds MAX_NAME_LEN (64)",
            ),
            (
                vec![
                    base_root.clone(),
                    PowercapZoneNodeConfig {
                        node_id: 0,
                        zone_path: "gpc:0:dup".to_string(),
                        ..base_root.clone()
                    },
                ],
                "Duplicate powercap node_id 0",
            ),
            (
                vec![
                    base_root.clone(),
                    PowercapZoneNodeConfig {
                        node_id: 1,
                        zone_path: "gpc:0".to_string(),
                        ..base_root.clone()
                    },
                ],
                "Duplicate powercap zone_path 'gpc:0'",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    min_power_uw: Some(2_000_000),
                    max_power_uw: Some(1_000_000),
                    ..base_root.clone()
                }],
                "min_power_uw (2000000) must be <= max_power_uw (1000000)",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    power_levels_uw: Some(
                        (1..=fpowercap::MAX_POWER_LEVELS as u64 + 1).rev().collect(),
                    ),
                    domain_mappings: vec![],
                    ..base_root.clone()
                }],
                "power_levels_uw length (65) exceeds MAX_POWER_LEVELS (64)",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    power_levels_uw: Some(vec![500_000, 1_000_000, 100_000]),
                    ..base_root.clone()
                }],
                "power_levels_uw must be ordered from highest to lowest",
            ),
            (
                vec![PowercapZoneNodeConfig { max_power_uw: Some(900_000), ..base_root.clone() }],
                "max_power_uw (900000) must equal power_levels_uw[0] (1000000)",
            ),
            (
                vec![PowercapZoneNodeConfig { min_power_uw: Some(200_000), ..base_root.clone() }],
                "min_power_uw (200000) must be <= lowest power_levels_uw (100000)",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    domain_mappings: vec![PowercapDomainMappingConfig {
                        cluster_id: fpowercap::MAX_DOMAINS as usize,
                        opps: None,
                    }],
                    ..base_root.clone()
                }],
                "domain_mapping cluster_id (32) must be < MAX_DOMAINS (32)",
            ),
            (
                vec![PowercapZoneNodeConfig { power_levels_uw: None, ..base_root.clone() }],
                "defines domain_mappings for cluster 0 but has no power_levels_uw",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    domain_mappings: vec![
                        PowercapDomainMappingConfig { cluster_id: 0, opps: None },
                        PowercapDomainMappingConfig { cluster_id: 0, opps: None },
                    ],
                    ..base_root.clone()
                }],
                "duplicate domain_mapping for cluster 0",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    domain_mappings: vec![PowercapDomainMappingConfig {
                        cluster_id: 0,
                        opps: Some(vec![0, 1]),
                    }],
                    ..base_root.clone()
                }],
                "opps length (2) must match power_levels_uw length (3)",
            ),
            (
                vec![PowercapZoneNodeConfig {
                    domain_mappings: vec![PowercapDomainMappingConfig {
                        cluster_id: 0,
                        opps: Some(vec![0, 3, 2]),
                    }],
                    ..base_root.clone()
                }],
                "opps must be non-decreasing",
            ),
            (
                vec![PowercapZoneNodeConfig { parent_node_id: Some(99), ..base_root.clone() }],
                "references unknown parent_node_id 99",
            ),
            (
                vec![PowercapZoneNodeConfig { parent_node_id: Some(0), ..base_root.clone() }],
                "cyclic parent_node_id chain involving 0",
            ),
            (
                vec![
                    PowercapZoneNodeConfig {
                        node_id: 0,
                        parent_node_id: Some(1),
                        zone_path: "gpc:0".to_string(),
                        ..base_root.clone()
                    },
                    PowercapZoneNodeConfig {
                        node_id: 1,
                        parent_node_id: Some(0),
                        zone_path: "gpc:1".to_string(),
                        ..base_root
                    },
                ],
                "cyclic parent_node_id chain",
            ),
        ];

        for (nodes, expected_err) in cases {
            let err = PowercapControllerBuilder::new(nodes)
                .build()
                .err()
                .unwrap_or_else(|| panic!("Expected error containing '{expected_err}'"));
            assert!(
                format!("{err:#}").contains(expected_err),
                "Expected '{expected_err}' in '{err:#}'"
            );
        }
    }

    #[fuchsia::test]
    fn test_bypass_timer_and_expiration() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let inspector = inspect::Inspector::default();
        let mut service_fs = ServiceFs::new_local();
        let controller = PowercapControllerBuilder::new(make_test_zones())
            .with_outgoing_svc_dir(service_fs.root_dir())
            .with_inspect_root(inspector.root())
            .build()
            .unwrap();
        let mut init_fut = controller.init();
        assert_matches!(exec.run_until_stalled(&mut init_fut), Poll::Ready(Ok(())));
        drop(init_fut);

        let connector = service_fs.create_protocol_connector().unwrap();
        let _service_fs_task = fasync::Task::local(service_fs.collect::<()>());

        let client_proxy = connector.connect_to_protocol::<fpowercap::ClientMarker>().unwrap();
        let watcher_proxy =
            connector.connect_to_protocol::<fpowercap::ClientStateWatcherMarker>().unwrap();

        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 0, 0]
        );

        // Throttle node 3 (big_and_big_mid) to 3424000 uW -> [0, 2, 3].
        let mut set_limit_fut = client_proxy.set_power_limit(3, 3424000);
        assert_matches!(exec.run_until_stalled(&mut set_limit_fut), Poll::Ready(Ok(Ok(()))));

        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 2, 3]
        );

        // Activate a 100ms bypass on node 3 -> immediately lifts cap back to [0, 0, 0].
        let mut set_bypass_fut = client_proxy.set_power_limit_bypass(3, 100);
        assert_matches!(exec.run_until_stalled(&mut set_bypass_fut), Poll::Ready(Ok(Ok(()))));

        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 0, 0]
        );

        // Advance time by 40ms -> remaining bypass should be 60ms, watcher still pending.
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(40_000_000));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(exec.run_until_stalled(&mut watch_fut), Poll::Pending);

        let mut get_bypass_fut = client_proxy.get_power_limit_bypass(3);
        assert_matches!(exec.run_until_stalled(&mut get_bypass_fut), Poll::Ready(Ok(Ok(60))));

        // Advance time past the 100ms deadline -> bypass expires and restores [0, 2, 3].
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(100_000_000));
        assert!(exec.wake_expired_timers());
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 2, 3]
        );

        let mut get_bypass_fut = client_proxy.get_power_limit_bypass(3);
        assert_matches!(exec.run_until_stalled(&mut get_bypass_fut), Poll::Ready(Ok(Ok(0))));

        // Re-arm a 50ms bypass at t=100ms, then replace it at t=120ms with a 100ms bypass
        // (new deadline t=220ms), and verify the earlier t=150ms deadline is superseded.
        let mut set_bypass_fut = client_proxy.set_power_limit_bypass(3, 50);
        assert_matches!(exec.run_until_stalled(&mut set_bypass_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 0, 0]
        );

        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(120_000_000));
        let mut set_bypass_fut = client_proxy.set_power_limit_bypass(3, 100);
        assert_matches!(exec.run_until_stalled(&mut set_bypass_fut), Poll::Ready(Ok(Ok(()))));

        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(150_000_000));
        // The earlier t=150ms timer wakes and runs `expire_bypass`, which ignores the stale
        // deadline because the newer t=220ms deadline is active on the zone.
        assert!(exec.wake_expired_timers());
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(exec.run_until_stalled(&mut watch_fut), Poll::Pending);
        let mut get_bypass_fut = client_proxy.get_power_limit_bypass(3);
        assert_matches!(exec.run_until_stalled(&mut get_bypass_fut), Poll::Ready(Ok(Ok(70))));

        // Explicitly cancel bypass early via set_power_limit_bypass(3, 0) -> immediately restores [0, 2, 3].
        let mut clear_bypass_fut = client_proxy.set_power_limit_bypass(3, 0);
        assert_matches!(exec.run_until_stalled(&mut clear_bypass_fut), Poll::Ready(Ok(Ok(()))));
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 2, 3]
        );
        let mut get_bypass_fut = client_proxy.get_power_limit_bypass(3);
        assert_matches!(exec.run_until_stalled(&mut get_bypass_fut), Poll::Ready(Ok(Ok(0))));

        // Verify per-zone bypass semantics across overlapping zones on cluster 1:
        // Node 3 (big_and_big_mid) is at 3424000 uW -> [0, 2, 3].
        // Throttle child node 4 (cpufreq-cpu5, cluster 1) to 50000 uW (level 3 -> OPP 5)
        // -> cluster 1 takes max(2, 5) = 5 -> [0, 5, 3].
        let mut set_limit_fut = client_proxy.set_power_limit(4, 50000);
        assert_matches!(exec.run_until_stalled(&mut set_limit_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 5, 3]
        );

        // Bypassing only node 4 lifts node 4's constraint (OPP 0), so cluster 1 falls back to
        // node 3's ceiling (max(2, 0) = 2) -> [0, 2, 3].
        let mut set_bypass_fut = client_proxy.set_power_limit_bypass(4, 100);
        assert_matches!(exec.run_until_stalled(&mut set_bypass_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 2, 3]
        );

        // Bypassing node 3 while node 4 is still bypassed lifts both -> [0, 0, 0].
        let mut set_bypass_fut = client_proxy.set_power_limit_bypass(3, 100);
        assert_matches!(exec.run_until_stalled(&mut set_bypass_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 0, 0]
        );

        // Canceling node 4's bypass while node 3 remains bypassed restores node 4's cap on
        // cluster 1 (OPP 5) while cluster 2 stays uncapped (OPP 0) -> [0, 5, 0].
        let mut clear_bypass_fut = client_proxy.set_power_limit_bypass(4, 0);
        assert_matches!(exec.run_until_stalled(&mut clear_bypass_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 5, 0]
        );

        // Restore node 4's power limit to max (1853000 uW) and cancel node 3's bypass -> [0, 2, 3].
        let mut set_limit_fut = client_proxy.set_power_limit(4, 1853000);
        assert_matches!(exec.run_until_stalled(&mut set_limit_fut), Poll::Ready(Ok(Ok(()))));
        let mut clear_bypass_fut = client_proxy.set_power_limit_bypass(3, 0);
        assert_matches!(exec.run_until_stalled(&mut clear_bypass_fut), Poll::Ready(Ok(Ok(()))));
        let mut watch_fut = watcher_proxy.watch();
        assert_matches!(
            exec.run_until_stalled(&mut watch_fut),
            Poll::Ready(Ok(opps)) if opps == vec![0, 2, 3]
        );

        // Node 0 ("gpc") does not support bypass -> returns InvalidArguments.
        let mut bad_bypass_fut = client_proxy.set_power_limit_bypass(0, 50);
        assert_matches!(
            exec.run_until_stalled(&mut bad_bypass_fut),
            Poll::Ready(Ok(Err(fpowercap::PowercapError::InvalidArguments)))
        );

        assert_data_tree!(
            @executor exec,
            inspector,
            root: {
                PowercapController: contains {
                    connect_count: 1u64,
                    domain_opps: vec![0u64, 2u64, 3u64],
                }
            }
        );
    }
}
