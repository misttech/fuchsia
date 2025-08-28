// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::disk_builder::{DataSpec, DiskBuilder, VolumesSpec};
use anyhow::Error;
use ffeedback::FileReportResults;
use fidl::prelude::*;
use fs_management::filesystem::DirBasedBlockConnector;
use fuchsia_component::client::connect::connect_to_named_protocol_at_dir_root;
use fuchsia_component_test::LocalComponentHandles;
use futures::channel::mpsc::{self};
use futures::future::BoxFuture;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use std::sync::Arc;
use vfs::execution_scope::ExecutionScope;

use {
    fidl_fuchsia_boot as fboot, fidl_fuchsia_feedback as ffeedback,
    fidl_fuchsia_fshost_fxfsprovisioner as ffxfsprovisioner, fidl_fuchsia_io as fio,
    fidl_fuchsia_storage_partitions as fpartitions,
};

/// Identifier for ramdisk storage. Defined in sdk/lib/zbi-format/include/lib/zbi-format/zbi.h.
const ZBI_TYPE_STORAGE_RAMDISK: u32 = 0x4b534452;

pub async fn new_mocks(
    vmo: Option<zx::Vmo>,
    crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
    device_config: String,
) -> impl Fn(LocalComponentHandles) -> BoxFuture<'static, Result<(), Error>> + Sync + Send + 'static
{
    let vmo = vmo.map(Arc::new);
    let mock = move |handles: LocalComponentHandles| {
        let vmo_clone = vmo.clone();
        let config_clone = device_config.clone();
        run_mocks(handles, vmo_clone, crash_reports_sink.clone(), config_clone).boxed()
    };

    mock
}

async fn run_mocks(
    handles: LocalComponentHandles,
    vmo: Option<Arc<zx::Vmo>>,
    crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
    device_config: String,
) -> Result<(), Error> {
    let export = vfs::pseudo_directory! {
        "boot" => vfs::pseudo_directory! {
            "config" => vfs::pseudo_directory! {
                "fshost" => vfs::file::read_only(&device_config),
                // Tests are expected to use a null zxcrypt policy.
                "zxcrypt" => vfs::file::read_only("null"),
            },
        },
        "svc" => vfs::pseudo_directory! {
            fboot::ItemsMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                let vmo_clone = vmo.clone();
                run_boot_items(stream, vmo_clone)
            }),
            ffeedback::CrashReporterMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                run_crash_reporter(stream, crash_reports_sink.clone())
            }),
            ffxfsprovisioner::FxfsProvisionerMarker::PROTOCOL_NAME => vfs::service::host(
                move |stream| { run_fxfs_provisioner(stream) }
            ),
        },
    };

    let scope = ExecutionScope::new();
    vfs::directory::serve_on(export, fio::PERM_READABLE, scope.clone(), handles.outgoing_dir);
    scope.wait().await;

    Ok(())
}

/// fshost uses exactly one boot item - it checks to see if there is an item of type
/// ZBI_TYPE_STORAGE_RAMDISK. If it's there, it's a vmo that represents a ramdisk version of the
/// fvm, and fshost creates a ramdisk from the vmo so it can go through the normal device matching.
async fn run_boot_items(mut stream: fboot::ItemsRequestStream, vmo: Option<Arc<zx::Vmo>>) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            fboot::ItemsRequest::Get { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!(extra, 0);
                let response_vmo = vmo.as_ref().map(|vmo| {
                    vmo.create_child(zx::VmoChildOptions::SLICE, 0, vmo.get_size().unwrap())
                        .unwrap()
                });
                responder.send(response_vmo, 0).unwrap();
            }
            fboot::ItemsRequest::Get2 { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!((*extra.unwrap()).n, 0);
                responder.send(Ok(Vec::new())).unwrap();
            }
            fboot::ItemsRequest::GetBootloaderFile { .. } => {
                panic!(
                    "unexpectedly called GetBootloaderFile on {}",
                    fboot::ItemsMarker::PROTOCOL_NAME
                );
            }
        }
    }
}

async fn run_crash_reporter(
    mut stream: ffeedback::CrashReporterRequestStream,
    mut crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            ffeedback::CrashReporterRequest::FileReport { report, responder } => {
                crash_reports_sink.send(report).await.unwrap();
                responder.send(Ok(&FileReportResults::default())).unwrap();
            }
        }
    }
}

async fn run_fxfs_provisioner(mut stream: ffxfsprovisioner::FxfsProvisionerRequestStream) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            ffxfsprovisioner::FxfsProvisionerRequest::Provision {
                partition_service,
                responder,
            } => {
                let partition_service = partition_service.into_proxy();

                let overlay = connect_to_named_protocol_at_dir_root::<
                    fpartitions::OverlayPartitionProxy,
                >(&partition_service, "overlay")
                .expect("failed to connect to OverlayPartition protocol");
                let partitions_info = overlay
                    .get_partitions()
                    .await
                    .expect("get_partitions FIDL call failed")
                    .expect("get_partitions failed");
                assert_eq!(partitions_info.len(), 2);
                assert!(partitions_info.iter().any(|info| info.name == "super"));
                assert!(partitions_info.iter().any(|info| info.name == "userdata"));

                let connector =
                    Box::new(DirBasedBlockConnector::new(partition_service, "/volume".to_string()));

                let mut disk_builder = DiskBuilder::new();
                disk_builder
                    .format_volumes(VolumesSpec { fxfs_blob: true, create_data_partition: true })
                    .format_data(DataSpec { format: Some("fxfs"), zxcrypt: false });
                disk_builder.build_fxfs_as_volume_manager(connector).await;

                responder.send(Ok(())).unwrap();
            }
            _ => {
                unreachable!()
            }
        }
    }
}
