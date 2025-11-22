// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::audio::build_audio_default_settings;
use crate::audio::types::{AudioInfo, AudioStreamType};
#[cfg(test)]
use crate::audio::{StreamVolumeControl, create_default_audio_stream};
use crate::clock;
use crate::tests::fakes::audio_core_service;
use fuchsia_inspect::component;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::lock::Mutex;
use settings_common::inspect::config_logger::InspectConfigLogger;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::{ExternalServiceEvent, ServiceContext};
use settings_test_common::fakes::service::ServiceRegistry;
use std::rc::Rc;

fn default_audio_info() -> AudioInfo {
    let config_logger =
        Rc::new(std::sync::Mutex::new(InspectConfigLogger::new(component::inspector().root())));
    let mut audio_configuration = build_audio_default_settings(config_logger);
    audio_configuration
        .load_default_value()
        .expect("config should exist and parse for test")
        .unwrap()
}

// Returns a registry populated with the AudioCore service.
async fn create_service() -> Rc<Mutex<ServiceRegistry>> {
    let service_registry = ServiceRegistry::create();
    let audio_core_service_handle = audio_core_service::Builder::new(default_audio_info())
        .set_suppress_client_errors(true)
        .build();
    service_registry.lock().await.register_service(audio_core_service_handle.clone());
    service_registry
}

// Tests that the volume event stream thread exits when the StreamVolumeControl is deleted.
#[fuchsia::test(allow_stalls = false)]
async fn test_drop_thread() {
    let service_context = ServiceContext::new(Some(ServiceRegistry::serve(create_service().await)));
    let (event_tx, _) = mpsc::unbounded();
    let external_publisher = ExternalEventPublisher::new(event_tx);

    let audio_proxy = service_context
        .connect_with_publisher::<fidl_fuchsia_media::AudioCoreMarker, _>(external_publisher)
        .await
        .expect("service should be present");

    let (event_tx, mut event_rx) = mpsc::unbounded();
    let _ = StreamVolumeControl::create(
        0.into(),
        audio_proxy,
        create_default_audio_stream(AudioStreamType::Media),
        None,
        Some(event_tx),
    )
    .await;
    let req = "unknown";
    let req_timestamp = "unknown";
    let resp_timestamp = clock::inspect_format_now();

    assert_eq!(
        event_rx.next().await.expect("First message should have been the closed event"),
        ExternalServiceEvent::Closed(
            "volume_control_events",
            req.into(),
            req_timestamp.into(),
            resp_timestamp.into(),
        )
    );
}

// Ensures that the StreamVolumeControl properly fires the provided early exit
// closure when the underlying AudioCoreService closes unexpectedly.
#[fuchsia::test(allow_stalls = false)]
async fn test_detect_early_exit() {
    let service_registry = ServiceRegistry::create();
    let audio_core_service_handle = audio_core_service::Builder::new(default_audio_info())
        .set_suppress_client_errors(true)
        .build();
    service_registry.lock().await.register_service(audio_core_service_handle.clone());

    let service_context = ServiceContext::new(Some(ServiceRegistry::serve(service_registry)));
    let (event_tx, _) = mpsc::unbounded();
    let external_publisher = ExternalEventPublisher::new(event_tx);

    let audio_proxy = service_context
        .connect_with_publisher::<fidl_fuchsia_media::AudioCoreMarker, _>(external_publisher)
        .await
        .expect("proxy should be present");
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();

    // Create StreamVolumeControl, specifying firing an event as the early exit
    // action. Note that we must store the returned value or else the normal
    // drop behavior will clean up it before the AudioCoreService's exit can
    // be detected.
    let _stream_volume_control = StreamVolumeControl::create(
        0.into(),
        audio_proxy,
        create_default_audio_stream(AudioStreamType::Media),
        Some(Rc::new(move || {
            tx.unbounded_send(()).unwrap();
        })),
        None,
    )
    .await
    .expect("should successfully build");

    // Trigger AudioCoreService exit.
    audio_core_service_handle.lock().await.exit();

    // Check to make sure early exit event was received.
    assert!(matches!(rx.next().await, Some(..)));
}
