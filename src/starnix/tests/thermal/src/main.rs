// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use component_events::events::{EventStream, ExitStatus, Stopped};
use component_events::matcher::EventMatcher;
use fidl_fuchsia_power_battery as fbattery;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmBuilderParams, Ref, Route,
};
use log::info;
mod fake_battery;
use {fidl_fuchsia_power_cpu as fcpu, fidl_fuchsia_thermal as fthermal};

#[fuchsia::main]
async fn main() {
    let mut events = EventStream::open().await.unwrap();
    let builder = RealmBuilder::with_params(
        RealmBuilderParams::new()
            .realm_name("thermal_test")
            .from_relative_url("#meta/container_with_thermal_client.cm"),
    )
    .await
    .unwrap();

    let sensor_manager_child = builder
        .add_child(
            "sensor_manager",
            "fake-thermal-sensor-manager#meta/fake-thermal-sensor-manager.cm",
            ChildOptions::new(),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&sensor_manager_child),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fthermal::SensorManagerMarker>())
                .from(&sensor_manager_child)
                .to(Ref::child("kernel")),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fthermal::SensorManagerMarker>())
                .from(&sensor_manager_child)
                .to(Ref::parent()),
        )
        .await
        .unwrap();

    let battery_charger_enable_requests = fake_battery::ChargerEnableRequests::default();
    let battery_charger_mock = builder
        .add_local_child(
            "battery_charger",
            {
                let enable_requests = battery_charger_enable_requests.clone();
                move |handles: LocalComponentHandles| {
                    Box::pin(fake_battery::mock_charger_service(handles, enable_requests.clone()))
                }
            },
            ChildOptions::new(),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::service::<fbattery::ChargerServiceMarker>())
                .from(&battery_charger_mock)
                .to(Ref::child("kernel")),
        )
        .await
        .unwrap();

    let domain_controller_child = builder
        .add_child(
            "domain_controller",
            "fake-domain-controller#meta/fake-domain-controller.cm",
            ChildOptions::new(),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&domain_controller_child),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fcpu::DomainControllerMarker>())
                .from(&domain_controller_child)
                .to(Ref::child("kernel")),
        )
        .await
        .unwrap();

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fcpu::DomainControllerMarker>())
                .from(&domain_controller_child)
                .to(Ref::parent()),
        )
        .await
        .unwrap();

    info!("starting realm");
    let instance = builder.build().await.unwrap();

    let realm_moniker = format!("realm_builder:{}", instance.root.child_name());
    info!(realm_moniker:%; "started");
    let thermal_client_moniker = format!("{realm_moniker}/thermal_client");

    info!(thermal_client_moniker:%; "waiting for thermal_client to exit");
    let stopped = EventMatcher::ok()
        .moniker(&thermal_client_moniker)
        .wait::<Stopped>(&mut events)
        .await
        .unwrap();
    let status = stopped.result().unwrap().status;
    info!(status:?; "thermal_client stopped");
    assert_eq!(status, ExitStatus::Clean);

    let battery_charger_enable_requests = battery_charger_enable_requests.lock().unwrap();
    assert_eq!(*battery_charger_enable_requests, vec![false, true]);
}
