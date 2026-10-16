// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The complexity of a separate struct doesn't seem universally better than having many arguments
#![allow(clippy::too_many_arguments)]
#![recursion_limit = "1024"]
// Turn on additional lints that could lead to unexpected crashes in production code
#![warn(clippy::indexing_slicing)]
#![cfg_attr(test, allow(clippy::indexing_slicing))]
#![warn(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![warn(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::expect_used))]
#![warn(clippy::unreachable)]
#![cfg_attr(test, allow(clippy::unreachable))]
#![warn(clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unimplemented))]

use anyhow::{Context as _, Error, format_err};
use diagnostics_log::PublishOptions;
use fidl::endpoints::ControlHandle as _;
use fidl_fuchsia_location_namedplace::RegulatoryRegionWatcherMarker;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_system as fsystem;
use fidl_fuchsia_process_lifecycle as flifecycle;
use fidl_fuchsia_wlan_device_service::DeviceMonitorMarker;
use fidl_fuchsia_wlan_policy as fidl_policy;
use fuchsia_async as fasync;
use fuchsia_async::DurationExt;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_trace_provider as ftrace_provider;
use futures::channel::{mpsc, oneshot};
use futures::lock::Mutex;
use futures::prelude::*;
use futures::{TryFutureExt, select};
use log::{error, info, warn};
use std::convert::Infallible;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use wlan_power_manager::{DevicePowerManager, PowerManager};
use wlan_trace as wtrace;
use wlancfg_lib::access_point::AccessPoint;
use wlancfg_lib::client::connection_selection::{
    CONNECTION_SELECTION_REQUEST_BUFFER_SIZE, ConnectionSelectionRequester, ConnectionSelector,
    serve_connection_selection_request_loop,
};
use wlancfg_lib::client::roaming::lib::ROAMING_CHANNEL_BUFFER_SIZE;
use wlancfg_lib::client::roaming::local_roam_manager::{
    RoamManager, serve_local_roam_manager_requests,
};
use wlancfg_lib::client::{self, scan};
use wlancfg_lib::config_management::{SavedNetworksManager, SavedNetworksManagerApi};
use wlancfg_lib::legacy::{self, IfaceRef};
use wlancfg_lib::mode_management::iface_manager_api::IfaceManagerApi;
use wlancfg_lib::mode_management::phy_manager::{self, PhyManager, PhyManagerApi};
use wlancfg_lib::mode_management::{
    DEFECT_CHANNEL_SIZE, create_iface_manager, device_monitor, recovery,
};
use wlancfg_lib::regulatory_manager::RegulatoryManager;
use wlancfg_lib::telemetry::{
    TelemetrySender, connect_to_metrics_logger_factory, create_metrics_logger, serve_telemetry,
};
use wlancfg_lib::util;

const REGULATORY_LISTENER_TIMEOUT_SEC: i64 = 30;

async fn serve_fidl(
    ap: AccessPoint,
    configurator: legacy::deprecated_configuration::DeprecatedConfigurator,
    iface_manager: Arc<Mutex<dyn IfaceManagerApi>>,
    legacy_client_ref: IfaceRef,
    saved_networks: Arc<dyn SavedNetworksManagerApi>,
    scan_requester: Arc<dyn scan::ScanRequestApi>,
    client_sender: util::listener::ClientListenerMessageSender,
    client_listener_msgs: mpsc::UnboundedReceiver<util::listener::ClientListenerMessage>,
    ap_listener_msgs: mpsc::UnboundedReceiver<util::listener::ApMessage>,
    regulatory_receiver: oneshot::Receiver<()>,
    telemetry_sender: TelemetrySender,
    power_manager: Arc<dyn wlan_power_manager::PowerManager>,
) -> Result<Infallible, Error> {
    // Wait a bit for the country code to be set before serving the policy APIs.
    let regulatory_listener_timeout = fasync::Timer::new(
        zx::MonotonicDuration::from_seconds(REGULATORY_LISTENER_TIMEOUT_SEC).after_now(),
    );
    select! {
        _ = regulatory_listener_timeout.fuse() => {
            // Log at info level because we expect RegulatoryManager may in some
            // cases be delayed.
            info!(
                "RegulatoryManager did not send a country code after {} seconds.",
                REGULATORY_LISTENER_TIMEOUT_SEC,
            );
        },
        result = regulatory_receiver.fuse() => {
            match result {
                Ok(()) => {
                    info!("Received country code from RegulatoryManager.");
                },
                // Log at warning level because RegulatoryManager should not
                // normally drop its end.
                Err(e) => warn!("Failed to receive country code from RegulatoryManager: {:?}", e),
            }
        }
    }
    info!("Proceeding to serve policy API.");

    let mut fs = ServiceFs::new_local();

    let _inspect_server_task = inspect_runtime::publish(
        component::inspector(),
        inspect_runtime::PublishOptions::default(),
    );

    let client_sender1 = client_sender.clone();
    let client_sender2 = client_sender.clone();

    let second_ap = ap.clone();

    let saved_networks_clone = saved_networks.clone();

    let client_provider_lock = Arc::new(Mutex::new(()));

    // TODO(sakuma): Once the legacy API is deprecated, the interface manager should default to
    // stopped.
    {
        let mut iface_manager = iface_manager.lock().await;
        iface_manager.start_client_connections().await?;
    }

    let power_manager_clone = power_manager.clone();
    let _ = fs
        .dir("svc")
        .add_fidl_service(move |reqs| {
            fasync::Task::local(client::serve_provider_requests(
                iface_manager.clone(),
                client_sender1.clone(),
                Arc::clone(&saved_networks_clone),
                scan_requester.clone(),
                client_provider_lock.clone(),
                reqs,
                telemetry_sender.clone(),
                power_manager_clone.clone(),
            ))
            .detach()
        })
        .add_fidl_service(move |reqs| {
            fasync::Task::local(client::serve_listener_requests(client_sender2.clone(), reqs))
                .detach()
        })
        .add_fidl_service(move |reqs| {
            fasync::Task::local(ap.clone().serve_provider_requests(reqs)).detach()
        })
        .add_fidl_service(move |reqs| {
            fasync::Task::local(second_ap.clone().serve_listener_requests(reqs)).detach()
        })
        .add_fidl_service(move |reqs| {
            fasync::Task::local(configurator.clone().serve_deprecated_configuration(reqs)).detach()
        })
        .add_fidl_service(|reqs| {
            let fut =
                legacy::deprecated_client::serve_deprecated_client(reqs, legacy_client_ref.clone())
                    .unwrap_or_else(|e| error!("error serving deprecated client API: {}", e));
            fasync::Task::local(fut).detach()
        });
    let service_fut = fs.take_and_serve_directory_handle()?.collect::<()>().fuse();
    let mut service_fut = pin!(service_fut);

    let serve_client_policy_listeners = util::listener::serve::<
        fidl_policy::ClientStateUpdatesProxy,
        fidl_policy::ClientStateSummary,
        util::listener::ClientStateUpdate,
    >(client_listener_msgs)
    .fuse();
    let mut serve_client_policy_listeners = pin!(serve_client_policy_listeners);

    let serve_ap_policy_listeners = util::listener::serve::<
        fidl_policy::AccessPointStateUpdatesProxy,
        Vec<fidl_policy::AccessPointState>,
        util::listener::ApStatesUpdate,
    >(ap_listener_msgs)
    .fuse();
    let mut serve_ap_policy_listeners = pin!(serve_ap_policy_listeners);

    loop {
        select! {
            _ = service_fut => (),
            _ = serve_client_policy_listeners => (),
            _ = serve_ap_policy_listeners => (),
        }
    }
}

/// Calls the metric recording function immediately and every 24 hours.
async fn saved_networks_manager_metrics_loop(saved_networks: Arc<dyn SavedNetworksManagerApi>) {
    loop {
        saved_networks.record_periodic_metrics().await;
        fasync::Timer::new(zx::MonotonicDuration::from_hours(24).after_now()).await;
    }
}

// wlancfg expects to be able to get updates from the RegulatoryRegionWatcher UNLESS the
// service is not present in wlancfg's sandbox OR the product configuration does not offer the
// service to wlancfg.  If the RegulatoryRegionWatcher is not available for either of these
// allowed reasons, wlancfg will continue serving the WLAN policy API in WW mode.
async fn run_regulatory_manager(
    iface_manager: Arc<Mutex<dyn IfaceManagerApi>>,
    regulatory_sender: oneshot::Sender<()>,
    power_manager: Arc<dyn wlan_power_manager::PowerManager>,
) -> Result<(), Error> {
    // This initial connection will always succeed due to the presence of the protocol in the
    // component manifest.
    let req = match fuchsia_component::client::new_protocol_connector::<RegulatoryRegionWatcherMarker>(
    ) {
        Ok(req) => req,
        Err(e) => {
            warn!("error probing RegulatoryRegionWatcher service: {:?}", e);
            return Ok(());
        }
    };

    // An error here indicates that there is a missing `use` statement and the
    // RegulatoryRegionWatcher is not available in this namespace.  This should never happen since
    // the build system checks the capability routes and wlancfg needs this service.
    if !req.exists().await.context("error checking for RegulatoryRegionWatcher existence")? {
        warn!("RegulatoryRegionWatcher is not available per the component manifest");
        return Ok(());
    }

    // The connect call will always succeed because all it does is send the server end of a channel
    // to the directory that holds the capability.
    let regulatory_svc =
        req.connect().context("unable to connect RegulatoryRegionWatcher proxy")?;

    let regulatory_manager = RegulatoryManager::new(regulatory_svc, iface_manager, power_manager);

    // The only way to test for the presence of the RegulatoryRegionWatcher service is to actually
    // use the handle to poll for updates.  If the RegulatoryManager future exits, simply log the
    // reason and return Ok(()) so that the WLAN policy API will continue to be served.
    if let Some(e) = regulatory_manager.run(regulatory_sender).await.err() {
        warn!("RegulatoryManager exiting: {:?}", e);
    }
    Ok(())
}

async fn serve_lifecycle<F>(
    phy_manager: Arc<Mutex<dyn PhyManagerApi>>,
    mut requests: flifecycle::LifecycleRequestStream,
    main_fut: F,
) -> Result<(), Error>
where
    F: std::future::Future<Output = Result<(), Error>>,
{
    let wait_for_stop = async {
        match requests.next().await {
            Some(Ok(flifecycle::LifecycleRequest::Stop { control_handle })) => control_handle,
            Some(Err(e)) => {
                warn!("Lifecycle request stream error: {:?}; waiting indefinitely", e);
                futures::future::pending().await
            }
            None => {
                warn!("Lifecycle request stream closed without Stop; waiting indefinitely");
                futures::future::pending().await
            }
        }
    };
    let mut main_fut = Box::pin(main_fut.fuse());
    let mut wait_for_stop = pin!(wait_for_stop.fuse());
    let control_handle = select! {
        res = main_fut => return res,
        control_handle = wait_for_stop => control_handle,
    };
    // Explicitly drop `main_fut` before acquiring the `phy_manager` lock so that any lock guard
    // held across an `.await` point inside `main_fut` is released and cannot deadlock shutdown.
    drop(main_fut);
    info!("Received lifecycle stop request; destroying all interfaces before shutdown");
    let mut phy_manager = phy_manager.lock().await;
    if let Err(e) = phy_manager.destroy_all_client_ifaces().await {
        warn!("Failed to destroy client interfaces during shutdown: {:?}", e);
    }
    if let Err(e) = phy_manager.destroy_all_ap_ifaces().await {
        warn!("Failed to destroy AP interfaces during shutdown: {:?}", e);
    }
    for phy_id in phy_manager.get_phy_ids() {
        phy_manager.remove_phy(phy_id);
    }
    control_handle.shutdown();
    Ok(())
}

async fn run_all_futures() -> Result<(), Error> {
    // Get wlancfg product config data.
    let cfg = wlancfg_config::Config::take_from_startup_handle();

    // Set up power framework
    info!("Power Framework Enabled: {}", cfg.power_framework_enabled);
    let proxies = if cfg.power_framework_enabled {
        match (
            fuchsia_component::client::connect_to_protocol::<fsystem::ActivityGovernorMarker>(),
            fuchsia_component::client::connect_to_protocol::<fbroker::TopologyMarker>(),
        ) {
            (Ok(ag), Ok(pb)) => Some((ag, pb)),
            (ag_res, pb_res) => {
                if let Err(e) = ag_res {
                    warn!("Failed to connect to fuchsia.power.system.ActivityGovernor: {:?}", e);
                }
                if let Err(e) = pb_res {
                    warn!("Failed to connect to fuchsia.power.broker.Topology: {:?}", e);
                }
                None
            }
        }
    } else {
        None
    };
    let power_manager = Arc::new(DevicePowerManager::new(proxies));
    // Register for suspend/resume notifications
    let (initial_wake_lease, suspend_blocker_requests) = if cfg.power_framework_enabled {
        let (suspend_blocker_client, suspend_blocker_requests) =
            fidl::endpoints::create_request_stream::<fsystem::SuspendBlockerMarker>();
        let lease = match power_manager
            .register_suspend_blocker(suspend_blocker_client, "wlancfg-suspend-blocker")
            .await
        {
            Ok(lease) => Some(lease),
            Err(e) => {
                warn!("Failed to register suspend blocker: {:?}", e);
                None
            }
        };
        (lease, Some(suspend_blocker_requests))
    } else {
        (None, None)
    };

    let monitor_svc = fuchsia_component::client::connect_to_protocol::<DeviceMonitorMarker>()
        .context("failed to connect to device monitor")?;

    let cobalt_svc = connect_to_metrics_logger_factory().await?;
    let cobalt_proxy = match create_metrics_logger(&cobalt_svc).await {
        Ok(proxy) => proxy,
        Err(e) => {
            warn!("Metrics logging is unavailable: {}", e);

            // If it is not possible to acquire a metrics logging proxy, create a disconnected
            // proxy and attempt to serve the policy API with metrics disabled.
            let (proxy, _) =
                fidl::endpoints::create_proxy::<fidl_fuchsia_metrics::MetricEventLoggerMarker>();
            proxy
        }
    };

    let external_inspect_node = component::inspector().root().create_child("external");
    let (defect_sender, defect_receiver) = mpsc::channel(DEFECT_CHANNEL_SIZE);
    let (telemetry_sender, telemetry_fut) = serve_telemetry(
        monitor_svc.clone(),
        cobalt_proxy,
        component::inspector().root().create_child("client_stats"),
        external_inspect_node.create_child("client_stats"),
        defect_sender.clone(),
    );
    component::inspector().root().record(external_inspect_node);

    let (scan_request_sender, scan_request_receiver) =
        mpsc::channel(scan::SCAN_REQUEST_BUFFER_SIZE);
    let scan_requester = Arc::new(scan::ScanRequester { sender: scan_request_sender });
    let saved_networks = Arc::new(SavedNetworksManager::new(telemetry_sender.clone()).await);

    let connection_selector = Rc::new(ConnectionSelector::new(
        saved_networks.clone(),
        scan_requester.clone(),
        component::inspector().root().create_child("connection_selector"),
        telemetry_sender.clone(),
    ));
    let (connection_selection_request_sender, connection_selection_request_receiver) =
        mpsc::channel(CONNECTION_SELECTION_REQUEST_BUFFER_SIZE);
    let connection_selection_service = serve_connection_selection_request_loop(
        connection_selector,
        connection_selection_request_receiver,
        power_manager.clone(),
    );
    let connection_selection_requester =
        ConnectionSelectionRequester::new(connection_selection_request_sender);

    info!("Roaming policy: {}", cfg.roaming_policy);
    let (roam_service_request_sender, roam_service_request_receiver) =
        mpsc::channel(ROAMING_CHANNEL_BUFFER_SIZE);
    let roam_manager_service_fut = serve_local_roam_manager_requests(
        cfg.roaming_policy.into(),
        roam_service_request_receiver,
        connection_selection_requester.clone(),
        telemetry_sender.clone(),
        saved_networks.clone(),
    );
    let roam_manager = RoamManager::new(roam_service_request_sender);

    let (recovery_sender, recovery_receiver) =
        mpsc::channel::<recovery::RecoverySummary>(recovery::RECOVERY_SUMMARY_CHANNEL_CAPACITY);
    // Get the recovery settings.
    info!("Recovery Profile: {}", cfg.recovery_profile);
    info!("Recovery Enabled: {}", cfg.recovery_enabled);

    let phy_manager = Arc::new(Mutex::new(PhyManager::new(
        monitor_svc.clone(),
        recovery::lookup_recovery_profile(&cfg.recovery_profile),
        cfg.recovery_enabled,
        component::inspector().root().create_child("phy_manager"),
        telemetry_sender.clone(),
        recovery_sender,
        power_manager.clone(),
    )));
    let configurator =
        legacy::deprecated_configuration::DeprecatedConfigurator::new(phy_manager.clone());

    let (watcher_proxy, watcher_server_end) = fidl::endpoints::create_proxy();
    monitor_svc.watch_devices(watcher_server_end)?;

    let (client_sender, client_receiver) = mpsc::unbounded();
    let (ap_sender, ap_receiver) = mpsc::unbounded();
    let (iface_manager, iface_manager_service) = create_iface_manager(
        phy_manager.clone(),
        client_sender.clone(),
        ap_sender.clone(),
        monitor_svc.clone(),
        saved_networks.clone(),
        connection_selection_requester,
        roam_manager,
        telemetry_sender.clone(),
        defect_sender,
        defect_receiver,
        recovery_receiver,
        component::inspector().root().create_child("iface_manager"),
        power_manager.clone(),
    );

    let scanning_service = scan::serve_scanning_loop(
        iface_manager.clone(),
        saved_networks.clone(),
        telemetry_sender.clone(),
        scan::LocationSensorUpdater {},
        scan_request_receiver,
    );

    let legacy_client = IfaceRef::new();
    let device_event_listener = device_monitor::Listener::new(
        monitor_svc.clone(),
        legacy_client.clone(),
        phy_manager.clone(),
        iface_manager.clone(),
        power_manager.clone(),
    );

    let (regulatory_sender, regulatory_receiver) = oneshot::channel();
    let ap = AccessPoint::new(
        iface_manager.clone(),
        ap_sender,
        Arc::new(Mutex::new(())),
        power_manager.clone(),
    );
    let fidl_fut = serve_fidl(
        ap,
        configurator,
        iface_manager.clone(),
        legacy_client,
        saved_networks.clone(),
        scan_requester,
        client_sender,
        client_receiver,
        ap_receiver,
        regulatory_receiver,
        telemetry_sender.clone(),
        power_manager.clone(),
    );

    let dev_watcher_fut = watcher_proxy
        .take_event_stream()
        .try_for_each(|evt| device_monitor::handle_event(&device_event_listener, evt).map(Ok))
        .err_into()
        .and_then(|_| {
            let result: Result<(), Error> =
                Err(format_err!("Device watcher future exited unexpectedly"));
            future::ready(result)
        });

    let saved_networks_metrics_fut = saved_networks_manager_metrics_loop(saved_networks.clone());
    let regulatory_fut =
        run_regulatory_manager(iface_manager.clone(), regulatory_sender, power_manager.clone());

    let suspend_blocker_fut: future::LocalBoxFuture<'static, Result<(), Error>> =
        if let Some(requests) = suspend_blocker_requests {
            Box::pin(phy_manager::serve_suspend_blocker(phy_manager.clone(), requests))
        } else {
            Box::pin(future::ready(Ok(())))
        };

    // We're initialized now, we can stop keeping the system awake
    drop(initial_wake_lease);

    let mut telemetry_fut = pin!(telemetry_fut.map(Ok));
    let main_fut = async {
        let _ = futures::try_join!(
            fidl_fut,
            dev_watcher_fut,
            iface_manager_service,
            saved_networks_metrics_fut.map(Ok),
            scanning_service,
            regulatory_fut,
            &mut telemetry_fut,
            roam_manager_service_fut.map(Ok),
            connection_selection_service.map(Ok),
            suspend_blocker_fut,
        )?;
        Err(format_err!("WLAN policy layer stopped"))
    };

    match fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::Lifecycle.into()) {
        Some(lifecycle_handle) => {
            let request_stream =
                fidl::endpoints::ServerEnd::<flifecycle::LifecycleMarker>::from(lifecycle_handle)
                    .into_stream();
            Box::pin(serve_lifecycle(phy_manager, request_stream, main_fut)).await
        }
        None => {
            warn!("No lifecycle startup handle provided; skipping shutdown listener");
            main_fut.await
        }
    }
}

#[fuchsia::main(logging = false)]
async fn main() -> Result<(), Error> {
    let options =
        PublishOptions::default().tags(&["wlan"]).enable_metatag(diagnostics_log::Metatag::Target);
    // Crash instead of proceeded without logging
    #[expect(clippy::expect_used)]
    diagnostics_log::initialize(options).expect("Failed to initialize diagnostics log");
    ftrace_provider::trace_provider_create_with_fdio();
    wtrace::instant_wlancfg_start();

    // When `run_all_futures` exits due to an unexpected error (rather than a graceful
    // `LifecycleRequest::Stop`), exiting with an error will trigger a reboot on devices
    // where wlancfg is `on_terminate: "reboot"`.
    Box::pin(run_all_futures()).await.inspect_err(
        |e| {
            // When the ELF runner executing wlancfg is configured with `forward_std_err_to: "none"`,
            // the Rust Termination trait's implementation to print an `Err` value to stderr would be lost.
            // For this reason, always send the error directly to the syslog. The additional comment in
            // the log is a reminder in the syslog that the duplicate log is intentional.
            error!("{e:?}. (ELF runner logs this again as a warning unless configured not to forward stderr.)");
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use async_trait::async_trait;
    use fidl::endpoints::{self, Proxy as _};
    use fuchsia_async::TestExecutor;
    use futures::task::Poll;
    use ieee80211::MacAddr;
    use std::collections::HashMap;
    use wlancfg_lib::mode_management::Defect;
    use wlancfg_lib::mode_management::phy_manager::{CreateClientIfacesReason, PhyManagerError};
    use wlancfg_lib::mode_management::recovery::RecoverySummary;
    use wlancfg_lib::regulatory_manager::CountryCode;

    #[derive(Debug, PartialEq)]
    enum PhyManagerCall {
        DestroyAllClientIfaces,
        DestroyAllApIfaces,
        RemovePhy(u16),
    }

    struct FakePhyManager {
        phys: Vec<u16>,
        calls: Vec<PhyManagerCall>,
        destroy_ifaces_succeeds: bool,
    }

    impl FakePhyManager {
        fn new(phys: Vec<u16>, destroy_ifaces_succeeds: bool) -> Self {
            Self { phys, calls: Vec::new(), destroy_ifaces_succeeds }
        }
    }

    #[async_trait(?Send)]
    impl PhyManagerApi for FakePhyManager {
        async fn add_phy(&mut self, _phy_id: u16) -> Result<(), PhyManagerError> {
            unimplemented!()
        }

        fn remove_phy(&mut self, phy_id: u16) {
            self.calls.push(PhyManagerCall::RemovePhy(phy_id));
            self.phys.retain(|phy| *phy != phy_id);
        }

        async fn on_iface_added(&mut self, _iface_id: u16) -> Result<(), PhyManagerError> {
            unimplemented!()
        }

        fn on_iface_removed(&mut self, _iface_id: u16) {
            unimplemented!()
        }

        async fn create_all_client_ifaces(
            &mut self,
            _reason: CreateClientIfacesReason,
        ) -> HashMap<u16, Result<Vec<u16>, PhyManagerError>> {
            unimplemented!()
        }

        fn client_connections_enabled(&self) -> bool {
            unimplemented!()
        }

        async fn destroy_all_client_ifaces(&mut self) -> Result<(), PhyManagerError> {
            self.calls.push(PhyManagerCall::DestroyAllClientIfaces);
            if self.destroy_ifaces_succeeds {
                Ok(())
            } else {
                Err(PhyManagerError::IfaceDestroyFailure)
            }
        }

        fn get_client(&mut self) -> Option<u16> {
            unimplemented!()
        }

        async fn create_or_get_ap_iface(&mut self) -> Result<Option<u16>, PhyManagerError> {
            unimplemented!()
        }

        async fn destroy_ap_iface(&mut self, _iface_id: u16) -> Result<(), PhyManagerError> {
            unimplemented!()
        }

        async fn destroy_all_ap_ifaces(&mut self) -> Result<(), PhyManagerError> {
            self.calls.push(PhyManagerCall::DestroyAllApIfaces);
            if self.destroy_ifaces_succeeds {
                Ok(())
            } else {
                Err(PhyManagerError::IfaceDestroyFailure)
            }
        }

        fn suggest_ap_mac(&mut self, _mac: MacAddr) {
            unimplemented!()
        }

        fn get_phy_ids(&self) -> Vec<u16> {
            self.phys.clone()
        }

        fn log_phy_add_failure(&mut self) {
            unimplemented!()
        }

        async fn set_country_code(
            &mut self,
            _country_code: Option<CountryCode>,
        ) -> Result<(), PhyManagerError> {
            unimplemented!()
        }

        fn record_defect(&mut self, _defect: Defect) {
            unimplemented!()
        }

        async fn perform_recovery(&mut self, _summary: RecoverySummary) {
            unimplemented!()
        }

        async fn on_before_suspend(&mut self) {}
        async fn on_after_resume(&mut self) {}
    }

    #[fuchsia::test]
    fn test_serve_lifecycle_stop_destroys_ifaces_and_removes_phys() {
        let mut exec = TestExecutor::new();
        let phy_manager = Arc::new(Mutex::new(FakePhyManager::new(vec![0, 1], true)));

        // Have `main_fut` hold the `phy_manager` lock across an `.await` point so we verify that
        // `serve_lifecycle` drops `main_fut` before acquiring `phy_manager.lock().await`.
        let phy_manager_for_main = Arc::clone(&phy_manager);
        let (main_tx, main_rx) = oneshot::channel::<Result<(), Error>>();
        let main_fut = async move {
            let _guard = phy_manager_for_main.lock().await;
            main_rx.await.unwrap_or_else(|_| Err(format_err!("canceled")))
        };

        let (lifecycle_proxy, lifecycle_stream) =
            endpoints::create_proxy_and_stream::<flifecycle::LifecycleMarker>();
        let mut lifecycle_fut = pin!(serve_lifecycle(
            Arc::clone(&phy_manager) as Arc<Mutex<dyn PhyManagerApi>>,
            lifecycle_stream,
            main_fut,
        ));
        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Pending);
        assert!(!main_tx.is_canceled());
        assert!(phy_manager.try_lock().is_none());

        lifecycle_proxy.stop().expect("failed to send stop request");

        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Ready(Ok(())));
        assert!(main_tx.is_canceled());
        assert!(lifecycle_proxy.is_closed());

        let phy_manager = phy_manager.try_lock().expect("phy_manager lock should be released");
        assert_eq!(
            phy_manager.calls,
            vec![
                PhyManagerCall::DestroyAllClientIfaces,
                PhyManagerCall::DestroyAllApIfaces,
                PhyManagerCall::RemovePhy(0),
                PhyManagerCall::RemovePhy(1),
            ]
        );
        assert!(phy_manager.phys.is_empty());
    }

    #[fuchsia::test]
    fn test_serve_lifecycle_stop_still_removes_phys_when_destroy_iface_fails() {
        let mut exec = TestExecutor::new();
        let phy_manager = Arc::new(Mutex::new(FakePhyManager::new(vec![0], false)));

        let (lifecycle_proxy, lifecycle_stream) =
            endpoints::create_proxy_and_stream::<flifecycle::LifecycleMarker>();
        let mut lifecycle_fut = pin!(serve_lifecycle(
            Arc::clone(&phy_manager) as Arc<Mutex<dyn PhyManagerApi>>,
            lifecycle_stream,
            futures::future::pending(),
        ));

        lifecycle_proxy.stop().expect("failed to send stop request");

        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Ready(Ok(())));
        assert!(lifecycle_proxy.is_closed());

        let phy_manager = phy_manager.try_lock().expect("phy_manager lock should be released");
        assert_eq!(
            phy_manager.calls,
            vec![
                PhyManagerCall::DestroyAllClientIfaces,
                PhyManagerCall::DestroyAllApIfaces,
                PhyManagerCall::RemovePhy(0),
            ]
        );
        assert!(phy_manager.phys.is_empty());
    }

    #[fuchsia::test]
    fn test_serve_lifecycle_propagates_main_fut_error() {
        let mut exec = TestExecutor::new();
        let phy_manager = Arc::new(Mutex::new(FakePhyManager::new(vec![0], true)));

        let (_lifecycle_proxy, lifecycle_stream) =
            endpoints::create_proxy_and_stream::<flifecycle::LifecycleMarker>();
        let mut lifecycle_fut = pin!(serve_lifecycle(
            Arc::clone(&phy_manager) as Arc<Mutex<dyn PhyManagerApi>>,
            lifecycle_stream,
            future::ready(Err(format_err!("main_fut failed"))),
        ));

        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Ready(Err(_)));
        let phy_manager = phy_manager.try_lock().expect("phy_manager lock should be released");
        assert!(phy_manager.calls.is_empty());
    }

    #[fuchsia::test]
    fn test_serve_lifecycle_waits_indefinitely_when_stream_closes_without_stop() {
        let mut exec = TestExecutor::new();
        let phy_manager = Arc::new(Mutex::new(FakePhyManager::new(vec![0], true)));

        let (main_tx, main_rx) = oneshot::channel::<Result<(), Error>>();
        let main_fut =
            async move { main_rx.await.unwrap_or_else(|_| Err(format_err!("canceled"))) };
        let (lifecycle_proxy, lifecycle_stream) =
            endpoints::create_proxy_and_stream::<flifecycle::LifecycleMarker>();
        drop(lifecycle_proxy);

        let mut lifecycle_fut = pin!(serve_lifecycle(
            Arc::clone(&phy_manager) as Arc<Mutex<dyn PhyManagerApi>>,
            lifecycle_stream,
            main_fut,
        ));
        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Pending);
        assert!(phy_manager.try_lock().expect("phy_manager lock").calls.is_empty());

        main_tx.send(Err(format_err!("main_fut error"))).expect("failed to send main_fut result");
        assert_matches!(exec.run_until_stalled(&mut lifecycle_fut), Poll::Ready(Err(_)));
    }
}
