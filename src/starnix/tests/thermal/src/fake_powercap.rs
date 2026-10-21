// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_power_powercap as fpowercap;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::LocalComponentHandles;
use fuchsia_sync::Mutex;
use futures::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Sentinel write value used in tests to trigger `PowercapError::Internal` -> `EIO`.
pub const TRIGGER_INTERNAL_ERROR_VALUE: u64 = u64::MAX;

#[derive(Clone, Debug)]
struct ZoneState {
    info: fpowercap::PowercapNodeInfo,
    power_limit_uw: u64,
    remaining_bypass_ms: u64,
}

#[derive(Default)]
pub struct FakePowercapState {
    zones: HashMap<u64, ZoneState>,
}

impl FakePowercapState {
    pub fn new_default() -> Arc<Mutex<Self>> {
        let mut zones = HashMap::new();

        // Root aggregator zone: `gpc:0`
        zones.insert(
            0,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(0),
                    parent_node_id: None,
                    zone_path: Some("gpc:0".to_string()),
                    name: Some("gpc".to_string()),
                    supports_bypass: Some(false),
                    ..Default::default()
                },
                power_limit_uw: 0,
                remaining_bypass_ms: 0,
            },
        );

        // Intermediate aggregator zone: `gpc:0:0` ("TestZone")
        zones.insert(
            1,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(1),
                    parent_node_id: Some(0),
                    zone_path: Some("gpc:0:0".to_string()),
                    name: Some("TestZone".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(15_000_000),
                    min_power_uw: Some(1_150_000),
                    supports_bypass: Some(false),
                    ..Default::default()
                },
                power_limit_uw: 15_000_000,
                remaining_bypass_ms: 0,
            },
        );

        // Leaf zones under `gpc:0:0`: `LITTLE`, `MID`, `BIG`, `GPU`
        zones.insert(
            2,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(2),
                    parent_node_id: Some(1),
                    zone_path: Some("gpc:0:0:0".to_string()),
                    name: Some("LITTLE".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(1_438_000),
                    min_power_uw: Some(150_000),
                    power_levels_uw: Some(vec![1_438_000, 1_100_000, 800_000, 500_000, 150_000]),
                    supports_bypass: Some(true),
                    ..Default::default()
                },
                power_limit_uw: 1_438_000,
                remaining_bypass_ms: 0,
            },
        );

        zones.insert(
            3,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(3),
                    parent_node_id: Some(1),
                    zone_path: Some("gpc:0:0:1".to_string()),
                    name: Some("MID".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(5_612_000),
                    min_power_uw: Some(300_000),
                    power_levels_uw: Some(vec![
                        5_612_000, 4_200_000, 2_800_000, 1_400_000, 300_000,
                    ]),
                    supports_bypass: Some(true),
                    ..Default::default()
                },
                power_limit_uw: 5_612_000,
                remaining_bypass_ms: 0,
            },
        );

        zones.insert(
            4,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(4),
                    parent_node_id: Some(1),
                    zone_path: Some("gpc:0:0:2".to_string()),
                    name: Some("BIG".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(4_565_000),
                    min_power_uw: Some(400_000),
                    power_levels_uw: Some(vec![
                        4_565_000, 3_500_000, 2_200_000, 1_100_000, 400_000,
                    ]),
                    supports_bypass: Some(true),
                    ..Default::default()
                },
                power_limit_uw: 4_565_000,
                remaining_bypass_ms: 0,
            },
        );

        zones.insert(
            5,
            ZoneState {
                info: fpowercap::PowercapNodeInfo {
                    node_id: Some(5),
                    parent_node_id: Some(1),
                    zone_path: Some("gpc:0:0:3".to_string()),
                    name: Some("GPU".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(6_900_000),
                    min_power_uw: Some(300_000),
                    power_levels_uw: Some(vec![
                        6_900_000, 5_000_000, 3_200_000, 1_500_000, 300_000,
                    ]),
                    supports_bypass: Some(true),
                    ..Default::default()
                },
                power_limit_uw: 6_900_000,
                remaining_bypass_ms: 0,
            },
        );

        Arc::new(Mutex::new(Self { zones }))
    }
}

pub async fn mock_powercap_service(
    handles: LocalComponentHandles,
    state: Arc<Mutex<FakePowercapState>>,
) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: fpowercap::ClientRequestStream| stream);
    fs.serve_connection(handles.outgoing_dir)?;

    fs.for_each_concurrent(None, |stream: fpowercap::ClientRequestStream| {
        let state = state.clone();
        async move {
            stream
                .try_for_each(|request| {
                    let state = state.clone();
                    async move {
                        match request {
                            fpowercap::ClientRequest::GetPowercapTree { responder } => {
                                // Deliberately return a leaf node before its parent to verify that
                                // Starnix topologically resolves parent-child relationships, and
                                // include malformed/duplicate/orphaned/cyclic entries to verify
                                // that invalid nodes are safely skipped without panicking.
                                let guard = state.lock();
                                let order = [4, 2, 1, 0, 3, 5];
                                let mut nodes: Vec<_> = order
                                    .iter()
                                    .filter_map(|id| guard.zones.get(id).map(|z| z.info.clone()))
                                    .collect();
                                nodes.extend([
                                    // Missing node_id.
                                    fpowercap::PowercapNodeInfo {
                                        node_id: None,
                                        zone_path: Some("gpc:missing-id".to_string()),
                                        name: Some("missing-id".to_string()),
                                        ..Default::default()
                                    },
                                    // Invalid zone_path values ("", "/", ".", "..", attribute collision).
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(80),
                                        zone_path: Some(String::new()),
                                        name: Some("empty-path".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(81),
                                        zone_path: Some("bad/path".to_string()),
                                        name: Some("slash-path".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(82),
                                        parent_node_id: Some(0),
                                        zone_path: Some(".".to_string()),
                                        name: Some("dot-path".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(83),
                                        parent_node_id: Some(0),
                                        zone_path: Some("..".to_string()),
                                        name: Some("dotdot-path".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(84),
                                        parent_node_id: Some(0),
                                        zone_path: Some("name".to_string()),
                                        name: Some("attr-collision".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(85),
                                        parent_node_id: Some(0),
                                        zone_path: Some("uevent".to_string()),
                                        name: Some("uevent-collision".to_string()),
                                        ..Default::default()
                                    },
                                    // Missing name.
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(86),
                                        zone_path: Some("gpc:missing-name".to_string()),
                                        name: None,
                                        ..Default::default()
                                    },
                                    // Duplicate node_id (0).
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(0),
                                        zone_path: Some("gpc:dup-id".to_string()),
                                        name: Some("dup-id".to_string()),
                                        ..Default::default()
                                    },
                                    // Duplicate zone_path ("gpc:0") with a distinct node_id.
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(87),
                                        zone_path: Some("gpc:0".to_string()),
                                        name: Some("dup-path".to_string()),
                                        ..Default::default()
                                    },
                                    // Orphaned parent_node_id (999).
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(88),
                                        parent_node_id: Some(999),
                                        zone_path: Some("gpc:orphan".to_string()),
                                        name: Some("orphan".to_string()),
                                        ..Default::default()
                                    },
                                    // Cyclic parent_node_id chain (89 <-> 90).
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(89),
                                        parent_node_id: Some(90),
                                        zone_path: Some("gpc:cycle-a".to_string()),
                                        name: Some("cycle-a".to_string()),
                                        ..Default::default()
                                    },
                                    fpowercap::PowercapNodeInfo {
                                        node_id: Some(90),
                                        parent_node_id: Some(89),
                                        zone_path: Some("gpc:cycle-b".to_string()),
                                        name: Some("cycle-b".to_string()),
                                        ..Default::default()
                                    },
                                ]);
                                responder.send(&nodes)?;
                            }
                            fpowercap::ClientRequest::GetPowerLimit { node_id, responder } => {
                                // Root aggregator zone `gpc:0` (node_id 0) has no hardware power
                                // limit and returns `PowercapError::Internal` to exercise EIO mapping.
                                if node_id == 0 {
                                    responder.send(Err(fpowercap::PowercapError::Internal))?;
                                    return Ok(());
                                }
                                let guard = state.lock();
                                match guard.zones.get(&node_id) {
                                    Some(zone) => responder.send(Ok(zone.power_limit_uw))?,
                                    None => responder
                                        .send(Err(fpowercap::PowercapError::InvalidArguments))?,
                                }
                            }
                            fpowercap::ClientRequest::SetPowerLimit {
                                node_id,
                                power_limit_uw,
                                responder,
                            } => {
                                if power_limit_uw == TRIGGER_INTERNAL_ERROR_VALUE {
                                    responder.send(Err(fpowercap::PowercapError::Internal))?;
                                    return Ok(());
                                }
                                let mut guard = state.lock();
                                match guard.zones.get_mut(&node_id) {
                                    Some(zone) => {
                                        zone.power_limit_uw = power_limit_uw;
                                        responder.send(Ok(()))?;
                                    }
                                    None => responder
                                        .send(Err(fpowercap::PowercapError::InvalidArguments))?,
                                }
                            }
                            fpowercap::ClientRequest::GetPowerLimitBypass {
                                node_id,
                                responder,
                            } => {
                                let guard = state.lock();
                                match guard.zones.get(&node_id) {
                                    Some(zone) if zone.info.supports_bypass.unwrap_or(false) => {
                                        responder.send(Ok(zone.remaining_bypass_ms))?
                                    }
                                    _ => responder
                                        .send(Err(fpowercap::PowercapError::InvalidArguments))?,
                                }
                            }
                            fpowercap::ClientRequest::SetPowerLimitBypass {
                                node_id,
                                bypass_time_ms,
                                responder,
                            } => {
                                if bypass_time_ms == TRIGGER_INTERNAL_ERROR_VALUE {
                                    responder.send(Err(fpowercap::PowercapError::Internal))?;
                                    return Ok(());
                                }
                                let mut guard = state.lock();
                                match guard.zones.get_mut(&node_id) {
                                    Some(zone) if zone.info.supports_bypass.unwrap_or(false) => {
                                        zone.remaining_bypass_ms = bypass_time_ms;
                                        responder.send(Ok(()))?;
                                    }
                                    _ => responder
                                        .send(Err(fpowercap::PowercapError::InvalidArguments))?,
                                }
                            }
                            fpowercap::ClientRequest::_UnknownMethod { .. } => {}
                        }
                        Ok(())
                    }
                })
                .await
                .unwrap()
        }
    })
    .await;

    Ok(())
}
