// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl_fuchsia_boot as fboot;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_component_runtime as fruntime;
use fuchsia_component::client;
use fuchsia_component::runtime::{Data, DataValue, Dictionary, DictionaryRouterReceiver};
use fuchsia_component::server::ServiceFs;
use futures::{FutureExt, StreamExt};

async fn initialize_dictionary(
    boot_args: fboot::ArgumentsProxy,
    capabilities_proxy: fruntime::CapabilitiesProxy,
) -> Result<Dictionary> {
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
        "driver.usb.peripheral".to_string(),
        "omaha_app_id".to_string(),
        "omaha_url".to_string(),
        "ota_channel".to_string(),
        "ota_realm".to_string(),
        "product_id".to_string(),
        "TERM".to_string(),
        "system.base_merkle".to_string(),
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

    let dictionary = Dictionary::new_with_proxy(capabilities_proxy.clone()).await;
    for (key, value) in std::iter::zip(bool_keys, bool_values) {
        let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(value));
        let config = Data::new_with_proxy(
            capabilities_proxy.clone(),
            DataValue::Bytes(fidl::persist(&config)?),
        )
        .await;
        dictionary.insert(&format!("fuchsia.{}", key.key), config).await;
    }

    for (key, value) in std::iter::zip(string_keys, string_values) {
        let value = value.unwrap_or_else(|| "".to_string());
        let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::String(value));
        let config = Data::new_with_proxy(
            capabilities_proxy.clone(),
            DataValue::Bytes(fidl::persist(&config)?),
        )
        .await;
        dictionary.insert(&format!("fuchsia.{}", key), config).await;
    }

    // Model `kernel.serial` as a boolean indicating whether the kernel has claimed the debug
    // serial UART:
    // - If `kernel.serial` is "none", false (kernel did not claim the UART; usermode can drive it).
    // - If `kernel.serial` is unset or anything other than "none" (e.g. "legacy", or a uart config),
    //   true (kernel has claimed the debug serial UART).
    // This avoids leaking the raw `kernel.serial` boot option string format to usermode drivers,
    // providing a clean boolean API surface for whether the UART is claimed by the kernel.
    //
    // Note: Only the kernel command line (ZBI CMDLINE items) configures the kernel. A value that
    // fuchsia.boot.Arguments takes from ImageArgs or /boot/config/additional_boot_args
    // overrides the command line here without changing the kernel.
    let kernel_serial = boot_args.get_string("kernel.serial").await?;
    let kernel_serial_enabled = match kernel_serial.as_deref() {
        Some("none") => false,
        _ => true,
    };
    let config = fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(kernel_serial_enabled));
    let config =
        Data::new_with_proxy(capabilities_proxy.clone(), DataValue::Bytes(fidl::persist(&config)?))
            .await;
    dictionary.insert("fuchsia.kernel.serial", config).await;

    Ok(dictionary)
}

enum IncomingRequest {
    Router(fruntime::DictionaryRouterRequestStream),
}

#[fuchsia::main(logging = true)]
async fn main() -> Result<()> {
    let boot_args = client::connect_to_protocol::<fboot::ArgumentsMarker>()?;
    let capabilities_proxy = client::connect_to_protocol::<fruntime::CapabilitiesMarker>()?;
    let dictionary = initialize_dictionary(boot_args, capabilities_proxy).await?;

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(IncomingRequest::Router);
    fs.take_and_serve_directory_handle()?;
    fs.for_each_concurrent(None, move |request: IncomingRequest| {
        let dictionary = dictionary.clone();
        async move {
            match request {
                IncomingRequest::Router(stream) => {
                    let dictionary = dictionary.clone();
                    DictionaryRouterReceiver::from(stream)
                        .handle_with(move |_request, _instance_token| {
                            futures::future::ready(Ok(Some(dictionary.clone()))).boxed()
                        })
                        .await;
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
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    async fn test_with_kernel_serial(
        kernel_serial: Option<&'static str>,
    ) -> HashMap<String, fdecl::ConfigValue> {
        let (capabilities_proxy, mut capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let (boot_args_proxy, mut boot_args_stream) =
            fidl::endpoints::create_proxy_and_stream::<fboot::ArgumentsMarker>();

        // Spawn boot args server
        let _boot_args_task = fuchsia_async::Task::local(async move {
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
                    fboot::ArgumentsRequest::GetString { key, responder } => {
                        assert_eq!(key, "kernel.serial");
                        responder.send(kernel_serial).unwrap();
                    }
                    _ => {
                        panic!("unexpected message to boot args");
                    }
                }
            }
        });

        let created_config = Rc::new(RefCell::new(None));
        let created_config_clone = created_config.clone();
        let inserted_configs = Rc::new(RefCell::new(HashMap::new()));
        let inserted_configs_clone = inserted_configs.clone();

        // Spawn capabilities server
        let _capabilities_task = fuchsia_async::Task::local(async move {
            while let Some(Ok(request)) = capabilities_stream.next().await {
                match request {
                    fruntime::CapabilitiesRequest::DictionaryCreate { responder, .. } => {
                        responder.send(Ok(())).unwrap();
                    }
                    fruntime::CapabilitiesRequest::DataCreate { data, responder, .. } => {
                        let config = match data {
                            fruntime::Data::Bytes(bytes) => {
                                fidl::unpersist::<fdecl::ConfigValue>(&bytes).unwrap()
                            }
                            _ => panic!("unexpected data type"),
                        };
                        *created_config_clone.borrow_mut() = Some(config);
                        responder.send(Ok(())).unwrap();
                    }
                    fruntime::CapabilitiesRequest::DictionaryInsert { key, responder, .. } => {
                        let config = created_config_clone
                            .borrow_mut()
                            .take()
                            .expect("DataCreate must precede DictionaryInsert");
                        inserted_configs_clone.borrow_mut().insert(key, config);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {
                        panic!("unexpected message to capabilities");
                    }
                }
            }
        });

        let _dictionary = initialize_dictionary(boot_args_proxy, capabilities_proxy).await.unwrap();
        inserted_configs.take()
    }

    #[fuchsia::test]
    async fn initialize_dictionary_test() {
        let configs = test_with_kernel_serial(None).await;
        assert_eq!(configs.len(), 24);
        assert_eq!(
            configs.get("fuchsia.kernel.serial"),
            Some(&fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(true)))
        );
    }

    #[fuchsia::test]
    async fn kernel_serial_none_produces_false() {
        let configs = test_with_kernel_serial(Some("none")).await;
        assert_eq!(configs.len(), 24);
        assert_eq!(
            configs.get("fuchsia.kernel.serial"),
            Some(&fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(false)))
        );
    }

    #[fuchsia::test]
    async fn kernel_serial_set_produces_true() {
        let configs = test_with_kernel_serial(Some("legacy")).await;
        assert_eq!(configs.len(), 24);
        assert_eq!(
            configs.get("fuchsia.kernel.serial"),
            Some(&fdecl::ConfigValue::Single(fdecl::ConfigSingleValue::Bool(true)))
        );
    }
}
