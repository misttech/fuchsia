// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use settings_camera::connect_to_cameras;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::ServiceContext;
use settings_common::trace;

pub struct CameraWatcherAgent {
    /// Sends an event whenever camera muted state changes. The `bool`
    /// represents whether the camera is muted or not.
    muted_txs: Vec<UnboundedSender<bool>>,
    external_publisher: ExternalEventPublisher,
}

impl CameraWatcherAgent {
    pub fn new(
        muted_txs: Vec<UnboundedSender<bool>>,
        external_publisher: ExternalEventPublisher,
    ) -> Self {
        Self { muted_txs, external_publisher }
    }

    pub async fn spawn(self, service_context: &ServiceContext) -> Result<(), Error> {
        // Multiple cameras are not yet supported individually, and individual camera mute/unmute
        // is not exposed, so all reported cameras are watched and aggregated together.
        let camera_device_clients =
            connect_to_cameras(service_context, self.external_publisher.clone())
                .await
                .context("connecting to camera")?;
        let mut event_handler =
            EventHandler::new(self.muted_txs.clone(), camera_device_clients.len());
        fasync::Task::local(async move {
            let id = fuchsia_trace::Id::new();
            // Here we don't care about hw_muted state because the input service would pick
            // up mute changes directly from the switch. We care about sw changes because
            // other clients of the camera3 service could change the sw mute state but not
            // notify the settings service.
            trace!(id, "camera_watcher_agent_handler");
            let mut mute_streams = futures::stream::select_all(
                camera_device_clients.into_iter().enumerate().map(|(index, client)| {
                    futures::stream::unfold(Some(client), move |client_opt| async move {
                        let client = client_opt?;
                        match client.watch_mute_state().await {
                            Ok((sw_muted, _hw_muted)) => {
                                Some(((index, CameraEvent::MuteState(sw_muted)), Some(client)))
                            }
                            Err(e) => {
                                log::warn!("Failed to watch mute state for camera {index}: {e:?}");
                                Some(((index, CameraEvent::Disconnected), None))
                            }
                        }
                    })
                    .boxed_local()
                }),
            );
            while let Some((index, event)) = mute_streams.next().await {
                trace!(id, "event");
                match event {
                    CameraEvent::MuteState(sw_muted) => {
                        event_handler.handle_event(index, sw_muted);
                    }
                    CameraEvent::Disconnected => {
                        event_handler.handle_disconnect(index);
                    }
                }
            }
            log::warn!("All camera mute state streams have closed");
        })
        .detach();

        Ok(())
    }
}

enum CameraEvent {
    MuteState(bool),
    Disconnected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CameraState {
    Unknown,
    Active(bool),
    Disconnected,
}

struct EventHandler {
    muted_txs: Vec<UnboundedSender<bool>>,
    sw_muted: Option<bool>,
    camera_states: Vec<CameraState>,
}

impl EventHandler {
    fn new(muted_txs: Vec<UnboundedSender<bool>>, num_cameras: usize) -> Self {
        Self { muted_txs, sw_muted: None, camera_states: vec![CameraState::Unknown; num_cameras] }
    }

    fn handle_event(&mut self, index: usize, sw_muted: bool) {
        if let Some(state) = self.camera_states.get_mut(index) {
            *state = CameraState::Active(sw_muted);
        }
        self.evaluate_and_send();
    }

    fn handle_disconnect(&mut self, index: usize) {
        if let Some(state) = self.camera_states.get_mut(index) {
            *state = CameraState::Disconnected;
        }
        self.evaluate_and_send();
    }

    fn evaluate_and_send(&mut self) {
        let mut has_connected = false;
        let mut has_unknown = false;
        let mut any_unmuted = false;

        for state in &self.camera_states {
            match state {
                CameraState::Active(false) => {
                    has_connected = true;
                    any_unmuted = true;
                }
                CameraState::Active(true) => {
                    has_connected = true;
                }
                CameraState::Unknown => {
                    has_connected = true;
                    has_unknown = true;
                }
                CameraState::Disconnected => {}
            }
        }

        let resolved_muted = if !has_connected {
            None
        } else if any_unmuted {
            Some(false)
        } else if !has_unknown {
            Some(true)
        } else {
            None
        };

        if let Some(all_muted) = resolved_muted
            && self.sw_muted != Some(all_muted)
        {
            self.sw_muted = Some(all_muted);
            self.send_event(all_muted);
        }
    }

    fn send_event(&self, muted: bool) {
        for muted_tx in &self.muted_txs {
            let _ = muted_tx.unbounded_send(muted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async::{MonotonicInstant, TestExecutor};
    use futures::channel::mpsc;
    use futures::lock::Mutex;
    use settings_camera::CAMERA_WATCHER_TIMEOUT;
    use settings_test_common::fakes::camera3_service::Camera3Service;
    use settings_test_common::fakes::service::ServiceRegistry;
    use settings_test_common::helpers::move_executor_forward_and_get;
    use std::rc::Rc;

    // Tests that the agent cannot start without a camera3 service.
    #[fuchsia::test(allow_stalls = false)]
    async fn when_camera3_inaccessible_returns_err() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        let agent = CameraWatcherAgent { muted_txs: vec![], external_publisher };
        let service_context =
            ServiceContext::new(Some(ServiceRegistry::serve(ServiceRegistry::create())));

        // Try to initiate the Service lifespan without providing the camera3 fidl interface.
        let result = agent.spawn(&service_context).await;
        assert!(result.is_err());
    }

    // Tests that events can be sent to the intended recipients.
    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_proxies_event() {
        let (tx1, mut rx1) = mpsc::unbounded();
        let (tx2, mut rx2) = mpsc::unbounded();

        let mut event_handler = EventHandler::new(vec![tx1.clone(), tx2.clone()], 1);

        // Send the events.
        event_handler.handle_event(0, true);

        let mut channel_received = 0;

        let mut next_rx1 = rx1.next();
        let mut next_rx2 = rx2.next();

        // Loop over the select so we can handle the messages as they come in. When all messages
        // have been handled, the senders a closed to ensure the complete case can be hit below.
        loop {
            futures::select! {
                event = next_rx1 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx1.close_channel();
                    channel_received += 1;
                }
                event = next_rx2 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx2.close_channel();
                    channel_received += 1;
                }
                complete => break,
            }
        }

        assert_eq!(channel_received, 2);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_aggregates_multiple_cameras() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 2);

        // Muting only camera 0 should not emit a muted event because camera 1 is still unmuted.
        event_handler.handle_event(0, true);
        assert!(rx.try_recv().is_err());

        // Muting camera 1 makes all cameras muted, emitting `true` once.
        event_handler.handle_event(1, true);
        assert_eq!(rx.next().await, Some(true));

        // Duplicate `true` on camera 0 does not emit another event.
        event_handler.handle_event(0, true);
        assert!(rx.try_recv().is_err());

        // Unmuting camera 1 immediately emits `false`.
        event_handler.handle_event(1, false);
        assert_eq!(rx.next().await, Some(false));

        // Unmuting camera 0 afterwards does not emit a duplicate `false`.
        event_handler.handle_event(0, false);
        assert!(rx.try_recv().is_err());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_forwards_first_unmuted_event_and_dedups() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 1);

        // The first resolved state is forwarded even when it is `false`.
        event_handler.handle_event(0, false);
        assert_eq!(rx.next().await, Some(false));

        // An identical follow-up event is not forwarded.
        event_handler.handle_event(0, false);
        assert!(rx.try_recv().is_err());

        // A change is forwarded.
        event_handler.handle_event(0, true);
        assert_eq!(rx.next().await, Some(true));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_handles_camera_disconnect() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 2);

        // Initial state: both cameras unmuted (`false`), emitting initial `false`.
        event_handler.handle_event(0, false);
        assert_eq!(rx.next().await, Some(false));
        event_handler.handle_event(1, false);
        assert!(rx.try_recv().is_err());

        // Disconnecting camera 0 while unmuted does not prevent camera 1 from muting the aggregate.
        event_handler.handle_disconnect(0);
        assert!(rx.try_recv().is_err());

        event_handler.handle_event(1, true);
        assert_eq!(rx.next().await, Some(true));

        // Disconnecting the last remaining camera does not emit a state change.
        event_handler.handle_disconnect(1);
        assert!(rx.try_recv().is_err());
    }

    struct FakeServices {
        camera3_service: Rc<Mutex<Camera3Service>>,
    }

    #[derive(PartialEq)]
    enum CameraDevice {
        With,
        Without,
    }

    #[derive(PartialEq)]
    enum DelayCamera {
        Yes,
        No,
    }

    // Returns a registry and input related services with which it is populated. If delay_camera_device
    // is true, then has_camera_device is ignored. It sends back the camera device after a delay.
    // Otherwise, if has_camera_device is true, it will immediately respond with the populated camera
    // device. If has_camera_device is false, it will immediately respond with an empty device list.
    async fn create_services(
        has_camera_device: CameraDevice,
        delay_camera_device: DelayCamera,
    ) -> (Rc<Mutex<ServiceRegistry>>, FakeServices) {
        let service_registry = ServiceRegistry::create();

        let camera3_service_handle =
            Rc::new(Mutex::new(if DelayCamera::Yes == delay_camera_device {
                Camera3Service::new_delayed_devices(delay_camera_device == DelayCamera::Yes)
            } else {
                Camera3Service::new(has_camera_device == CameraDevice::With)
            }));
        service_registry.lock().await.register_service(camera3_service_handle.clone());

        (service_registry, FakeServices { camera3_service: camera3_service_handle })
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_camera_agent_proxy() {
        // Setup the fake services.
        let (service_registry, fake_services) =
            create_services(CameraDevice::With, DelayCamera::No).await;

        let expected_camera_state = true;
        fake_services.camera3_service.lock().await.set_camera_sw_muted(expected_camera_state);
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let (tx, mut rx) = mpsc::unbounded();

        let agent = CameraWatcherAgent::new(vec![tx], external_publisher);
        let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));
        let res = agent.spawn(&service_context).await;

        // Validate that the setup is complete.
        assert!(matches!(res, Ok(())), "agent spawn failed");

        // Track the events to make sure they came in.
        let res = rx.next().await;
        assert_eq!(res, Some(true));
    }

    // Tests that an error is returned if the camera watcher cannot find a camera device
    // after the timeout is reached.
    #[fuchsia::test]
    fn test_camera_devices_watcher_timeout() {
        // Custom executor for this test so that we can advance the clock arbitrarily and verify the
        // state of the executor at any given point.
        let mut executor = TestExecutor::new_with_fake_time();
        executor.set_fake_time(MonotonicInstant::from_nanos(0));

        // Setup the fake services.
        let services_future = create_services(CameraDevice::Without, DelayCamera::No);
        let (service_registry, fake_services) = move_executor_forward_and_get(
            &mut executor,
            services_future,
            "Could not create services",
        );

        // Mute the camera via software.
        let camera_service_future = fake_services.camera3_service.lock();
        move_executor_forward_and_get(
            &mut executor,
            camera_service_future,
            "Unable to get camera service",
        )
        .set_camera_sw_muted(true);

        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let agent = CameraWatcherAgent::new(vec![], external_publisher);
        let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));

        // Create and send the invocation with faked services.
        let spawn_future = agent.spawn(&service_context);

        // Advance time past the timeout.
        executor.set_fake_time(MonotonicInstant::from_nanos(CAMERA_WATCHER_TIMEOUT + 1));

        let res =
            move_executor_forward_and_get(&mut executor, spawn_future, "Could not complete spawn");

        // Validate that the setup is complete.
        assert!(res.is_err(), "spawn did not hit timeout");
    }

    // Tests that the camera agent is able to handle an empty device list first, and then
    // a second update with the device in it that comes in before the timeout.
    #[fuchsia::test]
    async fn test_camera_agent_delayed_devices() {
        let (service_registry, fake_services) =
            create_services(CameraDevice::Without, DelayCamera::Yes).await;

        let expected_camera_state = true;
        fake_services.camera3_service.lock().await.set_camera_sw_muted(expected_camera_state);
        let (tx, mut rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let agent = CameraWatcherAgent::new(vec![tx], external_publisher);
        let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));
        let res = agent.spawn(&service_context).await;

        // Validate that the setup is complete.
        assert!(matches!(res, Ok(())), "spawn failed");
        let muted = rx.next().await;
        assert_eq!(muted, Some(true));
    }
}
