// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod battery_manager;
mod battery_simulator;

use crate::battery_manager::{BatteryManager, BatterySimulationStateObserver};
use crate::battery_simulator::SimulatedBatteryInfoSource;
use anyhow::{Context, Error};
use fidl_fuchsia_power_battery::BatteryManagerRequestStream;
use fuchsia_component::client as fclient;
use fuchsia_component::server::ServiceFs;
use futures::prelude::*;
use log::{error, info};
use std::sync::{Arc, Weak};
use {
    fidl_fuchsia_power_battery as fpower, fidl_fuchsia_power_battery_test as spower,
    fuchsia_async as fasync,
};

enum IncomingService {
    BatteryManager(BatteryManagerRequestStream),
    BatterySimulator(spower::BatterySimulatorRequestStream),
}

async fn get_battery_info_provider_proxy() -> Result<fpower::BatteryInfoProviderProxy, Error> {
    let device = fclient::Service::open(fpower::InfoServiceMarker)
        .context("Failed to open service")?
        .watch_for_any()
        .await
        .context("Failed to find instance")?
        .connect_to_device()
        .context("Failed to connect to device protocol")?;
    return Ok(device);
}

#[fuchsia::main(logging_tags = ["battery_manager"])]
async fn main() -> Result<(), Error> {
    info!("starting up");

    let battery_manager = Arc::new(BatteryManager::new());
    let battery_manager_clone = battery_manager.clone();

    fasync::Task::local(async move {
        let proxy = match get_battery_info_provider_proxy().await {
            Ok(p) => p,
            Err(e) => {
                error!("Error getting battery info provider: {e:?}");
                return; // Exit the task on error
            }
        };

        if let Err(e) = battery_manager_clone.start_watching_battery_info(proxy).await {
            error!("Error when watching battery info: {e:?}");
        }
    })
    .detach();

    let battery_simulator = Arc::new(SimulatedBatteryInfoSource::new(
        battery_manager.get_battery_info_copy(),
        Arc::downgrade(&battery_manager) as Weak<dyn BatterySimulationStateObserver>,
    ));

    let mut fs = ServiceFs::new();
    fs.dir("svc")
        .add_fidl_service(IncomingService::BatteryManager)
        .add_fidl_service(IncomingService::BatterySimulator);

    fs.take_and_serve_directory_handle()?;

    fs.for_each_concurrent(None, |request| {
        let battery_manager = battery_manager.clone();
        let battery_simulator = battery_simulator.clone();

        async move {
            match request {
                IncomingService::BatteryManager(stream) => {
                    let res = battery_manager.serve(stream).await;
                    if let Err(e) = res {
                        error!("BatteryManager failed {}", e);
                    }
                }
                IncomingService::BatterySimulator(stream) => {
                    let res = stream
                        .err_into()
                        .try_for_each_concurrent(None, |request| {
                            let battery_simulator = battery_simulator.clone();
                            let battery_manager = battery_manager.clone();
                            async move {
                                match request {
                                    spower::BatterySimulatorRequest::DisconnectRealBattery {
                                        ..
                                    } => {
                                        battery_simulator
                                            .update_simulation(
                                                true,
                                                battery_manager.get_battery_info_copy(),
                                            )
                                            .await?;
                                    }
                                    spower::BatterySimulatorRequest::ReconnectRealBattery {
                                        ..
                                    } => {
                                        battery_simulator
                                            .update_simulation(
                                                false,
                                                battery_manager.get_battery_info_copy(),
                                            )
                                            .await?;
                                    }
                                    spower::BatterySimulatorRequest::IsSimulating {
                                        responder,
                                        ..
                                    } => {
                                        let info = battery_manager.is_simulating();
                                        responder.send(info)?;
                                    }
                                    _ => {
                                        battery_simulator.handle_request(request).await?;
                                    }
                                }
                                Ok::<(), Error>(())
                            }
                        })
                        .await;

                    if let Err(e) = res {
                        error!("BatterySimulator failed {}", e);
                    }
                }
            }
        }
    })
    .await;

    info!("stopping battery_manager");
    Ok(())
}
