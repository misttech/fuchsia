// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Mock ODPM FIDL service implementation for Starnix integration tests.

use anyhow::Error;
use fidl_fuchsia_hardware_google_odpm as fodpm;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt as _, TryStreamExt as _};

const NANOS_PER_MILLI: i64 = 1_000_000;
const SAMPLE_TIME_MS: i64 = 1_279_315;
const RAIL0_ENERGY_UJ: i64 = 51_899_976;
const RAIL1_ENERGY_UJ: i64 = 185_487_825;

/// Converts milliseconds to nanoseconds.
fn millis_to_nanos(millis: i64) -> i64 {
    millis * NANOS_PER_MILLI
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service_instance("default", |request: fodpm::ServiceRequest| {
        let fodpm::ServiceRequest::Device(stream) = request;
        stream
    });
    service_fs.take_and_serve_directory_handle()?;

    service_fs
        .for_each_concurrent(None, |stream: fodpm::OdpmDeviceRequestStream| async move {
            let _ = stream
                .try_for_each(|request: fodpm::OdpmDeviceRequest| async move {
                    match request {
                        fodpm::OdpmDeviceRequest::GetRailMetadata { responder } => {
                            let metadata = [
                                fodpm::RailMetadata {
                                    name: Some("amb".to_string()),
                                    channel_id: Some(0),
                                    schematic_name: Some("S1M_VDD_AMB".to_string()),
                                    ..Default::default()
                                },
                                fodpm::RailMetadata {
                                    name: Some("cpu2".to_string()),
                                    channel_id: Some(1),
                                    schematic_name: Some("S2M_VDD_CPU2".to_string()),
                                    ..Default::default()
                                },
                            ];
                            responder.send(Ok(&metadata))?;
                        }
                        fodpm::OdpmDeviceRequest::GetEnergyJoules { payload, responder } => {
                            assert_eq!(
                                payload.measurement_type,
                                Some(fodpm::MeasurementType::Cumulative)
                            );
                            let readings = [
                                fodpm::EnergyReading {
                                    channel_id: Some(0),
                                    energy_uj: Some(RAIL0_ENERGY_UJ),
                                    interval: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    timestamp: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    ..Default::default()
                                },
                                fodpm::EnergyReading {
                                    channel_id: Some(1),
                                    energy_uj: Some(RAIL1_ENERGY_UJ),
                                    interval: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    timestamp: Some(millis_to_nanos(SAMPLE_TIME_MS)),
                                    ..Default::default()
                                },
                            ];
                            responder.send(Ok(&readings))?;
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
