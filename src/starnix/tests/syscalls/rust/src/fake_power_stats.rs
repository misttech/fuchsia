// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake power stats provider for the Starnix power residency syscall test.

use anyhow::Error;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt as _, TryStreamExt as _};

/// Residency reported for each entity.
fn reports() -> Vec<fpowerstats::Stats> {
    vec![
        fpowerstats::Stats {
            entity: "lpcm_gmc0".to_string(),
            state_residency_data: vec![
                fpowerstats::StateResidency {
                    id: 0,
                    name: "0".to_string(),
                    entry_count: 11,
                    total_time_ms: 2200,
                    last_entry_timestamp_ms: 0,
                },
                fpowerstats::StateResidency {
                    id: 1,
                    name: "1".to_string(),
                    entry_count: 4,
                    total_time_ms: 7,
                    last_entry_timestamp_ms: 0,
                },
            ],
        },
        fpowerstats::Stats {
            entity: "soc_power_state".to_string(),
            state_residency_data: vec![
                fpowerstats::StateResidency {
                    id: 0,
                    name: "ACTIVE".to_string(),
                    entry_count: 2,
                    total_time_ms: 100,
                    last_entry_timestamp_ms: 1,
                },
                fpowerstats::StateResidency {
                    id: 1,
                    name: "IDLE".to_string(),
                    entry_count: 0,
                    total_time_ms: 0,
                    last_entry_timestamp_ms: 0,
                },
            ],
        },
        // Neither of these is served: one is not a residency entity, and the other's name is not
        // safe to use as a file name.
        fpowerstats::Stats { entity: "other".to_string(), state_residency_data: vec![] },
        fpowerstats::Stats { entity: "lpcm_a-b".to_string(), state_residency_data: vec![] },
    ]
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service_instance(
        "default",
        |request: fpowerstats::ServiceRequest| {
            let fpowerstats::ServiceRequest::Device(stream) = request;
            stream
        },
    );
    service_fs.take_and_serve_directory_handle()?;

    service_fs
        .for_each_concurrent(None, |stream: fpowerstats::DeviceRequestStream| async move {
            let _ = stream
                .try_for_each(|request| async move {
                    match request {
                        fpowerstats::DeviceRequest::GetEntities { responder } => {
                            let entities: Vec<_> = reports()
                                .into_iter()
                                .map(|stats| fpowerstats::Entity {
                                    name: stats.entity,
                                    states: stats
                                        .state_residency_data
                                        .into_iter()
                                        .map(|state| fpowerstats::State {
                                            id: state.id,
                                            name: state.name,
                                        })
                                        .collect(),
                                })
                                .collect();
                            responder.send(&entities)?;
                        }
                        fpowerstats::DeviceRequest::GetStats { responder } => {
                            responder.send(&reports())?;
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await;
        })
        .await;

    Ok(())
}
