// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod arbiter;
mod battery_info_recorders;
mod battery_manager;
mod battery_simulator;
mod config;
mod governor_supervisor;
mod polisher;

use crate::arbiter::ChargeLimitArbiter;
use crate::battery_info_recorders::RecorderConfig;
use crate::battery_manager::{BatteryManager, BatterySimulationStateObserver, DriverUpdate};
use crate::battery_simulator::SimulatedBatteryInfoSource;
pub(crate) use crate::config::BatteryManagerConfig;
use crate::config::read_battery_manager_config;
use crate::governor_supervisor::spawn_configured_governors;
use anyhow::Error;
use battery_manager_config::Config;
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use fidl_fuchsia_power_system as fsystem;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect as inspect;
use futures::channel::mpsc;
use futures::prelude::*;
use inspect_runtime::PublishOptions;
use log::{error, info, warn};
use std::sync::{Arc, Weak};

pub(crate) enum BatteryInfoSource {
    New(fbattery::BatteryProxy),
    ModernService(fpower::BatteryInfoProviderProxy),
}

enum IncomingService {
    BatteryManager(fpower::BatteryManagerRequestStream),
    BatterySimulator(spower::BatterySimulatorRequestStream),
}

const CURR_BOOT_BATTERY_HISTORY_FILE: &str = "/data/history.txt";
// Paths to search for configuration: /config/config.json from BootFS (in production),
// falling back to /pkg/config/test_config.json (in integration tests).
const BOOTFS_CONFIG_PATH: &str = "/config/config.json";
const PKG_CONFIG_PATH: &str = "/pkg/config/test_config.json";
/// Initial delay before watching a battery driver again after its watch ended, or a charger whose
/// `Charger.Watch` failed while its `Controller` is still open.
const DRIVER_WATCH_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);
/// Cap on the retry and restart delays, which double after each failure.
const MAX_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(60);
const INSPECTOR_NODE_CHARGE_LIMIT_ARBITER: &str = "charge_limit_arbiter";

/// Capped exponential backoff between attempts of a task that is retried forever. An attempt
/// that ran for at least [`MAX_RETRY_DELAY`] counts as healthy and resets the delay.
pub(crate) struct Backoff {
    initial: zx::MonotonicDuration,
    next: zx::MonotonicDuration,
    attempt_start: fasync::MonotonicInstant,
}

impl Backoff {
    pub(crate) fn new(initial: zx::MonotonicDuration) -> Self {
        Self { initial, next: initial, attempt_start: fasync::MonotonicInstant::now() }
    }

    /// Sleeps for the current backoff duration and doubles the delay (up to [`MAX_RETRY_DELAY`])
    /// for the next call, resetting first if the previous attempt ran for at least
    /// [`MAX_RETRY_DELAY`].
    pub(crate) async fn wait(&mut self) {
        if fasync::MonotonicInstant::now() - self.attempt_start >= MAX_RETRY_DELAY {
            self.next = self.initial;
        }
        fasync::Timer::new(fasync::MonotonicInstant::after(self.next)).await;
        self.next = std::cmp::min(self.next * 2, MAX_RETRY_DELAY);
        self.attempt_start = fasync::MonotonicInstant::now();
    }
}

async fn get_battery_info_source() -> Result<BatteryInfoSource, Error> {
    info!("Looking for battery info service (new or old)...");

    let new_stream = match fclient::Service::open(fbattery::ServiceMarker) {
        Ok(s) => match s.watch().await {
            Ok(w) => Some(w),
            Err(e) => {
                warn!("Failed to watch new battery service: {:?}", e);
                None
            }
        },
        Err(e) => {
            warn!("Failed to open new battery service: {:?}", e);
            None
        }
    };

    let old_stream = match fclient::Service::open(fpower::InfoServiceMarker) {
        Ok(s) => match s.watch().await {
            Ok(w) => Some(w),
            Err(e) => {
                warn!("Failed to watch old battery service: {:?}", e);
                None
            }
        },
        Err(e) => {
            warn!("Failed to open old battery service: {:?}", e);
            None
        }
    };

    if new_stream.is_none() && old_stream.is_none() {
        return Err(anyhow::anyhow!(
            "Failed to initialize both battery service watchers. Check component manifest."
        ));
    }

    // Use futures::stream::iter to turn Option<impl Stream> into a Stream,
    // and flatten it to get a single stream of instances.
    // If the original stream was None, it becomes an empty stream that never yields.
    let mut new_stream = futures::stream::iter(new_stream).flatten().fuse();
    let mut old_stream = futures::stream::iter(old_stream).flatten().fuse();

    loop {
        futures::select! {
            instance_res = new_stream.select_next_some() => {
                match instance_res {
                    Ok(instance) => {
                        if let Ok(proxy) = instance.connect_to_battery() {
                            info!("Connected to new fuchsia.hardware.power.battery service");
                            return Ok(BatteryInfoSource::New(proxy));
                        }
                        warn!("Failed to connect to an instance of the new service, looking for next...");
                    }
                    Err(e) => warn!("New service stream error: {:?}", e),
                }
            }
            instance_res = old_stream.select_next_some() => {
                match instance_res {
                    Ok(instance) => {
                        if let Ok(proxy) = instance.connect_to_device() {
                            info!("Connected to fuchsia.power.battery service");
                            return Ok(BatteryInfoSource::ModernService(proxy));
                        }
                        warn!("Failed to connect to an instance of the old service, looking for next...");
                    }
                    Err(e) => warn!("Old service stream error: {:?}", e),
                }
            }
            complete => return Err(anyhow::anyhow!("All battery service streams closed")),
        }
    }
}

// TODO(b/523292405): Remove this function and the CURR_BOOT_BATTERY_HISTORY_FILE constant
// once we are confident that the legacy history file has been cleaned up from all user devices.
fn remove_battery_history() {
    // Remove legacy battery history file if it exists.
    let _ = std::fs::remove_file(CURR_BOOT_BATTERY_HISTORY_FILE);
}

fn load_battery_manager_config_from_path(path: &str) -> Result<BatteryManagerConfig, Error> {
    info!("Loading battery manager config from {path}");
    read_battery_manager_config(path)
}

fn load_battery_manager_config() -> Result<BatteryManagerConfig, Error> {
    if std::path::Path::new(BOOTFS_CONFIG_PATH).exists() {
        load_battery_manager_config_from_path(BOOTFS_CONFIG_PATH)
    } else if std::path::Path::new(PKG_CONFIG_PATH).exists() {
        load_battery_manager_config_from_path(PKG_CONFIG_PATH)
    } else {
        info!("No config file found in bootfs or package, using default config");
        Ok(BatteryManagerConfig::default())
    }
}

/// Watches the battery driver, and connects to it again whenever the watch ends, after a delay
/// that doubles from [`DRIVER_WATCH_RETRY_DELAY`] up to [`MAX_RETRY_DELAY`]. While no battery
/// service instance is available, waits for one to appear.
async fn start_watching_battery_service(
    battery_manager: Arc<BatteryManager>,
    sag: Option<fsystem::ActivityGovernorProxy>,
    updates: mpsc::UnboundedSender<DriverUpdate>,
) {
    let mut backoff = Backoff::new(DRIVER_WATCH_RETRY_DELAY);
    loop {
        let source = match get_battery_info_source().await {
            Ok(s) => s,
            Err(e) => {
                error!("Error getting battery info source: {e:?}");
                return;
            }
        };

        match battery_manager
            .start_watching_battery_info(source, sag.clone(), updates.clone())
            .await
        {
            Ok(()) => warn!("Battery info watch ended; reconnecting"),
            Err(e) => error!("Error when watching battery info, reconnecting: {e:?}"),
        }
        backoff.wait().await;
    }
}

/// Watches `fuchsia.hardware.power.charger.Service` instances, connects the arbiter to each
/// instance's `Controller`, and retries `Charger.Watch` with exponential backoff as long as the
/// `Controller` channel stays open.
async fn start_watching_charger_service(
    battery_manager: Arc<BatteryManager>,
    arbiter: Arc<ChargeLimitArbiter>,
    sag: Option<fsystem::ActivityGovernorProxy>,
    updates: mpsc::UnboundedSender<DriverUpdate>,
) {
    info!("Looking for charger service...");
    let service = match fclient::Service::open(fcharger::ServiceMarker) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to open charger service: {:?}", e);
            return;
        }
    };
    let stream = match service.watch().await {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to watch charger service: {:?}", e);
            return;
        }
    };

    let mut stream = stream.fuse();
    while let Some(instance_res) = stream.next().await {
        match instance_res {
            Ok(instance) => {
                let controller = match instance.connect_to_controller() {
                    Ok(controller) => {
                        info!("Connected to fuchsia.hardware.power.charger Controller");
                        arbiter.set_driver_controller(Some(controller.clone())).await;
                        Some(controller)
                    }
                    Err(e) => {
                        warn!("Failed to connect to charger Controller: {:?}", e);
                        None
                    }
                };
                // The votes stay bound for as long as the driver keeps the Controller open, so a
                // telemetry failure is retried rather than relaxing every vote.
                let mut backoff = Backoff::new(DRIVER_WATCH_RETRY_DELAY);
                loop {
                    match instance.connect_to_charger() {
                        Ok(proxy) => {
                            info!("Connected to fuchsia.hardware.power.charger service");
                            if let Err(e) = battery_manager
                                .start_watching_charger_info(proxy, sag.clone(), updates.clone())
                                .await
                            {
                                warn!("Charger watcher stream ended: {:?}", e);
                            }
                        }
                        Err(e) => warn!("Failed to connect to charger instance: {:?}", e),
                    }
                    if controller.as_ref().is_none_or(|c| c.is_closed()) {
                        break;
                    }
                    backoff.wait().await;
                }
                if controller.is_some() {
                    arbiter.set_driver_controller(None).await;
                }
            }
            Err(e) => warn!("Charger service watch error: {:?}", e),
        }
    }
}

#[fuchsia::main(logging_tags = ["battery_manager"])]
async fn main() -> Result<(), Error> {
    info!("starting up");

    let inspector = inspect::component::inspector();
    let _inspect_server_task = inspect_runtime::publish(inspector, PublishOptions::default());
    inspect::component::serve_inspect_stats();

    let config = Config::take_from_startup_handle();
    inspector.root().record_child("config", |config_node| config.record_inspect(config_node));
    log::info!(config:?; "config");

    // Remove the legacy battery history file before the service starts.
    remove_battery_history();

    let battery_manager_config = load_battery_manager_config()?;
    info!("Loaded battery manager config: {:?}", battery_manager_config);
    inspector
        .root()
        .record_child("battery_manager_config", |node| battery_manager_config.record_inspect(node));

    let recorder_config = RecorderConfig::default();
    let battery_manager = Arc::new(BatteryManager::new_with_battery_manager_config(
        recorder_config,
        battery_manager_config.clone(),
    ));

    let sag = if config.suspend_enabled {
        Some(
            fuchsia_component::client::connect_to_protocol::<fsystem::ActivityGovernorMarker>()
                .expect("should connect to system activity governor"),
        )
    } else {
        None
    };

    // Driver watch loops run as independent tasks but share the manager's cached state, so
    // they publish events here and a single consumer applies them in order.
    let (update_tx, update_rx) = mpsc::unbounded();

    let arbiter = Arc::new(ChargeLimitArbiter::new());
    let arbiter_for_inspect = Arc::downgrade(&arbiter);
    inspector.root().record_lazy_child(INSPECTOR_NODE_CHARGE_LIMIT_ARBITER, move || {
        let arbiter = arbiter_for_inspect.clone();
        async move {
            let inspector = inspect::Inspector::default();
            if let Some(arbiter) = arbiter.upgrade() {
                arbiter.record_inspect(inspector.root()).await;
            }
            Ok(inspector)
        }
        .boxed()
    });

    let battery_manager_for_updates = battery_manager.clone();
    let sag_updates = sag.clone();
    let _update_task = fasync::Task::local(async move {
        battery_manager_for_updates.run_update_consumer(update_rx, sag_updates).await;
    });
    let _battery_task = fasync::Task::local(start_watching_battery_service(
        battery_manager.clone(),
        sag.clone(),
        update_tx.clone(),
    ));
    let _charger_task = fasync::Task::local(start_watching_charger_service(
        battery_manager.clone(),
        arbiter.clone(),
        sag,
        update_tx,
    ));

    let battery_simulator = Arc::new(SimulatedBatteryInfoSource::new(
        battery_manager.get_battery_info_copy(),
        Arc::downgrade(&battery_manager) as Weak<dyn BatterySimulationStateObserver>,
    ));

    let mut fs = ServiceFs::new();
    fs.dir("svc")
        .add_fidl_service(IncomingService::BatteryManager)
        .add_fidl_service(IncomingService::BatterySimulator);

    fs.take_and_serve_directory_handle()?;

    let _governor_scope = spawn_configured_governors(
        &battery_manager_config.governor_urls,
        &arbiter,
        inspector.root(),
    );

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::task::Poll;

    /// Verifies that `Backoff` doubles the wait duration up to `MAX_RETRY_DELAY` across rapid
    /// failures, and resets to the initial delay after an attempt runs for at least
    /// `MAX_RETRY_DELAY`.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_backoff_doubles_caps_and_resets_after_healthy_run() {
        let initial = zx::MonotonicDuration::from_seconds(20);
        let mut backoff = Backoff::new(initial);

        // First rapid failure waits `initial` (20s), then doubles to 40s.
        {
            let mut fut = pin!(backoff.wait());
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Pending);
            fasync::TestExecutor::advance_to(
                fasync::MonotonicInstant::now() + zx::MonotonicDuration::from_seconds(20),
            )
            .await;
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Ready(()));
        }

        // Second rapid failure waits 40s, then caps at `MAX_RETRY_DELAY` (60s).
        {
            let mut fut = pin!(backoff.wait());
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Pending);
            fasync::TestExecutor::advance_to(
                fasync::MonotonicInstant::now() + zx::MonotonicDuration::from_seconds(39),
            )
            .await;
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Pending);
            fasync::TestExecutor::advance_to(
                fasync::MonotonicInstant::now() + zx::MonotonicDuration::from_seconds(1),
            )
            .await;
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Ready(()));
        }

        // Simulate an attempt that ran healthily for `MAX_RETRY_DELAY` (60s) before failing;
        // `wait()` should reset to `initial` (20s).
        fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + MAX_RETRY_DELAY).await;
        {
            let mut fut = pin!(backoff.wait());
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Pending);
            fasync::TestExecutor::advance_to(fasync::MonotonicInstant::now() + initial).await;
            assert_eq!(fasync::TestExecutor::poll_until_stalled(&mut fut).await, Poll::Ready(()));
        }
    }
}
