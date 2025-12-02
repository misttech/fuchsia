// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow};
use fuchsia_component::client;
use fuchsia_component::server::ServiceFs;
use log::warn;
use {
    fidl_fuchsia_boot as fboot, fidl_fuchsia_component_decl as fdecl,
    fidl_fuchsia_component_sandbox as fsandbox,
};

use futures::prelude::*;

async fn initialize_dictionary(
    boot_args: fboot::ArgumentsProxy,
    store: &fsandbox::CapabilityStoreProxy,
    id_gen: &sandbox::CapabilityIdGenerator,
) -> Result<u64> {
    let dict_id = id_gen.next();
    store
        .dictionary_create(dict_id)
        .await?
        .map_err(|e| anyhow!("failed dictionary_create: {e:?}"))?;

    // These keys must be secure. Changes should be reviewed with security.
    let bool_keys = vec![
        fboot::BoolPair { key: "astro.sysconfig.abr-wear-leveling".to_string(), defaultval: false },
        fboot::BoolPair { key: "console.shell".to_string(), defaultval: false },
        fboot::BoolPair { key: "console.use_virtio_console".to_string(), defaultval: false },
        fboot::BoolPair { key: "netsvc.advertise".to_string(), defaultval: true },
        fboot::BoolPair { key: "netsvc.all-features".to_string(), defaultval: true },
        fboot::BoolPair { key: "netsvc.disable".to_string(), defaultval: true },
        fboot::BoolPair { key: "netsvc.netboot".to_string(), defaultval: false },
    ];
    let string_keys = vec![
        "androidboot.slot_suffix".to_string(),
        "omaha_app_id".to_string(),
        "omaha_url".to_string(),
        "ota_channel".to_string(),
        "ota_realm".to_string(),
        "product_id".to_string(),
        "TERM".to_string(),
        "zircon.autorun.boot".to_string(),
        "zircon.autorun.system".to_string(),
        "zircon.namegen".to_string(),
        "zircon.nodename".to_string(),
        "zircon.system.pkgfs.cmd".to_string(),
        "zvb.current_slot".to_string(),
        "zvb.boot-partition-uuid".to_string(),
    ];
    let bool_values = boot_args.get_bools(&bool_keys).await?;
    let string_values = boot_args.get_strings(&string_keys).await?;

    for (key, value) in std::iter::zip(bool_keys, bool_values) {
        let config_id = id_gen.next();
        let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(value));
        let config = fsandbox::Capability::Data(fsandbox::Data::Bytes(fidl::persist(&config)?));
        store.import(config_id, config).await?.map_err(|e| anyhow!("failed import: {e:?}"))?;
        store
            .dictionary_insert(
                dict_id,
                &fsandbox::DictionaryItem { key: format!("fuchsia.{}", key.key), value: config_id },
            )
            .await?
            .map_err(|e| anyhow!("failed dictionary_insert: {e:?}"))?;
    }

    for (key, value) in std::iter::zip(string_keys, string_values) {
        let value = value.unwrap_or_else(|| "".to_string());
        let config_id = id_gen.next();
        let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::String(value));
        let config = fsandbox::Capability::Data(fsandbox::Data::Bytes(fidl::persist(&config)?));
        store.import(config_id, config).await?.map_err(|e| anyhow!("failed import: {e:?}"))?;
        store
            .dictionary_insert(
                dict_id,
                &fsandbox::DictionaryItem { key: format!("fuchsia.{}", key), value: config_id },
            )
            .await?
            .map_err(|e| anyhow!("failed dictionary_insert: {e:?}"))?;
    }

    Ok(dict_id)
}

enum IncomingRequest {
    Router(fsandbox::DictionaryRouterRequestStream),
}

#[fuchsia::main(logging = true)]
async fn main() -> Result<()> {
    let store = client::connect_to_protocol::<fsandbox::CapabilityStoreMarker>()?;
    let id_gen = sandbox::CapabilityIdGenerator::new();

    let boot_args = client::connect_to_protocol::<fboot::ArgumentsMarker>()?;
    let dict_id = initialize_dictionary(boot_args, &store, &id_gen).await?;

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(IncomingRequest::Router);
    fs.take_and_serve_directory_handle()?;
    fs.for_each_concurrent(None, move |request: IncomingRequest| {
        let store = store.clone();
        let id_gen = id_gen.clone();
        async move {
            match request {
                IncomingRequest::Router(mut stream) => {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[fuchsia::test]
    async fn initialize_dictionary_test() {
        let (store_proxy, store_server) =
            fidl::endpoints::create_proxy::<fsandbox::CapabilityStoreMarker>();
        let (boot_args_proxy, boot_args_server) =
            fidl::endpoints::create_proxy::<fboot::ArgumentsMarker>();
        let id_gen = sandbox::CapabilityIdGenerator::new();

        // Spawn boot args server
        fuchsia_async::Task::local(async move {
            let mut boot_args_stream = boot_args_server.into_stream();
            while let Some(Ok(request)) = boot_args_stream.next().await {
                match request {
                    fboot::ArgumentsRequest::GetBools { keys, responder } => {
                        let mut values: Vec<bool> = Vec::new();
                        for _ in keys {
                            values.push(true);
                        }
                        responder.send(&values).unwrap();
                    }
                    fboot::ArgumentsRequest::GetStrings { keys, responder } => {
                        let mut values: Vec<Option<String>> = Vec::new();
                        for key in keys {
                            values.push(Some(key.clone()));
                        }
                        responder.send(&values).unwrap();
                    }
                    _ => {
                        panic!("unexpected message to boot args");
                    }
                }
            }
        })
        .detach();

        let insert_count = Rc::new(Cell::new(0));
        let insert_count_clone = insert_count.clone();

        // Spawn store server
        fuchsia_async::Task::local(async move {
            let mut store_stream = store_server.into_stream();
            while let Some(Ok(request)) = store_stream.next().await {
                match request {
                    fsandbox::CapabilityStoreRequest::Import { responder, .. } => {
                        responder.send(Ok(())).unwrap();
                    }
                    fsandbox::CapabilityStoreRequest::DictionaryInsert { responder, .. } => {
                        insert_count_clone.update(|x| x + 1);
                        responder.send(Ok(())).unwrap();
                    }
                    fsandbox::CapabilityStoreRequest::DictionaryCreate { responder, .. } => {
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {
                        panic!("unexpected message to capability store");
                    }
                }
            }
        })
        .detach();

        let dict_id = initialize_dictionary(boot_args_proxy, &store_proxy, &id_gen).await.unwrap();
        assert_eq!(dict_id, 0);
        assert_eq!(insert_count.get(), 21);
    }
}
