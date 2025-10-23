// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod battery_manager;
mod battery_simulator;
mod history_logger;
mod polisher;

use crate::battery_manager::{BatteryManager, BatterySimulationStateObserver};
use crate::battery_simulator::SimulatedBatteryInfoSource;
use crate::history_logger::{HistoryLogger, HistoryLoggerConfig};
use anyhow::{Context, Error};
use battery_manager_config::Config;
use fidl_fuchsia_power_battery::BatteryManagerRequestStream;
use fuchsia_component::client as fclient;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::{self as inspect};
use futures::prelude::*;
use inspect_runtime::PublishOptions;
use log::{error, info, warn};
use std::path::Path;
use std::sync::{Arc, Weak};
use {
    fidl_fuchsia_power_battery as fpower, fidl_fuchsia_power_battery_test as spower,
    fidl_fuchsia_power_system as fsystem, fuchsia_async as fasync,
};

enum IncomingService {
    BatteryManager(BatteryManagerRequestStream),
    BatterySimulator(spower::BatterySimulatorRequestStream),
}

const MAX_BATTERY_LEVEL_MEASUREMENTS: usize = 1440;

const CURR_BOOT_BATTERY_HISTORY_FILE: &str = "/data/history.txt";
const PREV_BOOT_BATTERY_HISTORY_FILE: &str = "/tmp/history.txt";

// Record up to 144 battery charge status changes.
//
// This value was picked assuming there will be 1 charge status change for every 10 battery
// level measurements and it's expected the charge status change history will cover more time than
// the battery level measurements.
const MAX_CHARGE_STATUS_MEASUREMENTS: usize = 144;

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

fn move_battery_history() {
    // The previous boot values being present means this isn't the first time the component has
    // started in this boot. The previous boot values are stored in /tmp and aren't persisted
    // across boots.
    if Path::new(PREV_BOOT_BATTERY_HISTORY_FILE).exists() {
        warn!("Not moving history, {} already exists", PREV_BOOT_BATTERY_HISTORY_FILE);
        return;
    }

    // Move content by reading then writing it for moves from /data to /tmp.
    let content = match std::fs::read_to_string(CURR_BOOT_BATTERY_HISTORY_FILE) {
        Ok(c) => c,
        Err(e) => {
            info!("Could not read current boot history, not moving: {}", e);
            return;
        }
    };

    if let Err(e) = std::fs::write(PREV_BOOT_BATTERY_HISTORY_FILE, &content) {
        warn!("Could not write previous boot history: {}", e);
        return;
    }

    if let Err(e) = std::fs::File::create(CURR_BOOT_BATTERY_HISTORY_FILE) {
        warn!("Could not clear current boot history: {}", e);
    }
}

#[fuchsia::main(logging_tags = ["battery_manager"])]
async fn main() -> Result<(), Error> {
    info!("starting up");

    let inspector = inspect::component::inspector();
    let _inspect_server_task = inspect_runtime::publish(inspector, PublishOptions::default());
    inspect::component::serve_inspect_stats();

    // Move the battery history files before the service starts to ensure they're in the locations
    // it expects.
    move_battery_history();

    let logger_config = HistoryLoggerConfig {
        curr_boot_path: CURR_BOOT_BATTERY_HISTORY_FILE.to_string(),
        prev_boot_path: PREV_BOOT_BATTERY_HISTORY_FILE.to_string(),
        battery_level_buffer_capacity: MAX_BATTERY_LEVEL_MEASUREMENTS,
        charge_status_buffer_capacity: MAX_CHARGE_STATUS_MEASUREMENTS,
    };

    let logger = HistoryLogger::from_file(inspector.root(), logger_config);
    let battery_manager = Arc::new(BatteryManager::new_with_logger(logger));
    let battery_manager_clone = battery_manager.clone();

    let config = Config::take_from_startup_handle();
    inspector.root().record_child("config", |config_node| config.record_inspect(config_node));
    log::info!(config:?; "config");

    fasync::Task::local(async move {
        let proxy = match get_battery_info_provider_proxy().await {
            Ok(p) => p,
            Err(e) => {
                error!("Error getting battery info provider: {e:?}");
                return; // Exit the task on error
            }
        };

        let sag = if config.suspend_enabled {
            Some(
                fuchsia_component::client::connect_to_protocol::<fsystem::ActivityGovernorMarker>()
                    .expect("should connect to system activity governor"),
            )
        } else {
            None
        };
        if let Err(e) = battery_manager_clone.start_watching_battery_info(proxy, sag).await {
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
