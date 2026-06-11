// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl::endpoints::ServiceMarker as _;
use fidl_fuchsia_component_test as ftest;
use fidl_fuchsia_driver_test as fdt;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_test_hardwarepowercontrol as ftest_battery;

use fidl_fuchsia_power_battery as fpower;
use fidl_fuchsia_power_battery_test as spower;
use fuchsia_async as fasync;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::StreamExt as _;
use test_case::test_case;
use test_util::assert_gt;
use zx;

/// Dictates which battery FIDL protocols are routed from the DriverTestRealm
/// to the battery manager under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FidlRouteMode {
    /// Route only the new `fuchsia.hardware.power.battery` protocols.
    NewOnly,
    /// Route only the old `fuchsia.power.battery` protocols.
    OldOnly,
    /// Route both old and new battery protocols.
    Both,
}

const BATTERY_MANAGER_URL: &str = "#meta/battery_manager.cm";

async fn setup_realm(mode: FidlRouteMode) -> Result<RealmInstance> {
    let builder = RealmBuilder::new().await?;
    builder.driver_test_realm_setup().await?;

    let mut dtr_exposes = vec![];
    if mode == FidlRouteMode::NewOnly || mode == FidlRouteMode::Both {
        dtr_exposes.push(ftest::Capability::Service(ftest::Service {
            name: Some(fbattery::ServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }));
        dtr_exposes.push(ftest::Capability::Service(ftest::Service {
            name: Some(ftest_battery::ServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }));
    }
    if mode == FidlRouteMode::OldOnly || mode == FidlRouteMode::Both {
        dtr_exposes.push(ftest::Capability::Service(ftest::Service {
            name: Some(fpower::InfoServiceMarker::SERVICE_NAME.to_string()),
            ..Default::default()
        }));
    }
    builder.driver_test_realm_add_dtr_exposes(&dtr_exposes).await?;

    let battery_manager =
        builder.add_child("battery_manager", BATTERY_MANAGER_URL, ChildOptions::new()).await?;

    // Route LogSink to battery_manager
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&battery_manager),
        )
        .await?;

    // Route storage and configuration
    builder
        .add_route(
            Route::new()
                .capability(Capability::storage("data"))
                .capability(Capability::storage("tmp"))
                .from(Ref::parent())
                .to(&battery_manager),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::configuration("fuchsia.power.SuspendEnabled"))
                .from(Ref::void())
                .to(&battery_manager),
        )
        .await?;

    // Route driver services to battery_manager
    let mut dtr_route = Route::new();
    if mode == FidlRouteMode::NewOnly || mode == FidlRouteMode::Both {
        dtr_route = dtr_route.capability(Capability::service::<fbattery::ServiceMarker>());
    }
    if mode == FidlRouteMode::OldOnly || mode == FidlRouteMode::Both {
        dtr_route = dtr_route.capability(Capability::service::<fpower::InfoServiceMarker>());
    }
    builder
        .add_route(
            dtr_route.from(Ref::child(fuchsia_driver_test::COMPONENT_NAME)).to(&battery_manager),
        )
        .await?;

    // Expose Control Service from DTR to parent
    if mode == FidlRouteMode::NewOnly || mode == FidlRouteMode::Both {
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<ftest_battery::ServiceMarker>())
                    .from(Ref::child(fuchsia_driver_test::COMPONENT_NAME))
                    .to(Ref::parent()),
            )
            .await?;
    }

    // Expose BatteryManager and BatterySimulator to the test runner
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fpower::BatteryManagerMarker>())
                .capability(Capability::protocol::<spower::BatterySimulatorMarker>())
                .from(&battery_manager)
                .to(Ref::parent()),
        )
        .await?;

    let realm = builder.build().await?;

    realm
        .driver_test_realm_start(fdt::RealmArgs {
            root_driver: Some("fuchsia-boot:///platform-bus#meta/platform-bus.cm".to_owned()),
            dtr_exposes: Some(dtr_exposes),
            software_devices: Some(vec![fdt::SoftwareDevice {
                device_name: "fake-battery".to_string(),
                device_id: bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_FAKE_BATTERY,
            }]),
            ..Default::default()
        })
        .await?;

    Ok(realm)
}

fn assert_default_battery_info(info: &fpower::BatteryInfo) {
    assert_eq!(info.level_percent, Some(ftest_battery::DEFAULT_ROUNDED_LEVEL_PERCENT as f32));
    assert_eq!(info.charge_status, Some(fpower::ChargeStatus::Charging));
    assert_eq!(info.charge_source, Some(fpower::ChargeSource::AcAdapter));
    assert_eq!(info.present_voltage_mv, Some(ftest_battery::DEFAULT_PRESENT_VOLTAGE_MV));
    assert_eq!(info.remaining_charge_uah, Some(ftest_battery::DEFAULT_REMAINING_CHARGE_UAH));
    assert!(info.timestamp.is_some());
}

async fn wait_for_battery_info(
    mut watcher_stream: fpower::BatteryInfoWatcherRequestStream,
) -> Result<(fpower::BatteryInfo, fpower::BatteryInfoWatcherRequestStream)> {
    while let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
        info,
        responder,
        ..
    })) = watcher_stream.next().await
    {
        responder.send()?;
        if info.level_percent.is_some() && info.status != Some(fpower::BatteryStatus::NotAvailable)
        {
            return Ok((info, watcher_stream));
        }
    }
    Err(anyhow::anyhow!("Watcher stream ended without receiving valid battery info"))
}

#[test_case(FidlRouteMode::NewOnly; "new_only")]
#[test_case(FidlRouteMode::OldOnly; "old_only")]
#[test_case(FidlRouteMode::Both; "both")]
#[fuchsia::test]
async fn test_get_battery_info(mode: FidlRouteMode) -> Result<()> {
    let realm = setup_realm(mode).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (info, _stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);

    let get_info = battery_mgr.get_battery_info().await?;
    assert_default_battery_info(&get_info);
    Ok(())
}

#[fuchsia::test]
async fn test_watcher() -> Result<()> {
    let realm = setup_realm(FidlRouteMode::Both).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    let (info, _stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);
    Ok(())
}

#[fuchsia::test]
async fn test_simulator() -> Result<()> {
    let realm = setup_realm(FidlRouteMode::Both).await?;
    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let simulator: spower::BatterySimulatorProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from DTR driver first
    let (_info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;

    // Now disconnect real battery to trigger simulation mode
    simulator.disconnect_real_battery()?;

    // Wait for the simulation mode update to be propagated
    if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo { responder, .. })) =
        watcher_stream.next().await
    {
        responder.send()?;
    }

    // Now push simulated updates
    simulator.set_battery_percentage(50.0)?;
    simulator.set_charge_status(fpower::ChargeStatus::Discharging)?;

    // We should receive the updated battery info callback
    loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.level_percent == Some(50.0)
                && info.charge_status == Some(fpower::ChargeStatus::Discharging)
            {
                break;
            }
        } else {
            panic!("Watcher stream ended before receiving expected simulated updates");
        }
    }
    Ok(())
}

#[fuchsia::test]
async fn test_watch_dynamic_updates() -> Result<()> {
    let realm = setup_realm(FidlRouteMode::NewOnly).await?;

    let battery_mgr: fpower::BatteryManagerProxy =
        realm.root.connect_to_protocol_at_exposed_dir()?;
    let service = fuchsia_component::client::Service::open_from_dir(
        realm.root.get_exposed_dir(),
        ftest_battery::ServiceMarker,
    )?;
    let service_instance = service.watch_for_any().await?;
    let control = service_instance.connect_to_control()?;

    let (watcher_client, watcher_stream) =
        fidl::endpoints::create_request_stream::<fpower::BatteryInfoWatcherMarker>();
    battery_mgr.watch(watcher_client)?;

    // Wait for the initial update from driver
    let (info, mut watcher_stream) = wait_for_battery_info(watcher_stream).await?;
    assert_default_battery_info(&info);
    let t0 = info.timestamp.expect("timestamp missing from default info");

    // Sleep for 10 seconds so the rate limiter has enough time to allow the level to rise
    // from default 99% to 100.0% (requires 7.5s).
    fasync::Timer::new(fasync::MonotonicInstant::after(zx::Duration::from_seconds(10))).await;

    // Now update fake battery using driver Control.
    // Raw level 99.1% maps to 100.0% scaled level after processing.
    control
        .set_battery_status(&fbattery::Status {
            level_percent: Some(99.1),
            charge_status: Some(fbattery::ChargeStatus::Charging),
            ..Default::default()
        })
        .await?;

    // Wait for the update on watcher stream
    let t1 = loop {
        if let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
            info,
            responder,
            ..
        })) = watcher_stream.next().await
        {
            responder.send()?;
            if info.level_percent == Some(100.0)
                && info.charge_status == Some(fpower::ChargeStatus::Charging)
            {
                break info.timestamp.expect("timestamp missing from first update");
            }
        } else {
            panic!("Watcher stream ended before receiving expected driver updates");
        }
    };
    assert_gt!(t1, t0);

    Ok(())
}
