// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use diagnostics_reader::ArchiveReader;
use fidl_fuchsia_sys2 as fsys2;
use fuchsia_async as fasync;
use fuchsia_component_test::{
    Capability, RealmBuilder, RealmBuilderParams, RealmInstance, Ref, Route,
};
use futures::StreamExt;
use zx;

async fn build_realm() -> RealmInstance {
    let builder =
        RealmBuilder::with_params(RealmBuilderParams::new().from_relative_url("#meta/realm.cm"))
            .await
            .expect("created");

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<fsys2::LifecycleControllerMarker>())
                .from(Ref::framework())
                .to(Ref::parent()),
        )
        .await
        .unwrap();

    builder.build().await.unwrap()
}

#[fuchsia::test]
async fn test_shutdown_with_zombie() {
    let realm_instance = build_realm().await;
    let lifecycle_controller: fsys2::LifecycleControllerProxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    // Run the test program to generate an unreaped zombie.
    start(&realm_instance, &lifecycle_controller, "./zombie_shutdown", "[ZOMBIE_READY]").await;
    stop(&lifecycle_controller, "./zombie_shutdown").await;
}

#[fuchsia::test]
async fn test_stop_last_on_shutdown() {
    let realm_instance = build_realm().await;
    let lifecycle_controller: fsys2::LifecycleControllerProxy =
        realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap();

    // `stopped_last` runs an executable that the container lists in `stop_last_on_shutdown`, so it
    // must only be asked to exit once `stopped_first`, which takes 500ms to exit, has exited.
    start(&realm_instance, &lifecycle_controller, "./stopped_first", "[STOPPED_FIRST_READY]").await;
    start(&realm_instance, &lifecycle_controller, "./stopped_last", "[STOPPED_LAST_READY]").await;

    // Stopping the container shuts down the kernel.
    stop(&lifecycle_controller, "./container").await;
    let first_exited = wait_for_kernel_log(&realm_instance, "[STOPPED_FIRST_EXITING]").await;
    let last_exited = wait_for_kernel_log(&realm_instance, "[STOPPED_LAST_EXITING]").await;
    assert!(first_exited < last_exited, "stop_last_on_shutdown process exited first");
}

// Starts `moniker` and waits until its program writes `ready_marker` to stdout.
async fn start(
    realm_instance: &RealmInstance,
    lifecycle_controller: &fsys2::LifecycleControllerProxy,
    moniker: &str,
    ready_marker: &str,
) {
    let (_, binder_server) = fidl::endpoints::create_endpoints();
    lifecycle_controller.start_instance(moniker, binder_server).await.unwrap().unwrap();
    wait_for_kernel_log(realm_instance, ready_marker).await;
}

// Stops `moniker`, failing if that takes more than 5s.
async fn stop(lifecycle_controller: &fsys2::LifecycleControllerProxy, moniker: &str) {
    fasync::TimeoutExt::on_timeout(
        lifecycle_controller.stop_instance(moniker),
        fasync::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(5)),
        || panic!("stopping {moniker} took more than 5s"),
    )
    .await
    .expect("FIDL error")
    .expect("lifecycle error");
}

// Waits until a program in the container writes `marker` to stdout, and returns when it did. Fails
// if that takes more than 5s.
async fn wait_for_kernel_log(realm_instance: &RealmInstance, marker: &str) -> zx::BootInstant {
    let realm_moniker = format!("realm_builder:{}", realm_instance.root.child_name());
    let kernel_moniker = format!("{realm_moniker}/kernel");

    let mut logs = ArchiveReader::logs()
        .select_all_for_component(kernel_moniker.as_str())
        .snapshot_then_subscribe()
        .expect("failed to subscribe to kernel logs");

    let find_marker = async {
        while let Some(log) = logs.next().await {
            let log = log.expect("failed to read log from stream");
            if log.msg().is_some_and(|msg| msg.contains(marker)) {
                return log.metadata.timestamp;
            }
        }
        panic!("kernel log stream ended before {marker}");
    };
    fasync::TimeoutExt::on_timeout(
        find_marker,
        fasync::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(5)),
        || panic!("{marker} wasn't logged within 5s"),
    )
    .await
}
