// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `diagnostics-persistence` component persists Inspect VMOs and serves them at the next boot.

mod fetcher;
mod file_handler;
mod inspect_server;
mod scheduler;

use anyhow::{Context, Error};
use argh::FromArgs;
use fidl::endpoints;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use fuchsia_runtime::{HandleInfo, HandleType};
use futures::{StreamExt, TryStreamExt};
use log::*;
use persistence_build_config::Config as BuildConfig;
use scheduler::Scheduler;
use zx::BootInstant;
use {
    fidl_fuchsia_process_lifecycle as flifecycle, fidl_fuchsia_update as fupdate,
    fuchsia_async as fasync,
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
    let lifecycle_task = async move {
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
    };

    let mut health = component::health();
    let config = persistence_config::load_configuration_files().context("Error loading configs")?;
    let build_config = BuildConfig::take_from_startup_handle();
    let inspector = component::inspector();
    inspector.root().record_child("config", |config_node| build_config.record_inspect(config_node));
    let _inspect_server_task =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default());

    file_handler::forget_old_data(&config)?;

    // Add a persistence fidl service for each service defined in the config files.
    let scope = fasync::Scope::new();
    Scheduler::spawn(scope.to_handle(), &config).await.context("Error creating scheduler")?;

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
            if let Err(e) = inspect_server::serve_persisted_data(node) {
                error!("Failed to serve persisted data: {e}");
            }
            health.set_ok();
            info!("Diagnostics Persistence Service ready");
        });
        inspector.root().record_int(PUBLISHED_TIME_KEY, BootInstant::get().into_nanos());
    });

    lifecycle_task.await;
    info!("Stopping due to lifecycle request");
    scope.cancel().await;

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
