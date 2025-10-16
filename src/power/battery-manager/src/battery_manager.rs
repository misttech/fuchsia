// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::history_logger::HistoryLogger;
use anyhow::Error;
use fidl::HandleBased;
use fidl::endpoints::Proxy;
use fuchsia_sync::Mutex as SMutex;
use futures::lock::Mutex;
use futures::{StreamExt, TryStreamExt};
use log::{debug, error};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use {fidl_fuchsia_power_battery as fpower, fuchsia_async as fasync};

// Record up to 12 hours of battery level data in 30 second intervals.
//
// Note, more than 12 hours of wall clock time may be covered because sampling is done on the
// monotonic clock, which pauses during suspension, but timestamps are from the boot clock.
const BATTERY_LEVEL_PUBLISH_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) trait BatterySimulationStateObserver {
    fn update_simulation(&self, new_state: bool);
    fn update_simulated_battery_info(&self, battery_info: fpower::BatteryInfo);
}

impl BatterySimulationStateObserver for BatteryManager {
    fn update_simulation(&self, is_simulating: bool) {
        let mut sim_state = self.simulation_state.write().unwrap();
        *sim_state = is_simulating;
        drop(sim_state);
        if !is_simulating {
            self.update_watchers(None);
        }
    }
    fn update_simulated_battery_info(&self, battery_info: fpower::BatteryInfo) {
        let mut simulated_battery_info = self.simulated_battery_info.write().unwrap();
        *simulated_battery_info = battery_info;
        drop(simulated_battery_info);
        self.update_watchers_conditionally(true, None);
    }
}

/// Core component for the battery manager system.
///
/// BatteryManager maintains the current state info for the battery system
/// as well as the watchers that share this information with subscribed clients.
///
/// simulation_state: true when the simulator is running
pub struct BatteryManager {
    battery_info: Arc<RwLock<fpower::BatteryInfo>>,
    watchers: Arc<Mutex<Vec<fpower::BatteryInfoWatcherProxy>>>,
    simulation_state: RwLock<bool>,
    simulated_battery_info: RwLock<fpower::BatteryInfo>,

    /// Publishes battery events to Inspect.
    history_logger: Arc<SMutex<HistoryLogger>>,
}

#[inline]
fn get_current_time() -> i64 {
    zx::BootInstant::get().into_nanos()
}

impl BatteryManager {
    pub fn new_with_logger(logger: HistoryLogger) -> BatteryManager {
        BatteryManager {
            battery_info: Arc::new(RwLock::new(fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::NotAvailable),
                charge_status: Some(fpower::ChargeStatus::Unknown),
                charge_source: Some(fpower::ChargeSource::Unknown),
                level_percent: None,
                level_status: Some(fpower::LevelStatus::Unknown),
                health: Some(fpower::HealthStatus::Unknown),
                time_remaining: Some(fpower::TimeRemaining::Indeterminate(0)),
                timestamp: Some(get_current_time()),
                ..Default::default()
            })),
            watchers: Arc::new(Mutex::new(Vec::new())),
            simulation_state: RwLock::new(false),
            simulated_battery_info: RwLock::new(fpower::BatteryInfo {
                status: Some(fpower::BatteryStatus::NotAvailable),
                charge_status: Some(fpower::ChargeStatus::Unknown),
                charge_source: Some(fpower::ChargeSource::Unknown),
                level_percent: None,
                level_status: Some(fpower::LevelStatus::Unknown),
                health: Some(fpower::HealthStatus::Unknown),
                time_remaining: Some(fpower::TimeRemaining::Indeterminate(0)),
                timestamp: Some(get_current_time()),
                ..Default::default()
            }),
            history_logger: Arc::new(SMutex::new(logger)),
        }
    }

    // Adds watcher
    pub async fn add_watcher(&self, watcher: fpower::BatteryInfoWatcherProxy) {
        let mut watchers = self.watchers.lock().await;
        debug!("::manager:: adding watcher: {:?} [{:?}]", watcher, watchers.len());
        watchers.push(watcher)
    }

    // Call update_watchers if expecting_simulating == simulation_state.
    // This behavior avoids unnecessary updates.
    fn update_watchers_conditionally(
        &self,
        expect_simulating: bool,
        wake_lease: Option<zx::EventPair>,
    ) {
        if self.is_simulating() == expect_simulating {
            self.update_watchers(wake_lease);
        }
    }

    pub fn run_watchers(
        watchers: Arc<Mutex<Vec<fpower::BatteryInfoWatcherProxy>>>,
        info: fpower::BatteryInfo,
        wake_lease: Option<zx::EventPair>,
    ) {
        debug!("::manager:: run watchers...");
        fasync::Task::spawn(async move {
            let watchers = {
                let mut watchers = watchers.lock().await;
                watchers.retain(|w| !w.is_closed());
                watchers.clone()
            };
            debug!("::manager:: run watchers [{:?}]", &watchers.len());
            for w in &watchers {
                let wake_lease_dup = Self::duplicate_wake_lease(&wake_lease);
                if let Err(e) = w.on_change_battery_info(&info.clone().into(), wake_lease_dup).await
                {
                    error!("failed to send battery info to watcher {:?}", e);
                }
            }
        })
        .detach()
    }

    fn update_battery_info(&self, info: fpower::BatteryInfo) {
        let new_charge_status = info.charge_status;

        let mut new_battery_info = self.battery_info.write().unwrap();
        *new_battery_info = info;
        let now = get_current_time();
        new_battery_info.timestamp = Some(now);

        Self::publish_charge_status(self.history_logger.clone(), new_charge_status);
    }

    pub fn get_battery_info_copy(&self) -> fpower::BatteryInfo {
        if *self.simulation_state.read().unwrap() {
            let info_lock = self.simulated_battery_info.read().unwrap();
            (*info_lock).clone()
        } else {
            let info_lock = self.battery_info.read().unwrap();
            (*info_lock).clone()
        }
    }

    fn update_watchers(&self, wake_lease: Option<zx::EventPair>) {
        let info = self.get_battery_info_copy();
        let watchers = self.watchers.clone();
        BatteryManager::run_watchers(watchers, info, wake_lease);
    }

    pub fn is_simulating(&self) -> bool {
        *self.simulation_state.read().unwrap()
    }

    pub(crate) async fn serve(
        &self,
        stream: fpower::BatteryManagerRequestStream,
    ) -> Result<(), Error> {
        stream
            .try_for_each_concurrent(None, move |request| {
                async move {
                    match request {
                        fpower::BatteryManagerRequest::GetBatteryInfo { responder, .. } => {
                            let info = self.get_battery_info_copy();
                            debug!(
                                info:?;
                                "::battery_manager_request:: handle GetBatteryInfo request"
                            );
                            responder.send(&info)?;
                        }
                        fpower::BatteryManagerRequest::Watch { watcher, .. } => {
                            let watcher = watcher.into_proxy();
                            debug!("::battery_manager_request:: handle Watch request");
                            self.add_watcher(watcher.clone()).await;

                            // Make sure watcher has current battery info.
                            // But there is no copy of the wake lease.
                            let info = self.get_battery_info_copy();
                            debug!(info:?; "::battery_manager_request:: callback on new watcher");
                            watcher.on_change_battery_info(&info, None).await?;
                        }
                    }
                    Ok(())
                }
            })
            .await?;

        Ok(())
    }

    // Called by start_watching_battery_info to process the OnChangeBatteryInfo Call
    async fn wait_on_updates(
        &self,
        watcher: fidl::endpoints::ServerEnd<fpower::BatteryInfoWatcherMarker>,
    ) -> Result<(), Error> {
        let mut stream = watcher.into_stream();
        while let Some(event) = stream.try_next().await? {
            match event {
                fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                    info,
                    wake_lease,
                    responder,
                } => {
                    self.update_battery_info(info);
                    self.update_watchers_conditionally(false, wake_lease);
                    responder.send()?;
                }
            }
        }
        Ok(())
    }

    // Main should explicitly call this, so Battery Manager starts to watch the battery info from
    // battery driver, and conditionally dispatches to clients according to simulating state.
    pub(crate) async fn start_watching_battery_info(
        &self,
        proxy: fpower::BatteryInfoProviderProxy,
    ) -> Result<(), Error> {
        let (client_end, server_end) =
            fidl::endpoints::create_endpoints::<fpower::BatteryInfoWatcherMarker>();
        proxy.watch(client_end)?;
        Self::periodically_publish_battery_level(
            self.history_logger.clone(),
            self.battery_info.clone(),
        )
        .detach();
        self.wait_on_updates(server_end).await
    }

    fn periodically_publish_battery_level(
        history_logger: Arc<SMutex<HistoryLogger>>,
        info: Arc<RwLock<fpower::BatteryInfo>>,
    ) -> fasync::Task<()> {
        let mut last_valid_level: f32 = 0.0;
        let mut interval = fasync::Interval::new(BATTERY_LEVEL_PUBLISH_INTERVAL.into());
        fasync::Task::local(async move {
            loop {
                // Immediately publish battery level to get an early measurement.
                let level = info.read().unwrap().level_percent;

                // Use the most recent valid value.
                let level_to_publish = level.unwrap_or(last_valid_level);
                last_valid_level = level_to_publish;
                Self::publish_battery_level(history_logger.clone(), level_to_publish);

                interval.next().await;
            }
        })
    }

    fn publish_battery_level(history_logger: Arc<SMutex<HistoryLogger>>, percent: f32) {
        history_logger.lock().add_battery_level(zx::BootInstant::get(), percent as i32);
    }

    fn publish_charge_status(
        history_logger: Arc<SMutex<HistoryLogger>>,
        status: Option<fpower::ChargeStatus>,
    ) {
        history_logger.lock().update_charge_status(zx::BootInstant::get(), status);
    }

    // This function takes a reference to an Option<zx::EventPair>
    // and returns a new Option containing a duplicated handle, or None.
    fn duplicate_wake_lease(wake_lease_ref: &Option<zx::EventPair>) -> Option<zx::EventPair> {
        if let Some(handle_ref) = wake_lease_ref.as_ref() {
            handle_ref.duplicate_handle(zx::Rights::SAME_RIGHTS).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HistoryLoggerConfig;
    use fidl::AsHandleRef;
    use fidl::endpoints::create_request_stream;
    use fuchsia_inspect::{self as inspect};
    use futures::channel::oneshot;
    use futures::future::*;
    use log::info;
    use tempfile::{TempDir, tempdir};

    fn create_config(
        dir: &TempDir,
        battery_level_buffer_capacity: usize,
        charge_status_buffer_capacity: usize,
        curr_boot_file: &str,
        prev_boot_file: &str,
    ) -> HistoryLoggerConfig {
        HistoryLoggerConfig {
            curr_boot_path: dir.path().join(curr_boot_file).to_str().unwrap().to_string(),
            prev_boot_path: dir.path().join(prev_boot_file).to_str().unwrap().to_string(),
            battery_level_buffer_capacity,
            charge_status_buffer_capacity,
        }
    }

    pub fn create_manager() -> (TempDir, Arc<BatteryManager>) {
        let inspector = inspect::Inspector::default();
        let dir = tempdir().unwrap();

        let config = create_config(&dir, 3, 3, "curr_data.txt", "prev_data.txt");
        let logger = HistoryLogger::from_file(inspector.root(), config);
        let battery_manager = Arc::new(BatteryManager::new_with_logger(logger));
        (dir, battery_manager)
    }

    #[fuchsia::test]
    async fn test_run_watcher() {
        info!("Starting");
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        let (_dir, battery_manager) = create_manager();
        let mut battery_info: fpower::BatteryInfo = battery_manager.get_battery_info_copy();
        battery_info.level_percent = Some(50.0);

        let (watcher_client_end, mut stream) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher = watcher_client_end.into_proxy();

        let watchers = Arc::new(Mutex::new(vec![watcher]));

        // Create a zx::EventPair for the test
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id = token_info.koid;
        let wake_lease = Some(rx); // The rx handle is what we'll pass to the server

        let serve_fut = async move {
            info!("Try_nest");
            let request = stream.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: Some(received_lease),
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);

                let token_info = received_lease.basic_info().unwrap();
                let related_id = token_info.related_koid;
                assert_eq!(related_id, tx_id);
                info!("fake watcher ends checking lease");

                responder.send().unwrap();
                let _ = tx_signal.send(());
            } else {
                panic!("Unexpected message received");
            };
        };
        let request_fut = async move {
            info!("Running watchers");
            BatteryManager::run_watchers(watchers, battery_info, wake_lease);
        };

        join(serve_fut, request_fut).await;
        rx_signal.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_run_watchers_channel_closed() {
        let (_dir, battery_manager) = create_manager();
        let mut battery_info: fpower::BatteryInfo = battery_manager.get_battery_info_copy();
        battery_info.level_percent = Some(50.0);

        let (watcher1_client_end, mut stream1) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher1 = watcher1_client_end.into_proxy();

        let (watcher2_client_end, mut stream2) =
            create_request_stream::<fpower::BatteryInfoWatcherMarker>();
        let watcher2 = watcher2_client_end.into_proxy();

        let watchers = Arc::new(Mutex::new(vec![watcher1, watcher2]));

        let serve1_fut = async move {
            // first request should match first change notification sent
            // at 50%
            let request = stream1.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received");
            };
            // second should match subsequent notification at 60%
            let request = stream1.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 60);
                responder.send().unwrap();
            } else {
                panic!("Unexpected message received");
            };
        };

        let serve2_fut = async move {
            // first request should match first change notification sent
            // at 50%
            let request = stream2.try_next().await.unwrap();
            if let Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: None,
                responder,
            }) = request
            {
                let level = info.level_percent.unwrap().round() as u8;
                assert_eq!(level, 50);
                // but then we drop the channel...
                std::mem::drop(responder);
            } else {
                panic!("Unexpected message received");
            };
            // should not get the second...
            if let Some(_) = stream2.try_next().await.unwrap() {
                panic!("Unexpected message, channel should be closed");
            }
        };

        let request_fut = async move {
            BatteryManager::run_watchers(watchers.clone(), battery_info.clone(), None);
            battery_info.level_percent = Some(60.0);
            BatteryManager::run_watchers(watchers, battery_info, None);
        };

        join3(serve1_fut, serve2_fut, request_fut).await;
    }

    // This function acts as a fake watcher, processing FIDL messages
    fn fake_watcher(
        info_checker: impl Fn(fpower::BatteryInfo) + 'static,
        lease_checker: impl FnOnce(Option<zx::EventPair>) + 'static,
    ) -> fpower::BatteryInfoWatcherProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::BatteryInfoWatcherMarker>();
        fasync::Task::local(async move {
            if let Ok(req) = stream.try_next().await {
                match req {
                    Some(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                        info,
                        wake_lease,
                        responder,
                    }) => {
                        info_checker(info);
                        lease_checker(wake_lease);
                        let _ = responder.send();
                    }
                    e => panic!("Unexpected request: {:?}", e),
                }
            }
        })
        .detach();

        proxy
    }

    #[fuchsia::test]
    async fn test_wait_on_updates() {
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        // Prepare the wake_lease. For the tx, we need to obtain its koid.
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info().unwrap();
        let tx_id = token_info.koid;
        let wake_lease = Some(rx);

        let (_dir, battery_manager) = create_manager();

        // Create a client and server pair for the FIDL call to be used by the pair of
        // wait_on_updates(business logic) and on_change_battery_info(test)
        let (proxy, server_end) =
            fidl::endpoints::create_proxy::<fpower::BatteryInfoWatcherMarker>();

        // Set some battery info, and add a fake watcher.
        let mut updated_info = battery_manager.get_battery_info_copy();
        updated_info.level_percent = Some(60.0);
        updated_info.status = Some(fpower::BatteryStatus::Ok);
        battery_manager
            .add_watcher(fake_watcher(
                move |info| {
                    assert_eq!(info.level_percent, Some(60.0));
                    assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
                },
                move |lease| {
                    let lease = lease.expect("Should not be None");
                    let token_info = lease.basic_info().unwrap();
                    let related_id = token_info.related_koid;
                    assert_eq!(related_id, tx_id);
                    info!("fake watcher ends checking lease");
                    let _ = tx_signal.send(());
                },
            ))
            .await;

        // The 'server' task: run wait_on_updates in the background
        let battery_clone = battery_manager.clone();
        let server_task =
            fasync::Task::spawn(
                async move { battery_clone.clone().wait_on_updates(server_end).await },
            );

        let client_fut = async move {
            proxy.on_change_battery_info(&updated_info, wake_lease).await.unwrap();
        };

        // Run both the server task and client future concurrently
        let _ = join(server_task, client_fut).await;

        // After the futures complete, check the state of the BatteryManager
        let final_info = battery_manager.get_battery_info_copy();

        // Assert that the state was updated
        assert_eq!(final_info.level_percent, Some(60.0));
        assert_eq!(final_info.status, Some(fpower::BatteryStatus::Ok));

        rx_signal.await.unwrap();
    }

    // This function acts as a fake watcher, processing FIDL messages
    fn fake_driver(
        info: fpower::BatteryInfo,
        lease: Option<zx::EventPair>,
    ) -> fpower::BatteryInfoProviderProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::BatteryInfoProviderMarker>();
        fasync::Task::local(async move {
            while let Ok(req) = stream.try_next().await {
                match req {
                    Some(fpower::BatteryInfoProviderRequest::Watch { watcher, .. }) => {
                        let watcher = watcher.into_proxy();
                        let duplicated_lease = BatteryManager::duplicate_wake_lease(&lease);
                        assert!(
                            watcher.on_change_battery_info(&info, duplicated_lease).await.is_ok()
                        );
                    }
                    e => panic!("Unexpected request: {:?}", e),
                }
            }
        })
        .detach();

        proxy
    }

    #[fuchsia::test]
    async fn test_start_watching_battery_info() -> Result<(), Error> {
        // To guarantee the code in the fake_watcher gets executed to the end.
        let (tx_signal, rx_signal) = oneshot::channel();

        // Prepare the wake_lease. For the tx, we need to obtain its koid.
        let (tx, rx) = zx::EventPair::create();
        let token_info = tx.basic_info()?;
        let tx_id = token_info.koid;
        let wake_lease = Some(rx);

        // Set some battery info, and add a fake watcher.
        let (_dir, battery_manager) = create_manager();
        let mut updated_info = battery_manager.get_battery_info_copy();
        updated_info.level_percent = Some(60.0);
        updated_info.status = Some(fpower::BatteryStatus::Ok);
        updated_info.timestamp = Some(20);
        let first_info = battery_manager.get_battery_info_copy();
        let first_timestamp = first_info.timestamp.unwrap();

        battery_manager
            .add_watcher(fake_watcher(
                move |info| {
                    assert_eq!(info.level_percent, Some(60.0));
                    assert_eq!(info.status, Some(fpower::BatteryStatus::Ok));
                    let timestamp = info.timestamp.unwrap();
                    assert!(timestamp >= first_timestamp);
                },
                move |lease| {
                    let lease = lease.expect("Should not be None");
                    let token_info = lease.basic_info().unwrap();
                    let related_id = token_info.related_koid;
                    assert_eq!(related_id, tx_id);
                    info!("fake watcher ends checking lease");
                    let _ = tx_signal.send(());
                },
            ))
            .await;

        // test start_watching_battery_info
        let _ = battery_manager
            .clone()
            .start_watching_battery_info(fake_driver(updated_info, wake_lease))
            .await;

        // After the futures complete, check the state of the BatteryManager
        let final_info = battery_manager.get_battery_info_copy();

        // Assert that the state was updated
        assert_eq!(final_info.level_percent, Some(60.0));
        assert_eq!(final_info.status, Some(fpower::BatteryStatus::Ok));

        rx_signal.await.unwrap();
        Ok(())
    }
}
