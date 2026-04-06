// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "1024"]
// Turn on additional lints that could lead to unexpected crashes in production code
#![warn(clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![warn(clippy::expect_used)]
#![warn(clippy::unreachable)]
#![warn(clippy::unimplemented)]

use fidl_fuchsia_hardware_usb_policy as fpolicy;
use fidl_fuchsia_usb_policy as usb_policy;
use fuchsia_component::client::Service;

use anyhow::{Error, format_err};

use fidl_fuchsia_hardware_usb_policy::DeviceState;
use futures::{FutureExt, StreamExt};
use log::warn;
use std::sync::Arc;
mod controller;

use fuchsia_component::server::ServiceFs;

struct SharedState {
    controller: std::sync::Mutex<Option<Arc<controller::ControllerState>>>,
    waiters: std::sync::Mutex<Vec<futures::channel::oneshot::Sender<()>>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self { controller: std::sync::Mutex::new(None), waiters: std::sync::Mutex::new(Vec::new()) }
    }

    pub fn set_controller(&self, state: Arc<controller::ControllerState>) {
        let mut controller_guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        *controller_guard = Some(state);
        let mut waiters_guard = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        for sender in waiters_guard.drain(..) {
            let _ = sender.send(());
        }
    }

    pub fn get_controller(&self) -> Option<Arc<controller::ControllerState>> {
        let controller_guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        controller_guard.clone()
    }

    pub async fn wait_for_controller(&self) -> Arc<controller::ControllerState> {
        loop {
            if let Some(controller) = self.get_controller() {
                return controller;
            }
            let (sender, receiver) = futures::channel::oneshot::channel();
            {
                let mut waiters_guard = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
                waiters_guard.push(sender);
            }
            let _ = receiver.await;
        }
    }
}

enum IncomingRequest {
    Health(usb_policy::HealthRequestStream),
    Provider(usb_policy::PolicyProviderRequestStream),
}

async fn run_provider_server(
    mut stream: usb_policy::PolicyProviderRequestStream,
    shared_state: Arc<SharedState>,
) {
    let state = shared_state.wait_for_controller().await;
    let (initial_state, mut rx) = state.subscribe();
    let mut current_state = initial_state;
    // We haven't sent anything to the client yet, so the first WatchDeviceState
    // should get `current_state`.
    let mut state_changed = true;

    while let Some(Ok(request)) = stream.next().await {
        match request {
            usb_policy::PolicyProviderRequest::WatchDeviceState { responder } => {
                if !state_changed {
                    // Wait until we get an update from rx
                    if let Some(new_state) = rx.next().await {
                        current_state = new_state;
                        state_changed = true;

                        // Drain any additional buffered states
                        while let Some(Some(latest_state)) = rx.next().now_or_never() {
                            current_state = latest_state;
                        }
                    } else {
                        // The sender was dropped, meaning the controller is gone.
                        break;
                    }
                }

                if state_changed {
                    let update = fpolicy::DeviceStateUpdate {
                        state: Some(current_state.device_state),
                        address: Some(current_state.address),
                        ..Default::default()
                    };
                    if let Err(e) = responder.send(Ok(&update)) {
                        warn!("Failed to send PolicyProvider response: {:?}", e);
                    }
                    state_changed = false;
                }
            }
            usb_policy::PolicyProviderRequest::_UnknownMethod { .. } => {
                warn!("Unknown PolicyProvider request");
            }
        }
    }
}

async fn run_health_server(
    mut stream: usb_policy::HealthRequestStream,
    shared_state: Arc<SharedState>,
) {
    while let Some(Ok(request)) = stream.next().await {
        match request {
            usb_policy::HealthRequest::GetReport { responder } => {
                let controller = shared_state.get_controller();
                let report = if let Some(state) = controller {
                    let current_state = state.get_state();
                    usb_policy::HealthReport {
                        state: Some(current_state.device_state),
                        address: Some(current_state.address),
                        ..Default::default()
                    }
                } else {
                    usb_policy::HealthReport { state: None, address: None, ..Default::default() }
                };
                if let Err(e) = responder.send(Ok(&report)) {
                    warn!("Failed to send Health report: {:?}", e);
                }
            }
            usb_policy::HealthRequest::_UnknownMethod { .. } => {
                warn!("Unknown Health request");
            }
        }
    }
}

async fn run_usb_policy_service() -> Result<(), Error> {
    let shared_state = Arc::new(SharedState::new());

    let shared_state_clone = shared_state.clone();
    fuchsia_async::Task::local(async move {
        let result = async {
            let client = Service::open(fpolicy::ServiceMarker)?;
            let instance = client.watch_for_any().await?;
            let controller = instance.connect_to_controller()?;
            let controller_state =
                Arc::new(controller::ControllerState::new(controller, DeviceState::NotAttached, 0));
            shared_state_clone.set_controller(controller_state.clone());
            let _ = controller_state.monitor_device_state().await;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            warn!("Background discovery failed: {:?}", e);
        }
    })
    .detach();

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(IncomingRequest::Health).add_service_at(
        "fuchsia.usb.policy.PolicyProvider",
        fuchsia_component::server::FidlService::from(IncomingRequest::Provider),
    );
    fs.take_and_serve_directory_handle()?;

    let health_server_fut = fs.for_each_concurrent(None, |req| {
        let state = shared_state.clone();
        async move {
            match req {
                IncomingRequest::Health(stream) => run_health_server(stream, state).await,
                IncomingRequest::Provider(stream) => run_provider_server(stream, state).await,
            }
        }
    });

    let _ = health_server_fut.await;
    Ok(())
}

#[fuchsia::main(logging_tags = ["usb-policy"])]
async fn main() -> Result<(), Error> {
    Box::pin(run_usb_policy_service()).await.and(Err(format_err!("USB policy layer stopped")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_async as fasync;

    #[fasync::run_singlethreaded(test)]
    async fn test_health_report() -> Result<(), anyhow::Error> {
        let (controller_proxy, _) = create_proxy_and_stream::<fpolicy::ControllerMarker>();
        let shared_state = Arc::new(SharedState::new());
        let controller_state =
            Arc::new(controller::ControllerState::new(controller_proxy, DeviceState::Attached, 42));
        shared_state.set_controller(controller_state);

        let (health_proxy, stream) = create_proxy_and_stream::<usb_policy::HealthMarker>();

        fasync::Task::local(run_health_server(stream, shared_state)).detach();

        let report_res = health_proxy.get_report().await?;
        let report = match report_res {
            Ok(r) => r,
            Err(e) => return Err(format_err!("Health report error: {:?}", e)),
        };
        assert_eq!(report.state, Some(DeviceState::Attached));
        assert_eq!(report.address, Some(42));
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_provider_server_state() -> Result<(), anyhow::Error> {
        let (controller_proxy, _) = create_proxy_and_stream::<fpolicy::ControllerMarker>();
        let shared_state = Arc::new(SharedState::new());
        let controller_state = Arc::new(controller::ControllerState::new(
            controller_proxy,
            DeviceState::Configured,
            10,
        ));
        shared_state.set_controller(controller_state);

        let (provider_proxy, stream) =
            create_proxy_and_stream::<usb_policy::PolicyProviderMarker>();

        fasync::Task::local(run_provider_server(stream, shared_state)).detach();

        let update_res = provider_proxy.watch_device_state().await?;
        let update = match update_res {
            Ok(u) => u,
            Err(e) => return Err(format_err!("Watch state error: {:?}", e)),
        };
        assert_eq!(update.state, Some(DeviceState::Configured));
        assert_eq!(update.address, Some(10));
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_health_report_not_ready() -> Result<(), anyhow::Error> {
        let shared_state = Arc::new(SharedState::new());
        let (health_proxy, stream) = create_proxy_and_stream::<usb_policy::HealthMarker>();

        fasync::Task::local(run_health_server(stream, shared_state)).detach();

        let report_res = health_proxy.get_report().await?;
        let report = match report_res {
            Ok(r) => r,
            Err(e) => return Err(format_err!("Health report error: {:?}", e)),
        };
        assert_eq!(report.state, None);
        assert_eq!(report.address, None);
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_provider_server_wait() -> Result<(), anyhow::Error> {
        let shared_state = Arc::new(SharedState::new());
        let (provider_proxy, stream) =
            create_proxy_and_stream::<usb_policy::PolicyProviderMarker>();

        fasync::Task::local(run_provider_server(stream, shared_state.clone())).detach();

        let (controller_proxy, _) = create_proxy_and_stream::<fpolicy::ControllerMarker>();
        let controller_state = Arc::new(controller::ControllerState::new(
            controller_proxy,
            DeviceState::Configured,
            10,
        ));

        let shared_state_clone = shared_state.clone();
        fasync::Task::local(async move {
            shared_state_clone.set_controller(controller_state);
        })
        .detach();

        let update_res = provider_proxy.watch_device_state().await?;
        let update = match update_res {
            Ok(u) => u,
            Err(e) => return Err(format_err!("Watch state error: {:?}", e)),
        };
        assert_eq!(update.state, Some(DeviceState::Configured));
        assert_eq!(update.address, Some(10));
        Ok(())
    }
}
