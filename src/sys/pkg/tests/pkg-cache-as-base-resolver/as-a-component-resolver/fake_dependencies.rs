// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServerEnd};
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_component_runtime as fruntime;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_metrics as fmetrics;
use fuchsia_component::runtime::{Data, DataValue, Dictionary, DictionaryRouterReceiver};
use futures::FutureExt;
use log::info;
use mock_metrics::MockMetricEventLoggerFactory;
use std::sync::Arc;
use vfs::directory::helper::DirectlyMutable as _;

// When this feature is enabled, the base-resolver integration tests will start Fxblob.
#[cfg(feature = "use_fxblob")]
static BLOB_IMPLEMENTATION: blobfs_ramdisk::Implementation = blobfs_ramdisk::Implementation::Fxblob;

// When this feature is not enabled, the base-resolver integration tests will start cpp Blobfs.
#[cfg(not(feature = "use_fxblob"))]
static BLOB_IMPLEMENTATION: blobfs_ramdisk::Implementation =
    blobfs_ramdisk::Implementation::CppBlobfs;

const OUT_DIR_FLAGS: fio::Flags =
    fio::PERM_READABLE.union(fio::PERM_WRITABLE).union(fio::PERM_EXECUTABLE);

async fn initialize_dictionary(value: &str) -> Result<Dictionary> {
    let dictionary = Dictionary::new().await;

    let key = "fuchsia.zircon.system.pkgfs.cmd";

    let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::String(value.to_string()));
    let data = Data::new(DataValue::Bytes(fidl::persist(&config)?)).await;
    dictionary.insert(key, data).await;

    Ok(dictionary)
}

async fn run_dictionary_router(
    dictionary: Dictionary,
    stream: fruntime::DictionaryRouterRequestStream,
) {
    let receiver = DictionaryRouterReceiver::from(stream);
    receiver
        .handle_with(move |_request, _instance_token| {
            futures::future::ready(Ok(Some(dictionary.clone()))).boxed()
        })
        .await;
}

#[fuchsia::main]
async fn main() {
    info!("started");
    let this_pkg = fuchsia_pkg_testing::Package::identity().await.unwrap();
    let static_packages =
        system_image::StaticPackages::from([("mock-package/0".parse().unwrap(), *this_pkg.hash())]);
    let mut static_packages_bytes = vec![];
    static_packages.serialize(&mut static_packages_bytes).expect("write static_packages");
    let system_image = fuchsia_pkg_testing::PackageBuilder::new("system_image")
        .add_resource_at("data/static_packages", static_packages_bytes.as_slice())
        .build()
        .await
        .expect("build system_image package");
    let blobfs = blobfs_ramdisk::BlobfsRamdisk::builder()
        .implementation(BLOB_IMPLEMENTATION)
        .start()
        .await
        .unwrap();
    let () = system_image.write_to_blobfs(&blobfs).await;
    let () = this_pkg.write_to_blobfs_ignore_subpackages(&blobfs).await;
    let the_subpackage = fuchsia_pkg_testing::Package::from_dir("/the-subpackage").await.unwrap();
    let () = the_subpackage.write_to_blobfs(&blobfs).await;

    let pkgfs_boot_arg_value = format!("bin/pkgsvr+{}", *system_image.hash());
    let dictionary = initialize_dictionary(&pkgfs_boot_arg_value).await.unwrap();

    // Use VFS because ServiceFs does not support PERM_EXECUTABLE, but /blob needs it.
    let out_dir = vfs::pseudo_directory! {
        "svc" => vfs::pseudo_directory! {
            fmetrics::MetricEventLoggerFactoryMarker::PROTOCOL_NAME =>
                vfs::service::host(move |stream| {
                    Arc::new(MockMetricEventLoggerFactory::new()).run_logger_factory(stream)
                }),
            fruntime::DictionaryRouterMarker::PROTOCOL_NAME => {
                vfs::service::host(move |stream| {
                    run_dictionary_router(dictionary.clone(), stream) })
            }
        },
        "blob" =>
            vfs::remote::remote_dir(blobfs.root_dir_proxy().expect("get blobfs root dir")),
    };

    out_dir
        .add_entry(
            "fxfs-svc",
            vfs::remote::remote_dir(blobfs.svc_dir().expect("get blobfs svc dir")),
        )
        .unwrap();

    let scope = vfs::execution_scope::ExecutionScope::new();
    let dir_server: ServerEnd<fio::DirectoryMarker> =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
            .unwrap()
            .into();
    vfs::directory::serve_on(out_dir, OUT_DIR_FLAGS, scope.clone(), dir_server);
    let () = scope.wait().await;
}
