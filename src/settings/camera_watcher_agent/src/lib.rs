// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl_fuchsia_camera3::{DeviceProxy, WatchDevicesEvent};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use futures::stream::{LocalBoxStream, SelectAll};
use settings_camera::{
    CameraWatcherEvent, connect_to_camera, connect_to_cameras_with_watcher, watch_camera_changes,
};
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::ServiceContext;
use settings_common::trace;
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct CameraWatcherAgent {
    /// Sends an event whenever the aggregate camera muted state changes, or when a camera is
    /// connected after startup.
    muted_txs: Vec<UnboundedSender<CameraWatcherEvent>>,
    external_publisher: ExternalEventPublisher,
}

impl CameraWatcherAgent {
    pub fn new(
        muted_txs: Vec<UnboundedSender<CameraWatcherEvent>>,
        external_publisher: ExternalEventPublisher,
    ) -> Self {
        Self { muted_txs, external_publisher }
    }

    pub async fn spawn(self, service_context: &ServiceContext) -> Result<(), Error> {
        // Multiple cameras are not yet supported individually, and individual camera mute/unmute
        // is not exposed, so all reported cameras are watched and aggregated together.
        let (camera_watcher_proxy, cameras) =
            connect_to_cameras_with_watcher(service_context, self.external_publisher.clone())
                .await
                .context("connecting to camera")?;
        let camera_watcher_proxy = Rc::new(camera_watcher_proxy);
        let mut event_handler =
            EventHandler::new(self.muted_txs.clone(), cameras.iter().map(|(id, _)| *id));
        fasync::Task::local(async move {
            let id = fuchsia_trace::Id::new();
            // Here we don't care about hw_muted state because the input service would pick
            // up mute changes directly from the switch. We care about sw changes because
            // other clients of the camera3 service could change the sw mute state but not
            // notify the settings service.
            trace!(id, "camera_watcher_agent_handler");
            let mut mute_streams: SelectAll<_> = cameras
                .into_iter()
                .map(|(camera_id, client)| watch_mute_state(camera_id, 0, client))
                .collect();

            // Cameras that are added after startup, for example because they were hot-plugged
            // or their device restarted, are watched as well.
            let mut device_changes =
                futures::stream::unfold(Rc::clone(&camera_watcher_proxy), |proxy| async move {
                    match watch_camera_changes(&proxy).await {
                        Ok(events) => Some((events, proxy)),
                        Err(e) => {
                            log::warn!("Stopped watching for camera changes: {e:?}");
                            None
                        }
                    }
                })
                .boxed_local()
                .fuse();

            loop {
                futures::select! {
                    event = mute_streams.next() => {
                        let Some((camera_id, connection_id, event)) = event else {
                            continue;
                        };
                        trace!(id, "event");
                        match event {
                            CameraEvent::MuteState(sw_muted) => {
                                event_handler.handle_event(camera_id, connection_id, sw_muted);
                            }
                            CameraEvent::Disconnected => {
                                event_handler.handle_disconnect(camera_id, connection_id);
                            }
                        }
                    }
                    events = device_changes.next() => {
                        let Some(events) = events else {
                            continue;
                        };
                        for event in events {
                            match event {
                                WatchDevicesEvent::Added(camera_id) => {
                                    match connect_to_camera(&camera_watcher_proxy, camera_id) {
                                        Ok(client) => {
                                            let connection_id =
                                                event_handler.handle_added(camera_id);
                                            mute_streams.push(watch_mute_state(
                                                camera_id,
                                                connection_id,
                                                client,
                                            ));
                                        }
                                        Err(e) => {
                                            log::warn!(
                                                "Failed to connect to added camera {camera_id}: \
                                                {e:?}"
                                            );
                                        }
                                    }
                                }
                                WatchDevicesEvent::Removed(camera_id) => {
                                    event_handler.handle_removed(camera_id);
                                }
                                WatchDevicesEvent::Existing(_) => {}
                            }
                        }
                    }
                    complete => break,
                }
            }
            log::warn!("All camera mute state streams have closed");
        })
        .detach();

        Ok(())
    }
}

/// Returns a stream of mute state events for the camera with the given id and connection
/// generation. The stream ends after reporting that the camera disconnected.
fn watch_mute_state(
    camera_id: u64,
    connection_id: u64,
    client: DeviceProxy,
) -> LocalBoxStream<'static, (u64, u64, CameraEvent)> {
    futures::stream::unfold(Some(client), move |client_opt| async move {
        let client = client_opt?;
        match client.watch_mute_state().await {
            Ok((sw_muted, _hw_muted)) => {
                Some(((camera_id, connection_id, CameraEvent::MuteState(sw_muted)), Some(client)))
            }
            Err(e) => {
                log::warn!("Failed to watch mute state for camera {camera_id}: {e:?}");
                Some(((camera_id, connection_id, CameraEvent::Disconnected), None))
            }
        }
    })
    .boxed_local()
}

enum CameraEvent {
    MuteState(bool),
    Disconnected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CameraState {
    Unknown,
    Active(bool),
}

struct CameraEntry {
    connection_id: u64,
    state: CameraState,
}

struct EventHandler {
    muted_txs: Vec<UnboundedSender<CameraWatcherEvent>>,
    sw_muted: Option<bool>,
    camera_states: BTreeMap<u64, CameraEntry>,
    next_connection_id: u64,
}

impl EventHandler {
    fn new(
        muted_txs: Vec<UnboundedSender<CameraWatcherEvent>>,
        camera_ids: impl IntoIterator<Item = u64>,
    ) -> Self {
        Self {
            muted_txs,
            sw_muted: None,
            camera_states: camera_ids
                .into_iter()
                .map(|id| (id, CameraEntry { connection_id: 0, state: CameraState::Unknown }))
                .collect(),
            next_connection_id: 1,
        }
    }

    #[cfg(test)]
    fn is_watching(&self, camera_id: u64) -> bool {
        self.camera_states.contains_key(&camera_id)
    }

    fn handle_event(&mut self, camera_id: u64, connection_id: u64, sw_muted: bool) {
        let Some(entry) = self.camera_states.get_mut(&camera_id) else {
            return;
        };
        if entry.connection_id != connection_id {
            return;
        }
        entry.state = CameraState::Active(sw_muted);
        self.evaluate_and_send();
    }

    fn handle_disconnect(&mut self, camera_id: u64, connection_id: u64) {
        if self.camera_states.get(&camera_id).is_some_and(|e| e.connection_id == connection_id) {
            let _ = self.camera_states.remove(&camera_id);
            self.evaluate_and_send();
        }
    }

    fn handle_removed(&mut self, camera_id: u64) {
        if self.camera_states.remove(&camera_id).is_some() {
            self.evaluate_and_send();
        }
    }

    /// Starts tracking a camera that was connected after startup and returns its connection
    /// generation. Recipients are told about the new camera first, and the next resolved aggregate
    /// state is always sent afterwards, even if it matches the last state sent, so that recipients
    /// can apply their state to the new camera.
    fn handle_added(&mut self, camera_id: u64) -> u64 {
        let connection_id = self.next_connection_id;
        self.next_connection_id += 1;
        let _ = self
            .camera_states
            .insert(camera_id, CameraEntry { connection_id, state: CameraState::Unknown });
        self.sw_muted = None;
        self.send_event(CameraWatcherEvent::DeviceAdded);
        connection_id
    }

    fn evaluate_and_send(&mut self) {
        let mut has_connected = false;
        let mut has_unknown = false;
        let mut any_unmuted = false;

        for entry in self.camera_states.values() {
            match entry.state {
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
            self.send_event(CameraWatcherEvent::SoftwareMuted(all_muted));
        }
    }

    fn send_event(&self, event: CameraWatcherEvent) {
        for muted_tx in &self.muted_txs {
            let _ = muted_tx.unbounded_send(event);
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

        let mut event_handler = EventHandler::new(vec![tx1.clone(), tx2.clone()], 0..1);

        // Send the events.
        event_handler.handle_event(0, 0, true);

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
                    assert_eq!(muted, CameraWatcherEvent::SoftwareMuted(true));
                    // Close channel so we can exit select loop.
                    tx1.close_channel();
                    channel_received += 1;
                }
                event = next_rx2 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert_eq!(muted, CameraWatcherEvent::SoftwareMuted(true));
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
        let mut event_handler = EventHandler::new(vec![tx], 0..2);

        // Muting only camera 0 should not emit a muted event because camera 1 is still unmuted.
        event_handler.handle_event(0, 0, true);
        assert!(rx.try_recv().is_err());

        // Muting camera 1 makes all cameras muted, emitting `true` once.
        event_handler.handle_event(1, 0, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));

        // Duplicate `true` on camera 0 does not emit another event.
        event_handler.handle_event(0, 0, true);
        assert!(rx.try_recv().is_err());

        // Unmuting camera 1 immediately emits `false`.
        event_handler.handle_event(1, 0, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));

        // Unmuting camera 0 afterwards does not emit a duplicate `false`.
        event_handler.handle_event(0, 0, false);
        assert!(rx.try_recv().is_err());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_forwards_first_unmuted_event_and_dedups() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 0..1);

        // The first resolved state is forwarded even when it is `false`.
        event_handler.handle_event(0, 0, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));

        // An identical follow-up event is not forwarded.
        event_handler.handle_event(0, 0, false);
        assert!(rx.try_recv().is_err());

        // A change is forwarded.
        event_handler.handle_event(0, 0, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_handles_camera_disconnect() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 0..2);

        // Initial state: both cameras unmuted (`false`), emitting initial `false`.
        event_handler.handle_event(0, 0, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));
        event_handler.handle_event(1, 0, false);
        assert!(rx.try_recv().is_err());

        // Disconnecting camera 0 while unmuted does not prevent camera 1 from muting the aggregate.
        event_handler.handle_disconnect(0, 0);
        assert!(rx.try_recv().is_err());

        event_handler.handle_event(1, 0, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));

        // Disconnecting the last remaining camera does not emit a state change.
        event_handler.handle_disconnect(1, 0);
        assert!(rx.try_recv().is_err());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_reports_added_camera() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut event_handler = EventHandler::new(vec![tx], 0..1);
        event_handler.handle_event(0, 0, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));

        // An added camera is reported first, and the aggregate is not resolved until it reports.
        let conn1 = event_handler.handle_added(1);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::DeviceAdded));
        assert!(rx.try_recv().is_err());

        // The next resolved state is sent even though it matches the last state sent.
        event_handler.handle_event(1, conn1, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));
        event_handler.handle_event(1, conn1, true);
        assert!(rx.try_recv().is_err());

        // A removed camera no longer contributes to the aggregate, and a late event from its
        // stream cannot add it back.
        event_handler.handle_event(1, conn1, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));
        assert!(event_handler.is_watching(1));
        event_handler.handle_disconnect(1, conn1);
        assert!(!event_handler.is_watching(1));
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));
        event_handler.handle_event(1, conn1, false);
        assert!(rx.try_recv().is_err());

        // If a camera with the same id is removed and re-added before the old connection's stream
        // ends, the old connection's disconnect does not remove the new connection.
        event_handler.handle_removed(0);
        let conn0_restarted = event_handler.handle_added(0);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::DeviceAdded));
        event_handler.handle_disconnect(0, 0);
        assert!(event_handler.is_watching(0));
        event_handler.handle_event(0, conn0_restarted, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));

        // If a camera with the same id is added before its old connection's stream reports a
        // disconnect, the new connection replaces the old one and the old disconnect is ignored.
        let conn0_restarted_again = event_handler.handle_added(0);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::DeviceAdded));
        event_handler.handle_disconnect(0, conn0_restarted);
        assert!(event_handler.is_watching(0));
        event_handler.handle_event(0, conn0_restarted_again, true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));
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
        assert_eq!(res, Some(CameraWatcherEvent::SoftwareMuted(true)));
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
        assert_eq!(muted, Some(CameraWatcherEvent::SoftwareMuted(true)));
    }

    // Tests that a camera added after startup is reported and watched.
    #[fuchsia::test(allow_stalls = false)]
    async fn test_camera_agent_watches_added_camera() {
        let (service_registry, fake_services) =
            create_services(CameraDevice::With, DelayCamera::No).await;
        fake_services.camera3_service.lock().await.set_camera_sw_muted(true);
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let (tx, mut rx) = mpsc::unbounded();

        let agent = CameraWatcherAgent::new(vec![tx], external_publisher);
        let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));
        let res = agent.spawn(&service_context).await;
        assert!(matches!(res, Ok(())), "agent spawn failed");
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));

        // A camera is added unmuted. It is reported, and the aggregate mute state becomes false.
        fake_services.camera3_service.lock().await.add_camera_device(2, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::DeviceAdded));
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));

        // Removing the unmuted camera restores the aggregate mute state from the remaining camera.
        fake_services.camera3_service.lock().await.remove_camera_device(2);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));

        // A camera that restarts with the same id is watched on its new connection, and mute
        // changes on it keep being reported.
        fake_services.camera3_service.lock().await.remove_camera_device(1);
        fake_services.camera3_service.lock().await.add_camera_device(1, false);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::DeviceAdded));
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(false)));

        fake_services.camera3_service.lock().await.set_camera_sw_muted(true);
        assert_eq!(rx.next().await, Some(CameraWatcherEvent::SoftwareMuted(true)));
    }
}
