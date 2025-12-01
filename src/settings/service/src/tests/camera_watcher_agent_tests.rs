// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::camera_watcher::CameraWatcherAgent;
use fuchsia_async::{MonotonicInstant, TestExecutor};
use futures::StreamExt;
use futures::channel::mpsc;
use futures::lock::Mutex;
use settings_camera::CAMERA_WATCHER_TIMEOUT;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::ServiceContext;
use settings_test_common::fakes::camera3_service::Camera3Service;
use settings_test_common::fakes::service::ServiceRegistry;
use settings_test_common::helpers::move_executor_forward_and_get;
use std::rc::Rc;

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

    let camera3_service_handle = Rc::new(Mutex::new(if DelayCamera::Yes == delay_camera_device {
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
    let (service_registry, fake_services) =
        move_executor_forward_and_get(&mut executor, services_future, "Could not create services");

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
    assert!(matches!(res, Err(_)), "spawn did not hit timeout");
}

// Tests that the camera agent is able to handle an empty device list first, and then
// a second update with the device in it that comes in before the timeout.
#[fuchsia_async::run_singlethreaded(test)]
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
