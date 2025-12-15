// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow};
use fidl::endpoints::DiscoverableProtocolMarker;
use fuchsia_component::client;
use futures::stream::{StreamExt as _, TryStreamExt as _};
use log::{info, warn};
use std::sync::Arc;
use {fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_component_sandbox as fsandbox};

static PKGFS_BOOT_ARG_VALUE_PREFIX: &'static str = "bin/pkgsvr+";

/// Flags for fake_boot_arguments.
#[derive(argh::FromArgs, Debug, PartialEq)]
pub struct Args {
    /// absolute path to system_image package file.
    #[argh(option)]
    system_image_path: String,
}

async fn initialize_dictionary(
    store: &fsandbox::CapabilityStoreProxy,
    id_gen: &sandbox::CapabilityIdGenerator,
    value: &str,
) -> Result<u64> {
    let dict_id = id_gen.next();
    store
        .dictionary_create(dict_id)
        .await?
        .map_err(|e| anyhow!("failed dictionary_create: {e:?}"))?;

    let key = "fuchsia.zircon.system.pkgfs.cmd";

    let config_id = id_gen.next();
    let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::String(value.to_string()));
    let config = fsandbox::Capability::Data(fsandbox::Data::Bytes(fidl::persist(&config)?));
    store.import(config_id, config).await?.map_err(|e| anyhow!("failed import: {e:?}"))?;
    store
        .dictionary_insert(
            dict_id,
            &fsandbox::DictionaryItem { key: key.to_string(), value: config_id },
        )
        .await?
        .map_err(|e| anyhow!("failed dictionary_insert: {e:?}"))?;

    Ok(dict_id)
}

enum BootServices {
    Items(fidl_fuchsia_boot::ItemsRequestStream),
    Router(fsandbox::DictionaryRouterRequestStream),
}

#[fuchsia::main]
async fn main() {
    info!("Starting fake_boot_arguments...");
    let args @ Args { system_image_path } = &argh::from_env();
    info!(args:?; "Initalizing fake_boot_arguments");

    let system_image = fuchsia_fs::file::read(
        &fuchsia_fs::file::open_in_namespace(system_image_path.as_str(), fuchsia_fs::PERM_READABLE)
            .unwrap(),
    )
    .await
    .unwrap();

    let system_image_merkle = fuchsia_merkle::root_from_slice(&system_image);
    let pkgfs_boot_arg_value = format!("{}{}", PKGFS_BOOT_ARG_VALUE_PREFIX, system_image_merkle);

    let store = client::connect_to_protocol::<fsandbox::CapabilityStoreMarker>().unwrap();
    let id_gen = sandbox::CapabilityIdGenerator::new();

    let dict_id = initialize_dictionary(&store, &id_gen, &pkgfs_boot_arg_value).await.unwrap();

    let mut fs = fuchsia_component::server::ServiceFs::new();
    fs.dir("svc").add_fidl_service(BootServices::Items);
    fs.dir("svc").add_fidl_service(BootServices::Router);
    fs.take_and_serve_directory_handle().unwrap();

    fs.for_each_concurrent(None, move |stream| {
        let store = store.clone();
        let id_gen = id_gen.clone();
        async move {
            match stream {
                BootServices::Items(stream) => {
                    // The VMO provided here would be for the recovery case only, which isn't of interest for pkg_test.
                    run_boot_items(stream, None).await
                }
                BootServices::Router(mut stream) => {
                    while let Ok(Some(request)) = stream.try_next().await {
                        match request {
                            fsandbox::DictionaryRouterRequest::Route { payload: _, responder } => {
                                let dup_dict_id = id_gen.next();
                                store.duplicate(dict_id, dup_dict_id).await.unwrap().unwrap();
                                let capability = store.export(dup_dict_id).await.unwrap().unwrap();
                                let fsandbox::Capability::Dictionary(dict) = capability else {
                                    panic!("capability was not a dictionary? {capability:?}");
                                };
                                let _ = responder.send(Ok(
                                    fsandbox::DictionaryRouterRouteResponse::Dictionary(dict),
                                ));
                            }
                            fsandbox::DictionaryRouterRequest::_UnknownMethod {
                                ordinal, ..
                            } => {
                                warn!(ordinal:%; "Unknown DictionaryRouter request");
                            }
                        }
                    }
                }
            }
        }
    })
    .await;
}

/// Identifier for ramdisk storage. Defined in sdk/lib/zbi-format/include/lib/zbi-format/zbi.h.
const ZBI_TYPE_STORAGE_RAMDISK: u32 = 0x4b534452;

// Mocks for fshost, from https://cs.opensource.google/fuchsia/fuchsia/+/main:src/storage/fshost/integration/src/mocks.rs
// fshost uses exactly one boot item - it checks to see if there is an item of type
// ZBI_TYPE_STORAGE_RAMDISK. If it's there, it's a vmo that represents a ramdisk version of the
// fvm, and fshost creates a ramdisk from the vmo so it can go through the normal device matching.
async fn run_boot_items(
    mut stream: fidl_fuchsia_boot::ItemsRequestStream,
    vmo: Option<Arc<zx::Vmo>>,
) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            fidl_fuchsia_boot::ItemsRequest::Get { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!(extra, 0);
                let response_vmo = vmo.as_ref().map(|vmo| {
                    vmo.create_child(zx::VmoChildOptions::SLICE, 0, vmo.get_size().unwrap())
                        .unwrap()
                });
                responder.send(response_vmo, 0).unwrap();
            }
            fidl_fuchsia_boot::ItemsRequest::Get2 { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!((*extra.unwrap()).n, 0);
                responder.send(Ok(Vec::new())).unwrap();
            }
            fidl_fuchsia_boot::ItemsRequest::GetBootloaderFile { .. } => {
                panic!(
                    "unexpectedly called GetBootloaderFile on {}",
                    fidl_fuchsia_boot::ItemsMarker::PROTOCOL_NAME
                );
            }
        }
    }
}
