// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `diagnostics-persistence` component persists Inspect VMOs and serves them at the next boot.

mod constants;
mod fetcher;
mod file_handler;
mod inspect_server;
mod persist_server;
mod scheduler;

use anyhow::{Context, Error, format_err};
use argh::FromArgs;
use fidl::endpoints;
use fuchsia_component::client;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use fuchsia_runtime::{HandleInfo, HandleType};
use futures::{FutureExt, StreamExt, TryStreamExt, select};
use log::*;
use persist_server::PersistServer;
use persistence_build_config::Config as BuildConfig;
use persistence_config::Config;
use scheduler::Scheduler;
use std::pin::pin;
use zx::BootInstant;
use {
    fidl_fuchsia_component_sandbox as fsandbox, fidl_fuchsia_process_lifecycle as flifecycle,
    fidl_fuchsia_update as fupdate, fuchsia_async as fasync,
};

/// The name of the subcommand and the logs-tag.
pub const PROGRAM_NAME: &str = "persistence";
pub const PERSIST_NODE_NAME: &str = "persist";
/// Added after persisted data is fully published
pub const PUBLISHED_TIME_KEY: &str = "published";

/// Command line args
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "persistence")]
pub struct CommandLine {}

pub async fn main(_args: CommandLine) -> Result<(), Error> {
    info!("Starting Diagnostics Persistence Service service");
    let lifecycle =
        fuchsia_runtime::take_startup_handle(HandleInfo::new(HandleType::Lifecycle, 0)).unwrap();
    let lifecycle: zx::Channel = lifecycle.into();
    let lifecycle: endpoints::ServerEnd<flifecycle::LifecycleMarker> = lifecycle.into();
    let (mut lifecycle_request_stream, _) = lifecycle.into_stream_and_control_handle();
    let mut lifecycle_task = pin!(
        async move {
            match lifecycle_request_stream.next().await {
                Some(Ok(flifecycle::LifecycleRequest::Stop { .. })) => {
                    debug!("Received stop request");
                }
                Some(Err(e)) => {
                    error!("Received FIDL error from Lifecycle: {e:?}");
                    std::future::pending::<()>().await
                }
                None => {
                    debug!("Lifecycle request stream closed");
                    std::future::pending::<()>().await
                }
            }
        }
        .fuse()
    );

    let mut health = component::health();
    let config = persistence_config::load_configuration_files().context("Error loading configs")?;
    let build_config = BuildConfig::take_from_startup_handle();
    let inspector = component::inspector();
    inspector.root().record_child("config", |config_node| build_config.record_inspect(config_node));
    let _inspect_server_task =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default());

    file_handler::forget_old_data(&config);

    let scope = fasync::Scope::new();
    let scheduler =
        Scheduler::new(scope.to_handle(), &config).context("Error creating scheduler")?;

    // Add a persistence fidl service for each service defined in the config files.
    let scope = fasync::Scope::new();
    let (outgoing_dir_task, service_scope) =
        spawn_persist_services(&config, scheduler).await.expect("Error spawning persist services");

    // Before serving previous data, wait until the post-boot system update check has finished.
    // Note: We're already accepting persist requests. If we receive a request, store
    // some data, and then cache is cleared after data is persisted, that data will be lost. This
    // is correct behavior - we don't want to remember anything from before the cache was cleared.
    scope.spawn(async move {
        if build_config.skip_update_check {
            info!("Skipping the update check, publishing previous boot data");
        } else if let Err(e) = wait_for_update().await {
            warn!(e:?; "Will not publish previous boot data");
            return;
        }

        inspector.root().record_child(PERSIST_NODE_NAME, |node| {
            inspect_server::serve_persisted_data(node);
            health.set_ok();
            info!("Diagnostics Persistence Service ready");
        });
        inspector.root().record_int(PUBLISHED_TIME_KEY, BootInstant::get().into_nanos());
    });

    let mut outgoing_dir_task = outgoing_dir_task.fuse();

    select! {
        _ = lifecycle_task => {
            info!("Stopping due to lifecycle request");
            service_scope.cancel().await;
            scope.cancel().await;
        },
        _ = outgoing_dir_task => {
            info!("Stopping due to idle activity");
            service_scope.cancel().await;
            scope.join().await;
        },
    }

    Ok(())
}

async fn wait_for_update() -> Result<(), Error> {
    info!("Waiting for post-boot update check...");
    let (notifier_client, mut notifier_request_stream) =
        fidl::endpoints::create_request_stream::<fupdate::NotifierMarker>();
    match fuchsia_component::client::connect_to_protocol::<fupdate::ListenerMarker>() {
        Ok(proxy) => {
            proxy.notify_on_first_update_check(
                fupdate::ListenerNotifyOnFirstUpdateCheckRequest {
                    notifier: Some(notifier_client),
                    ..Default::default()
                },
            )?;
        }
        Err(e) => {
            warn!(
                e:?;
                "Unable to connect to fuchsia.update.Listener; will publish immediately"
            );

            return Ok(());
        }
    }

    match notifier_request_stream.try_next().await {
        Ok(Some(fupdate::NotifierRequest::Notify { control_handle: _ })) => {}
        Ok(None) => {
            return Err(anyhow::anyhow!("Did not receive update notification; not publishing"));
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Error waiting for update notification; not publishing: {e}"
            ));
        }
    }

    // Start serving previous boot data
    info!("...Update check has completed; publishing previous boot data");
    Ok(())
}

enum IncomingRequest {
    Router(fsandbox::DictionaryRouterRequestStream),
}

// Serve a DataPersistence capability for each service defined in `config` using
// a dynamic dictionary.
async fn spawn_persist_services(
    config: &Config,
    scheduler: Scheduler,
) -> Result<(impl Future<Output = ()>, fasync::Scope), Error> {
    let store = client::connect_to_protocol::<fsandbox::CapabilityStoreMarker>().unwrap();
    let id_gen = sandbox::CapabilityIdGenerator::new();

    let services_dict = id_gen.next();
    store
        .dictionary_create(services_dict)
        .await
        .context("Failed to send FIDL to create dictionary")?
        .map_err(|e| format_err!("Failed to create dictionary: {e:?}"))?;

    let service_scope = fasync::Scope::new();
    for service_name in config.keys() {
        let connector_id = id_gen.next();
        let (receiver, receiver_stream) =
            endpoints::create_request_stream::<fsandbox::ReceiverMarker>();

        store
            .connector_create(connector_id, receiver)
            .await
            .context("Failed to send FIDL to create connector")?
            .map_err(|e| format_err!("Failed to create connector: {e:?}"))?;

        store
            .dictionary_insert(
                services_dict,
                &fsandbox::DictionaryItem {
                    key: format!("{}-{}", constants::PERSIST_SERVICE_NAME_PREFIX, service_name),
                    value: connector_id,
                },
            )
            .await
            .context(
                "Failed to send FIDL to insert into diagnostics-persist-capabilities dictionary",
            )?
            .map_err(|e| {
                format_err!(
                    "Failed to insert into diagnostics-persist-capabilities dictionary: {e:?}"
                )
            })?;

        PersistServer::spawn(
            service_name.clone(),
            scheduler.clone(),
            service_scope.to_handle(),
            receiver_stream,
        );
    }

    // Expose the dynamic dictionary.
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(IncomingRequest::Router);
    fs.take_and_serve_directory_handle().expect("Failed to take service directory handle");
    let outgoing_dir_task =
        fs.for_each_concurrent(None, move |IncomingRequest::Router(mut stream)| {
            let store = store.clone();
            let id_gen = id_gen.clone();
            async move {
                while let Ok(Some(request)) = stream.try_next().await {
                    match request {
                        fsandbox::DictionaryRouterRequest::Route { payload: _, responder } => {
                            let dup_dict_id = id_gen.next();
                            store.duplicate(services_dict, dup_dict_id).await.unwrap().unwrap();
                            let capability = store.export(dup_dict_id).await.unwrap().unwrap();
                            let fsandbox::Capability::Dictionary(dict) = capability else {
                                panic!("capability was not a dictionary? {capability:?}");
                            };
                            let _ = responder.send(Ok(
                                fsandbox::DictionaryRouterRouteResponse::Dictionary(dict),
                            ));
                        }
                        fsandbox::DictionaryRouterRequest::_UnknownMethod { ordinal, .. } => {
                            warn!(ordinal:%; "Unknown DictionaryRouter request");
                        }
                    }
                }
            }
        });

    Ok((outgoing_dir_task, service_scope))
}
