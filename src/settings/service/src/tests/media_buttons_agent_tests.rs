// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::media_buttons;
use crate::tests::fakes::input_device_registry_service::InputDeviceRegistryService;
use fidl_fuchsia_ui_input::MediaButtonsEvent;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::lock::Mutex;
use media_buttons::MediaButtonsAgent;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::ServiceContext;
use settings_media_buttons::{Event, MediaButtons};
use settings_test_common::fakes::service::ServiceRegistry;
use std::rc::Rc;

struct FakeServices {
    input_device_registry: Rc<Mutex<InputDeviceRegistryService>>,
}

// Returns a registry and input related services with which it is populated.
async fn create_services() -> (Rc<Mutex<ServiceRegistry>>, FakeServices) {
    let service_registry = ServiceRegistry::create();

    let input_device_registry_service_handle =
        Rc::new(Mutex::new(InputDeviceRegistryService::new()));
    service_registry.lock().await.register_service(input_device_registry_service_handle.clone());

    (service_registry, FakeServices { input_device_registry: input_device_registry_service_handle })
}

#[fuchsia::test(allow_stalls = false)]
async fn test_media_buttons_proxied() {
    let (event_tx, _) = mpsc::unbounded();
    let external_publisher = ExternalEventPublisher::new(event_tx);
    let (tx, mut rx) = mpsc::unbounded();
    let agent = MediaButtonsAgent::new(vec![tx], external_publisher);

    // Setup the fake services.
    let (service_registry, fake_services) = create_services().await;
    let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));

    let res = agent.spawn(&service_context).await;
    // Validate that the setup is complete.
    assert!(matches!(res, Ok(())), "spawn failed");

    // The agent should now be initialized. Send a media button event.
    fake_services
        .input_device_registry
        .lock()
        .await
        .send_media_button_event(MediaButtonsEvent {
            volume: Some(1),
            mic_mute: Some(true),
            pause: None,
            camera_disable: None,
            ..Default::default()
        })
        .await;

    let mic_mute = rx.next().await;
    assert_eq!(
        mic_mute,
        Some(Event::OnButton(MediaButtons { mic_mute: Some(true), camera_disable: None }))
    );
}
