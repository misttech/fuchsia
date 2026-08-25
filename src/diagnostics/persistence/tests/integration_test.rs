// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use diagnostics_data::InspectData;
use diagnostics_hierarchy::SelectResult;
use fidl::endpoints::Proxy;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_diagnostics_persistence as fdiagnostics_persistence;
use fidl_fuchsia_logger as flogger;
use fidl_fuchsia_power_battery as fbattery;
use fidl_fuchsia_sys2 as fsys2;
use fidl_fuchsia_update as fupdate;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use futures::channel::mpsc;
use futures::{FutureExt, SinkExt, StreamExt, TryStreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn extract_counter(content: &str) -> Option<u64> {
    let data_vec: Vec<InspectData> = serde_json::from_str(content).ok()?;
    let selector = selectors::parse_verbose("*:root:counter").ok()?;

    for data in data_vec {
        if data.moniker.to_string().contains("publisher") {
            let prop = diagnostics_hierarchy::select_from_hierarchy(
                data.payload.as_ref().unwrap(),
                &selector,
            )
            .unwrap();
            let SelectResult::Properties(props) = prop else {
                panic!("malformed/unexpected test data")
            };
            assert_eq!(props.len(), 1);
            return props[0].uint();
        }
    }
    None
}

async fn make_realm(
    interval: i64,
    mock_battery_tx: Option<mpsc::Sender<fbattery::BatteryInfoWatcherProxy>>,
    mock_update_tx: Option<mpsc::Sender<fupdate::NotifierProxy>>,
) -> Result<RealmInstance, Error> {
    let skip_update_check = mock_update_tx.is_none();
    let builder = RealmBuilder::new().await?;

    let archivist = builder
        .add_child("archivist", "#meta/archivist-for-embedding.cm", ChildOptions::new().eager())
        .await?;

    let publisher = builder
        .add_local_child(
            "publisher",
            move |handles| {
                Box::pin(async move {
                    let counter = Arc::new(AtomicUsize::new(0));
                    let inspector = fuchsia_inspect::Inspector::default();
                    inspector.root().record_lazy_values("", move || {
                        let inspector = fuchsia_inspect::Inspector::default();
                        inspector
                            .root()
                            .record_uint("counter", counter.fetch_add(1, Ordering::SeqCst) as u64);
                        async move { Ok(inspector) }.boxed()
                    });
                    let mut options = inspect_runtime::PublishOptions::default();
                    if let Ok(proxy) =
                        handles.connect_to_protocol::<fidl_fuchsia_inspect::InspectSinkProxy>()
                    {
                        if let Ok(client_end) = proxy.into_client_end() {
                            options = options.on_inspect_sink_client(client_end);
                        }
                    }
                    let _inspect_server = inspect_runtime::publish(&inspector, options);
                    let mut fs = fuchsia_component::server::ServiceFs::new();
                    fs.serve_connection(handles.outgoing_dir)?;
                    fs.collect::<()>().await;
                    Ok(())
                })
            },
            ChildOptions::new().eager(),
        )
        .await?;

    let persistence = builder
        .add_child("persistence", "#meta/diagnostics-persistence.cm", ChildOptions::new())
        .await?;

    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.diagnostics.persist.PersistencePeriodSeconds".parse().unwrap(),
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int64(interval)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.diagnostics.persist.SkipUpdateCheck".parse().unwrap(),
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(
                skip_update_check,
            )),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.diagnostics.persist.StopOnIdleTimeoutMillis".parse().unwrap(),
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int64(-1)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.diagnostics.persist.LowBatteryThresholdPercent".parse().unwrap(),
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint64(10)),
        }))
        .await?;
    builder
        .add_route(
            Route::new()
                .capability(Capability::configuration(
                    "fuchsia.diagnostics.persist.PersistencePeriodSeconds",
                ))
                .capability(Capability::configuration(
                    "fuchsia.diagnostics.persist.SkipUpdateCheck",
                ))
                .capability(Capability::configuration(
                    "fuchsia.diagnostics.persist.StopOnIdleTimeoutMillis",
                ))
                .capability(Capability::configuration(
                    "fuchsia.diagnostics.persist.LowBatteryThresholdPercent",
                ))
                .from(Ref::self_())
                .to(&persistence),
        )
        .await?;

    if let Some(tx) = mock_battery_tx {
        let battery_manager = builder
            .add_local_child(
                "battery-manager",
                move |handles| {
                    let tx = tx.clone();
                    Box::pin(async move {
                        let mut fs = fuchsia_component::server::ServiceFs::new();
                        fs.dir("svc").add_fidl_service(
                            |stream: fbattery::BatteryManagerRequestStream| stream,
                        );
                        fs.serve_connection(handles.outgoing_dir)?;
                        fs.for_each_concurrent(None, move |mut stream| {
                            let mut tx = tx.clone();
                            async move {
                                while let Ok(Some(req)) = stream.try_next().await {
                                    match req {
                                        fbattery::BatteryManagerRequest::Watch {
                                            watcher,
                                            control_handle: _,
                                        } => {
                                            let proxy = watcher.into_proxy();
                                            let _ = tx.send(proxy).await;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        })
                        .await;
                        Ok(())
                    })
                },
                ChildOptions::new().eager(),
            )
            .await?;

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fbattery::BatteryManagerMarker>())
                    .from(&battery_manager)
                    .to(&persistence),
            )
            .await?;
    }

    if let Some(tx) = mock_update_tx {
        let update_listener = builder
            .add_local_child(
                "update-listener",
                move |handles| {
                    let tx = tx.clone();
                    Box::pin(async move {
                        let mut fs = fuchsia_component::server::ServiceFs::new();
                        fs.dir("svc")
                            .add_fidl_service(|stream: fupdate::ListenerRequestStream| stream);
                        fs.serve_connection(handles.outgoing_dir)?;
                        fs.for_each_concurrent(None, move |mut stream| {
                            let mut tx = tx.clone();
                            async move {
                                while let Ok(Some(req)) = stream.try_next().await {
                                    match req {
                                        fupdate::ListenerRequest::NotifyOnFirstUpdateCheck {
                                            payload,
                                            control_handle: _,
                                        } => {
                                            if let Some(notifier) = payload.notifier {
                                                let proxy = notifier.into_proxy();
                                                let _ = tx.send(proxy).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        })
                        .await;
                        Ok(())
                    })
                },
                ChildOptions::new().eager(),
            )
            .await?;

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fupdate::ListenerMarker>())
                    .from(&update_listener)
                    .to(&persistence),
            )
            .await?;
    }

    // Route capability_requested event stream to archivist
    builder
        .add_route(
            Route::new()
                .capability(Capability::event_stream("capability_requested"))
                .from(Ref::parent())
                .to(&archivist),
        )
        .await?;

    // Route LogSink from parent to archivist, publisher, and persistence
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<flogger::LogSinkMarker>())
                .from(Ref::parent())
                .to(&archivist)
                .to(&publisher)
                .to(&persistence),
        )
        .await?;

    // Route InspectSink from archivist to publisher, persistence
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fidl_fuchsia_inspect::InspectSinkMarker>())
                .from(&archivist)
                .to(&publisher)
                .to(&persistence),
        )
        .await?;

    // Route diagnostics-accessors dictionary from archivist as ArchiveAccessor.previous_boot to persistence
    builder
        .add_route(
            Route::new()
                .capability(
                    Capability::protocol::<fdiagnostics::ArchiveAccessorMarker>()
                        .as_("fuchsia.diagnostics.ArchiveAccessor.previous_boot"),
                )
                .from(Ref::dictionary(&archivist, "diagnostics-accessors"))
                .to(&persistence),
        )
        .await?;

    // Route storage cache from parent to persistence
    builder
        .add_route(
            Route::new()
                .capability(Capability::storage("cache"))
                .from(Ref::parent())
                .to(&persistence),
        )
        .await?;

    // Route PreviousBootDataProvider from persistence to parent
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<
                    fdiagnostics_persistence::PreviousBootDataProviderMarker,
                >())
                .from(&persistence)
                .to(Ref::parent()),
        )
        .await?;

    // Route LifecycleController from framework to parent
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fsys2::LifecycleControllerMarker>())
                .from(Ref::framework())
                .to(Ref::parent()),
        )
        .await?;

    Ok(builder.build().await?)
}

async fn restart_persistence(lifecycle: &fsys2::LifecycleControllerProxy) -> Result<(), Error> {
    lifecycle
        .stop_instance("./persistence")
        .await?
        .map_err(|e| anyhow::anyhow!("stop_instance error: {e:?}"))?;

    let (_, binder_server) = fidl::endpoints::create_endpoints();
    lifecycle
        .start_instance("./persistence", binder_server)
        .await?
        .map_err(|e| anyhow::anyhow!("start_instance error: {e:?}"))?;
    Ok(())
}

async fn wait_for_snapshot(
    instance: &RealmInstance,
) -> Result<fdiagnostics_persistence::PreviousBootData, Error> {
    loop {
        let provider: fdiagnostics_persistence::PreviousBootDataProviderProxy =
            instance.root.connect_to_protocol_at_exposed_dir()?;
        let data = provider
            .watch_previous_boot_data(
                &fdiagnostics_persistence::PreviousBootDataProviderOptions::default(),
            )
            .await?;
        if data.inspect.is_some() {
            return Ok(data);
        }
        fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(
            50,
        )))
        .await;
    }
}

#[fuchsia::test]
async fn test_persistence_rotation() -> Result<(), Error> {
    const INTERVAL: i64 = 1;
    let instance = make_realm(INTERVAL, None, None).await?;

    // Boot 1 Check: Connect to PreviousBootDataProvider, verify data.inspect is None
    let provider: fdiagnostics_persistence::PreviousBootDataProviderProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;
    let data = provider.watch_previous_boot_data(&Default::default()).await?;
    assert!(data.inspect.is_none(), "Expected no previous boot data on initial boot");

    // Wait for active snapshot on Boot 1 to be written before restarting persistence
    fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
        2 * INTERVAL,
    )))
    .await;

    let lifecycle: fsys2::LifecycleControllerProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;

    // Boot 2 (First Rotation): Wait until inspect data is present after restart
    restart_persistence(&lifecycle).await?;
    let data = wait_for_snapshot(&instance).await?;

    let inspect_file = data.inspect.expect("Expected inspect file on Boot 2");
    let file_proxy = inspect_file.into_proxy();
    let content = fuchsia_fs::file::read_to_string(&file_proxy).await?;
    let t1 = extract_counter(&content).unwrap_or_else(|| {
        panic!("Failed to extract counter T1 from JSON:\n{content}");
    });
    assert!(t1 > 0, "Expected T1 > 0, got {t1}");

    fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
        2 * INTERVAL,
    )))
    .await;

    // Boot 3 (Second Rotation): Wait until inspect data is present after next restart
    restart_persistence(&lifecycle).await?;
    let data = wait_for_snapshot(&instance).await?;

    let inspect_file = data.inspect.expect("Expected inspect file on Boot 3");
    let file_proxy = inspect_file.into_proxy();
    let content2 = fuchsia_fs::file::read_to_string(&file_proxy).await?;
    let t2 = extract_counter(&content2).unwrap_or_else(|| {
        panic!("Failed to extract counter T2 from JSON:\n{content2}");
    });

    assert!(t2 > t1, "Expected T2 ({t2}) > T1 ({t1})");

    Ok(())
}

#[fuchsia::test]
async fn test_low_battery_trigger() -> Result<(), Error> {
    const INTERVAL: i64 = 300;
    let (tx, mut rx) = mpsc::channel(1);
    let instance = make_realm(INTERVAL, Some(tx), None).await?;

    // Boot 1 Check: Connect to PreviousBootDataProvider, verify data.inspect is None
    let provider: fdiagnostics_persistence::PreviousBootDataProviderProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;
    let data = provider.watch_previous_boot_data(&Default::default()).await?;
    assert!(data.inspect.is_none(), "Expected no previous boot data on initial boot");

    // Wait for persistence to connect to mock BatteryManager and send watcher proxy
    let watcher_proxy = rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to receive BatteryInfoWatcherProxy"))?;

    // Emit a low battery event (8.0% <= threshold 10.0%)
    let low_battery_info = fbattery::BatteryInfo {
        level_percent: Some(8.0),
        level_status: Some(fbattery::LevelStatus::Low),
        ..Default::default()
    };
    watcher_proxy.on_change_battery_info(&low_battery_info, None).await?;

    let lifecycle: fsys2::LifecycleControllerProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;

    // Restart persistence to rotate active snapshot to previous_boot
    restart_persistence(&lifecycle).await?;

    // Verify inspect snapshot generated by low-battery trigger is present
    let data = wait_for_snapshot(&instance).await?;
    let inspect_file =
        data.inspect.expect("Expected inspect snapshot generated by low battery event");
    let file_proxy = inspect_file.into_proxy();
    let content = fuchsia_fs::file::read_to_string(&file_proxy).await?;
    let counter = extract_counter(&content).unwrap_or_else(|| {
        panic!("Failed to extract counter from JSON:\n{content}");
    });
    assert!(counter > 0, "Expected valid inspect counter > 0, got {counter}");

    Ok(())
}

#[fuchsia::test]
async fn test_update_check_gating() -> Result<(), Error> {
    const INTERVAL: i64 = 5;
    let (tx, mut rx) = mpsc::channel(1);
    let instance = make_realm(INTERVAL, None, Some(tx)).await?;

    let provider: fdiagnostics_persistence::PreviousBootDataProviderProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;

    // Wait for persistence to connect to mock update Listener and send notifier proxy
    let notifier_proxy =
        rx.next().await.ok_or_else(|| anyhow::anyhow!("Failed to receive NotifierProxy"))?;

    // Signal that the post-boot update check is complete
    notifier_proxy.notify()?;

    let data = provider.watch_previous_boot_data(&Default::default()).await?;
    assert!(data.inspect.is_none(), "Expected no previous boot data on initial boot");

    // Sleep so persistence collects an active snapshot
    fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
        INTERVAL,
    )))
    .await;

    let lifecycle: fsys2::LifecycleControllerProxy =
        instance.root.connect_to_protocol_at_exposed_dir()?;
    restart_persistence(&lifecycle).await?;

    // On Boot 2 restart, persistence connects to update Listener again
    let notifier_proxy_2 = rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to receive NotifierProxy on Boot 2"))?;
    notifier_proxy_2.notify()?;

    // Verify inspect snapshot is served by PreviousBootDataProvider
    let data = wait_for_snapshot(&instance).await?;
    assert!(data.inspect.is_some(), "Expected previous boot data after update check complete");

    Ok(())
}
