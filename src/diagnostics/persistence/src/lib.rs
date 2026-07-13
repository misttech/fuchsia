// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `diagnostics-persistence` component persists Inspect VMOs and serves them at the next boot.

mod fetcher;
mod file_handler;
mod inspect_server;
mod scheduler;

use anyhow::{Context, Error, anyhow};
use argh::FromArgs;
use fidl::endpoints::ControlHandle;
use fidl_fuchsia_component_sandbox as fsandbox;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_inspect as finspect;
use fidl_fuchsia_update as fupdate;
use fuchsia_async as fasync;
use fuchsia_component::escrow::EscrowOperation;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use fuchsia_runtime::{HandleInfo, HandleType};
use fuchsia_sync::Mutex;
use futures::{StreamExt, TryStreamExt};
use log::*;
use persistence_build_config::Config;
use sandbox::CapabilityRef;
use scheduler::Scheduler;
use serde::{Deserialize, Serialize};
use std::pin::pin;
use std::sync::{Arc, LazyLock};
use zx::BootInstant;

/// The name of the subcommand and the logs-tag.
pub const PROGRAM_NAME: &str = "persistence";
pub const PERSIST_NODE_NAME: &str = "persist";
/// Added after persisted data is fully published
pub const PUBLISHED_TIME_KEY: &str = "published";

/// Key in escrowed dictionary to immutable state persisted across instances of
/// this component across the same boot.
const INSTANCE_STATE_KEY: &str = "InstanceState";
/// Key in escrowed dictionary to frozen Inspect VMO.
const FROZEN_INSPECT_VMO_KEY: &str = "FrozenInspectVMO";

/// Parsed CML structured configuration.
#[derive(Clone, Debug)]
pub(crate) struct BuildConfig {
    /// If true, don't wait for a successful update check before publishing
    /// previous boot's persisted Inspect data.
    skip_update_check: bool,
    /// Duration to wait for FIDL requests before stalling the connection.
    stall_interval: zx::MonotonicDuration,
}

/// Build config, as defined by the CML structured configuration.
pub(crate) static BUILD_CONFIG: LazyLock<BuildConfig> = LazyLock::new(|| {
    let config = Config::take_from_startup_handle();
    component::inspector().root().record_child("config", |node| config.record_inspect(node));

    let Config { skip_update_check, stop_on_idle_timeout_millis } = config;

    if skip_update_check {
        info!("Configured to skip update check");
    }

    let stall_interval = if stop_on_idle_timeout_millis >= 0 {
        info!("Configured to idle after {stop_on_idle_timeout_millis}ms of inactivity");
        zx::MonotonicDuration::from_millis(stop_on_idle_timeout_millis)
    } else {
        info!("Not configured to idle after inactivity");
        zx::MonotonicDuration::INFINITE
    };

    BuildConfig { skip_update_check, stall_interval }
});

/// Command line args
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "persistence")]
pub struct CommandLine {}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum UpdateCheckStage {
    /// Waiting for the first update check before publishing previous boot
    /// inspect data.
    Waiting,
    /// First update check was skipped, previous boot inspect data has been published.
    Skipped,
    /// First update check has completed, previous boot inspect data has been
    /// published.
    Done,
    /// Unable to subscribe to the first update check.
    Error,
}
/// State to be persisted between instances of this component across
/// the same boot.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    /// Persistence config loaded from disk.
    config: persistence_config::Config,
    /// Stage of the update check.
    update_stage: Mutex<UpdateCheckStage>,
}

enum InspectState {
    /// Inspect data is actively being served by this component instance.
    Active(inspect_runtime::PublishedInspectController),
    /// Inspect data is escrowed with the Component Framework.
    ///
    /// Archivist monitors this handle for OBJECT_PEER_CLOSED. If the handle is dropped, the
    /// Archivist removes the escrowed Inspect data. By preserving it here, we ensure data
    /// availability even when we restart but skip active republication.
    Escrowed(zx::NullableHandle),
}

/// All component-specific state.
#[derive(Clone)]
struct ComponentState {
    /// State persisted across instances of this component.
    persisted: Arc<PersistedState>,
    /// Listener for Sample
    scheduler: Scheduler,
    /// Shared state for Inspect data.
    inspect: Arc<Mutex<Option<InspectState>>>,
}

impl ComponentState {
    /// Load state from a previous instance if possible, otherwise initialize
    /// new state.
    async fn load(
        scope: fasync::ScopeHandle,
        store: &sandbox::CapabilityStore,
    ) -> Result<Self, Error> {
        if let Some(dictionary) =
            fuchsia_runtime::take_startup_handle(HandleInfo::new(HandleType::EscrowedDictionary, 0))
        {
            debug!("Loading component state from escrowed dictionary");
            return ComponentState::from_escrow(scope.clone(), store, dictionary)
                .await
                .context("Failed to load component state from escrowed dictionary");
        }

        debug!("No escrowed dictionary available; generating one");
        Self::new(scope.clone()).await.context("Failed to create component state")
    }

    async fn new(scope: fasync::ScopeHandle) -> Result<Self, Error> {
        let inspect_controller = inspect_runtime::publish(
            component::inspector(),
            inspect_runtime::PublishOptions::default().custom_scope(scope.clone()),
        )
        .ok_or_else(|| anyhow!("failed to publish inspect"))?;

        let config =
            persistence_config::load_configuration_files().context("Error loading configs")?;
        file_handler::forget_old_data(&config).await?;

        let scheduler = Scheduler::new(&config);
        scheduler
            .subscribe(scope.clone(), &config)
            .await
            .context("Failed to subscribe to fuchsia.diagnostics.Sample")?;

        let persisted = {
            let update_stage = if BUILD_CONFIG.skip_update_check {
                UpdateCheckStage::Skipped
            } else {
                UpdateCheckStage::Waiting
            };
            Arc::new(PersistedState { config, update_stage: Mutex::new(update_stage) })
        };

        if BUILD_CONFIG.skip_update_check {
            publish_inspect_data().await;
        } else {
            // Listen for the first update check.
            let notifier_client = {
                let (notifier_client, notifier_request_stream) =
                    fidl::endpoints::create_request_stream::<fupdate::NotifierMarker>();
                let persisted = persisted.clone();
                scope.spawn(async move {
                    if let Err(e) = handle_update_done(notifier_request_stream, persisted).await {
                        error!("Failed to handle NotifierRequest: {e}");
                    }
                });
                notifier_client
            };

            match fuchsia_component::client::connect_to_protocol::<fupdate::ListenerMarker>() {
                Ok(proxy) => {
                    if let Err(e) = proxy.notify_on_first_update_check(
                        fupdate::ListenerNotifyOnFirstUpdateCheckRequest {
                            notifier: Some(notifier_client),
                            ..Default::default()
                        },
                    ) {
                        error!("Error subscribing to first update check; not publishing: {e:?}");
                        *persisted.update_stage.lock() = UpdateCheckStage::Error;
                    }
                }
                Err(e) => {
                    // TODO(https://fxbug.dev/444526593): Consider bailing
                    // if the update checker is not available.
                    warn!(e:?; "Unable to connect to fuchsia.update.Listener; will publish immediately");
                    *persisted.update_stage.lock() = UpdateCheckStage::Done;
                }
            }
        }

        Ok(Self {
            persisted,
            scheduler,
            inspect: Arc::new(Mutex::new(Some(InspectState::Active(inspect_controller)))),
        })
    }

    async fn from_escrow<'a>(
        scope: fasync::ScopeHandle,
        store: &'a sandbox::CapabilityStore,
        dictionary: zx::NullableHandle,
    ) -> Result<Self, Error> {
        let dict = store
            .import(fsandbox::DictionaryRef { token: dictionary.into() })
            .await
            .context("Error importing from component startup handle")?;

        let persisted_bytes = dict
            .get::<sandbox::Data<'a>>(INSTANCE_STATE_KEY)
            .await
            .context("Error getting instance state")?
            .export::<Vec<u8>>()
            .await
            .context("Error exporting as buffer")?;
        let persisted: PersistedState = ciborium::from_reader(&persisted_bytes[..])
            .context("Failed to deserialize InstanceState")?;
        let update_stage = persisted.update_stage.lock().clone();

        let escrow_token = dict
            .get::<sandbox::Handle<'a>>(FROZEN_INSPECT_VMO_KEY)
            .await
            .context("Failed to get frozen Inspect VMO")?
            .export::<zx::NullableHandle>()
            .await
            .context("Failed to export handle")?;

        let inspect = match update_stage {
            UpdateCheckStage::Waiting | UpdateCheckStage::Error => {
                // Create a new, writable Inspect tree. The previous instance of
                // Persistence did not receive the signal to persist data from
                // the last boot, but this instance might.
                let escrow_token =
                    finspect::EscrowToken { token: zx::EventPair::from(escrow_token) };

                // Swap escrowed Inspect data with a new Tree server.
                let inspect_runtime::FetchEscrowResult { vmo: _, server } =
                    inspect_runtime::fetch_escrow(
                        escrow_token,
                        inspect_runtime::FetchEscrowOptions::new().replace_with_tree(),
                    )
                    .await
                    .context("Failed to fetch escrowed Inspect data")?;

                let opts = inspect_runtime::PublishOptions::default()
                    .custom_scope(scope.clone())
                    .on_tree_server(server.context("FetchEscrow did not return a TreeHandle")?);

                let inspect_controller = inspect_runtime::publish(component::inspector(), opts)
                    .context("Failed to publish Inspect data")?;

                InspectState::Active(inspect_controller)
            }
            UpdateCheckStage::Done | UpdateCheckStage::Skipped => {
                // Persistence has already published persisted data from last
                // boot. By not republishing, the existing frozen Inspect data
                // remains published.
                //
                // Persistence needs to continue running to record data to
                // persist for the next boot.
                InspectState::Escrowed(escrow_token)
            }
        };

        // Do not spawn FIDL request handlers when returning from escrow. The
        // previous component instance escrowed its request streams, sending
        // them to the Component Framework. When an incoming request is received
        // on escrowed channels held by the Component Framework, it will be
        // routed to this instance's incoming namespace (via IncomingRequest)
        // then this instance will spawn new request handlers.

        Ok(Self {
            scheduler: Scheduler::new(&persisted.config),
            persisted: Arc::new(persisted),
            inspect: Arc::new(Mutex::new(Some(inspect))),
        })
    }

    async fn as_escrowed_dict(
        store: &sandbox::CapabilityStore,
        persisted: impl AsRef<PersistedState>,
        inspect: Arc<Mutex<Option<InspectState>>>,
    ) -> Result<fsandbox::DictionaryRef, Error> {
        let dict = store.create_dictionary().await?;

        // Save PersistedState
        let mut persisted_bytes: Vec<u8> = Vec::new();
        ciborium::into_writer(persisted.as_ref(), &mut persisted_bytes)
            .context("Failed to serialize InstanceState")?;
        let data = store.import(persisted_bytes).await?;
        dict.insert(INSTANCE_STATE_KEY, data).await?;

        // Save frozen Inspect VMO.
        let inspect = inspect.lock().take();
        match inspect {
            Some(InspectState::Active(inspect_controller)) => {
                match inspect_controller
                    .escrow_frozen(inspect_runtime::EscrowOptions::default())
                    .await
                {
                    Ok(escrow_token) => {
                        let handle = escrow_token.token.into_handle();
                        let data = store.import(handle).await?;
                        dict.insert(FROZEN_INSPECT_VMO_KEY, data).await?;
                    }
                    Err(e) => {
                        error!("Failed to escrow frozen Inspect VMO: {e:?}");
                    }
                }
            }
            Some(InspectState::Escrowed(handle)) => {
                let data = store.import(handle).await?;
                dict.insert(FROZEN_INSPECT_VMO_KEY, data).await?;
            }
            None => {}
        }

        dict.export().await.context("Failed to export escrowed dictionary")
    }
}

/// Handle fuchsia.update/Notifier requests. Notifies of when an update check
/// has been completed, signaling this component to publish persisted data to
/// Inspect.
async fn handle_update_done(
    stream: fupdate::NotifierRequestStream,
    persisted: Arc<PersistedState>,
) -> Result<(), Error> {
    let (stream, stalled) = detect_stall::until_stalled(stream, BUILD_CONFIG.stall_interval);
    let mut stream = pin!(stream);
    if let Ok(Some(request)) = stream.try_next().await {
        debug!("Received fuchsia.update.NotifierRequest");
        match request {
            fupdate::NotifierRequest::Notify { control_handle } => {
                debug!("Received notification that the update check has completed");
                let stage = persisted.update_stage.lock().clone();
                match stage {
                    UpdateCheckStage::Skipped | UpdateCheckStage::Error => {
                        unreachable!("Received impossible notification")
                    }
                    UpdateCheckStage::Waiting => {
                        *persisted.update_stage.lock() = UpdateCheckStage::Done;
                        info!("...Update check has completed; publishing previous boot data");
                        publish_inspect_data().await;
                        control_handle.shutdown();
                        return Ok(());
                    }
                    UpdateCheckStage::Done => {
                        debug!("Ignoring update check notification; already received one");
                        control_handle.shutdown();
                        return Ok(());
                    }
                }
            }
        }
    }
    if let Ok(Some(server_end)) = stalled.await {
        // Send the server endpoint back to the framework.
        debug!("Escrowing fuchsia.update.Notifier");
        fuchsia_component::client::connect_channel_to_protocol_at_path(
            server_end,
            "/escrow/fuchsia.update.Notifier",
        )
        .context("Failed to connect to fuchsia.update.Notifier")?;
    }
    Ok(())
}

async fn publish_inspect_data() {
    // TODO(https://fxbug.dev/444525059): Set health properly.
    component::health().set_ok();
    if let Err(e) = inspect_server::record_persist_node(PERSIST_NODE_NAME).await {
        error!("Failed to serve persisted Inspect data from previous boot: {e}");
    }
    component::inspector().root().record_int(PUBLISHED_TIME_KEY, BootInstant::get().into_nanos());
}

enum IncomingRequest {
    UpdateDone(fupdate::NotifierRequestStream),
    SampleSink(fdiagnostics::SampleSinkRequestStream),
}

pub async fn main(_args: CommandLine) -> Result<(), Error> {
    info!("Starting Diagnostics Persistence service");
    // initialize to 5MiB
    component::init_inspector_with_size(1024 * 1024 * 5);
    let scope = fasync::Scope::new();
    let store = sandbox::CapabilityStore::connect()?;
    let state = ComponentState::load(scope.to_handle(), &store)
        .await
        .context("Error getting escrowed state")?;
    component::health().set_starting_up();

    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(IncomingRequest::UpdateDone);
    fs.dir("svc").add_fidl_service(IncomingRequest::SampleSink);
    fs.take_and_serve_directory_handle().expect("Failed to take service directory handle");

    let escrow_operation = EscrowOperation::new();
    escrow_operation.watch_for_stop().context("Failed to watch for stop on lifecycle handle")?;

    let outgoing_dir_task =
        pin!(fs.until_stalled(BUILD_CONFIG.stall_interval).for_each_concurrent(None, move |item| {
            let escrow_operation = escrow_operation.clone();
            let state = state.clone();
            let store = store.clone();
            async move {
                match item {
                    fuchsia_component::server::Item::Request(req, _active_guard) => match req {
                        IncomingRequest::UpdateDone(stream) => {
                            if let Err(e) = handle_update_done(stream, state.persisted).await {
                                error!("Failed to handle NotifierRequest: {e}");
                            }
                        },
                        IncomingRequest::SampleSink(stream) => {
                            if let Err(e) = state.scheduler.handle_sample_sink(stream).await {
                                error!("Failed to handle SampleSinkRequest: {e}");
                            }
                        },
                    },
                    fuchsia_component::server::Item::Stalled(outgoing_directory) => {
                        match ComponentState::as_escrowed_dict(
                            &store,
                            state.persisted,
                            state.inspect,
                        ).await {
                            Ok(dict) => escrow_operation.with_fsandbox_dictionary(dict),
                            Err(e) => {
                                error!(
                                    "Failed to serialize PersistedState into component dictionary: {e}"
                                );
                            }
                        };
                        escrow_operation.run(outgoing_directory.into()).expect("failed to escrow handles");
                    }
                }
            }
        }));

    outgoing_dir_task.await;
    info!("Stopping due to idle activity");
    scope.join().await;

    Ok(())
}
