// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Spawns and supervises dynamic charge governor child components in the `#governors` collection.
//!
//! Each configured governor receives its own `fuchsia.hardware.power.charger.Controller`,
//! `fuchsia.hardware.power.battery.Battery`, and `fuchsia.hardware.power.charger.Charger`
//! connectors in `CreateChildArgs.additional_inputs` so that [`ChargeLimitArbiter`] and
//! [`Cache`] attribute votes and status connections to the governor's child name. If a
//! governor stops or fails to create/start, it is restarted with capped exponential backoff.

use crate::Backoff;
use crate::arbiter::ChargeLimitArbiter;
use crate::cache::Cache;
use fidl::endpoints::{DiscoverableProtocolMarker, ServerEnd};
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_hardware_power_battery as fbattery;
use fidl_fuchsia_hardware_power_charger as fcharger;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_component::runtime::{Connector, ConnectorReceiver, Dictionary};
use fuchsia_inspect::{self as inspect, NumericProperty as _, Property as _};
use futures::prelude::*;
use log::{error, info, warn};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Initial delay before restarting a charge governor that stopped or failed to start.
const GOVERNOR_RESTART_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);

const GOVERNORS_COLLECTION: &str = "governors";
const INSPECTOR_NODE_GOVERNORS: &str = "governors";
const INSPECTOR_PROP_GOVERNOR_URL: &str = "url";
const INSPECTOR_PROP_GOVERNOR_RUNNING: &str = "running";
const INSPECTOR_PROP_GOVERNOR_RESTARTS: &str = "restarts";

/// Spawns a supervision task on a new [`fasync::Scope`] for each configured governor URL and
/// records its Inspect node under `governors/<child_name>`.
pub(crate) fn spawn_configured_governors(
    governor_urls: &[String],
    arbiter: &Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    inspect_root: &inspect::Node,
) -> fasync::Scope {
    let scope = fasync::Scope::new();
    if governor_urls.is_empty() {
        return scope;
    }

    let realm = match fclient::realm() {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to connect to Realm service; cannot spawn governors: {e:?}");
            return scope;
        }
    };

    spawn_configured_governors_with_realm(governor_urls, realm, arbiter, cache, inspect_root, scope)
}

/// Spawns a supervision task on `scope` for each URL in `governor_urls` using `realm` and records
/// its Inspect node under `governors/<child_name>`.
fn spawn_configured_governors_with_realm(
    governor_urls: &[String],
    realm: fcomponent::RealmProxy,
    arbiter: &Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    inspect_root: &inspect::Node,
    scope: fasync::Scope,
) -> fasync::Scope {
    if governor_urls.is_empty() {
        return scope;
    }

    inspect_root.record_child(INSPECTOR_NODE_GOVERNORS, |governors_node| {
        let mut used_names = HashSet::new();
        for (idx, url) in governor_urls.iter().enumerate() {
            let child_name = governor_child_name(url, idx, &mut used_names);
            let inspect_node = governors_node.create_child(&child_name);
            let _ = scope.spawn_local(supervise_governor(
                scope.to_handle(),
                realm.clone(),
                arbiter.clone(),
                cache.clone(),
                child_name,
                url.clone(),
                inspect_node,
                GOVERNOR_RESTART_DELAY,
            ));
        }
    });
    scope
}

/// Returns the child name for the governor at `url` (its manifest name, such as `msc` for
/// `...#meta/msc.cm`), suffixed with its index if another governor already uses that name.
fn governor_child_name(url: &str, idx: usize, used: &mut HashSet<String>) -> String {
    let base_name = url
        .split_once('#')
        .and_then(|(_, fragment)| fragment.strip_prefix("meta/"))
        .and_then(|s| s.strip_suffix(".cm"))
        .unwrap_or("governor");
    let name =
        if used.contains(base_name) { format!("{base_name}_{idx}") } else { base_name.to_owned() };
    used.insert(name.clone());
    name
}

/// Runs governor `child_name` from `url` in the `#governors` collection forever, restarting it
/// with capped exponential backoff starting at `restart_delay` whenever it stops or cannot be
/// created or started. Records its `url`, `running` state, and `restarts` count under
/// `inspect_node`.
async fn supervise_governor(
    scope: fasync::ScopeHandle,
    realm: fcomponent::RealmProxy,
    arbiter: Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    child_name: String,
    url: String,
    inspect_node: inspect::Node,
    restart_delay: zx::MonotonicDuration,
) {
    inspect_node.record_string(INSPECTOR_PROP_GOVERNOR_URL, &url);
    let running = inspect_node.create_bool(INSPECTOR_PROP_GOVERNOR_RUNNING, false);
    let restarts = inspect_node.create_uint(INSPECTOR_PROP_GOVERNOR_RESTARTS, 0);
    let collection_ref = fdecl::CollectionRef { name: GOVERNORS_COLLECTION.into() };
    let child_decl = fdecl::Child {
        name: Some(child_name.clone()),
        url: Some(url.clone()),
        startup: Some(fdecl::StartupMode::Lazy),
        environment: None,
        ..Default::default()
    };
    let child_ref =
        fdecl::ChildRef { name: child_name.clone(), collection: Some(GOVERNORS_COLLECTION.into()) };
    let mut backoff = Backoff::new(restart_delay);
    loop {
        run_governor_attempt(
            &scope,
            &realm,
            &arbiter,
            &cache,
            &collection_ref,
            &child_decl,
            &child_name,
            &url,
            &running,
        )
        .await;

        let _ = realm.destroy_child(&child_ref).await;
        restarts.add(1);
        backoff.wait().await;
    }
}

/// Creates and starts a single governor instance in `#governors` and waits until its
/// `ExecutionController` reports `OnStop` (or closes).
#[allow(clippy::too_many_arguments)]
async fn run_governor_attempt(
    scope: &fasync::ScopeHandle,
    realm: &fcomponent::RealmProxy,
    arbiter: &Arc<ChargeLimitArbiter>,
    cache: &Rc<Cache>,
    collection_ref: &fdecl::CollectionRef,
    child_decl: &fdecl::Child,
    child_name: &str,
    url: &str,
    running: &inspect::BoolProperty,
) {
    let (comp_ctrl_proxy, comp_ctrl_server) =
        fidl::endpoints::create_proxy::<fcomponent::ControllerMarker>();
    let inputs =
        create_governor_inputs(scope, arbiter.clone(), cache.clone(), child_name.to_owned()).await;
    let child_args = fcomponent::CreateChildArgs {
        controller: Some(comp_ctrl_server),
        additional_inputs: Some(inputs.handle),
        ..Default::default()
    };

    info!("Spawning charge governor {child_name} ({url}) into collection #{GOVERNORS_COLLECTION}");
    match realm.create_child(collection_ref, child_decl, child_args).await {
        Ok(Ok(())) => {}
        Ok(Err(fcomponent::Error::InstanceAlreadyExists)) => {
            warn!("Charge governor {child_name} already exists; destroying before retry");
            return;
        }
        Ok(Err(e)) => {
            error!("Failed to spawn charge governor {child_name} ({url}): {e:?}");
            return;
        }
        Err(e) => {
            error!("FIDL transport error spawning charge governor {child_name} ({url}): {e:?}");
            return;
        }
    }

    let (exec_proxy, exec_server) =
        fidl::endpoints::create_proxy::<fcomponent::ExecutionControllerMarker>();
    match comp_ctrl_proxy.start(fcomponent::StartChildArgs::default(), exec_server).await {
        Ok(Ok(())) => {
            info!("Successfully started charge governor {child_name}");
            running.set(true);
            let mut events = exec_proxy.take_event_stream();
            while let Some(event) = events.next().await {
                if let Ok(fcomponent::ExecutionControllerEvent::OnStop { stopped_payload }) = event
                {
                    warn!(
                        "Charge governor {child_name} stopped ({stopped_payload:?}); \
                         restarting after backoff"
                    );
                    break;
                }
            }
            running.set(false);
        }
        Ok(Err(e)) => {
            warn!("Failed to start charge governor {child_name} ({url}): {e:?}");
        }
        Err(e) => {
            warn!("FIDL transport error starting charge governor {child_name} ({url}): {e:?}");
        }
    }
}

/// Creates the input dictionary for governor `child_name`, holding:
/// - a `fuchsia.hardware.power.charger.Controller` connector whose connections `arbiter` serves
///   as `child_name`'s votes;
/// - `fuchsia.hardware.power.battery.Battery` and `fuchsia.hardware.power.charger.Charger`
///   connectors whose connections `cache` serves from battery-manager's cached status.
async fn create_governor_inputs(
    scope: &fasync::ScopeHandle,
    arbiter: Arc<ChargeLimitArbiter>,
    cache: Rc<Cache>,
    child_name: String,
) -> Dictionary {
    let (connector, receiver) = Connector::new().await;
    let inputs = Dictionary::new().await;
    inputs.insert(fcharger::ControllerMarker::PROTOCOL_NAME, connector).await;
    let _ = scope.spawn_local(serve_governor_controller(arbiter, child_name.clone(), receiver));
    let (battery_connector, battery_receiver) = Connector::new().await;
    inputs.insert(fbattery::BatteryMarker::PROTOCOL_NAME, battery_connector).await;
    let (charger_connector, charger_receiver) = Connector::new().await;
    inputs.insert(fcharger::ChargerMarker::PROTOCOL_NAME, charger_connector).await;
    let _ = scope.spawn_local(serve_governor_cache(
        cache,
        child_name,
        battery_receiver,
        charger_receiver,
    ));
    inputs
}

/// Serves each `Controller` connection that governor `child_name` opens through its connector,
/// voting under `child_name`. Runs until the connector is dropped along with the governor.
async fn serve_governor_controller(
    arbiter: Arc<ChargeLimitArbiter>,
    child_name: String,
    receiver: ConnectorReceiver,
) {
    receiver
        .for_each_concurrent(None, |channel| {
            let arbiter = arbiter.clone();
            let child_name = child_name.clone();
            async move {
                let stream = ServerEnd::<fcharger::ControllerMarker>::new(channel).into_stream();
                if let Err(e) = arbiter.serve_controller(child_name.clone(), stream).await {
                    error!("Charger Controller for governor {child_name} failed: {e:?}");
                }
            }
        })
        .await;
}

/// Serves each `Battery` and `Charger` connection that governor `child_name` opens through its
/// connectors from `cache`. Runs until the connectors are dropped along with the governor.
async fn serve_governor_cache(
    cache: Rc<Cache>,
    child_name: String,
    battery_receiver: ConnectorReceiver,
    charger_receiver: ConnectorReceiver,
) {
    let cache = &*cache;
    let child_name = child_name.as_str();
    let battery = battery_receiver.for_each_concurrent(None, |channel| async move {
        let stream = ServerEnd::<fbattery::BatteryMarker>::new(channel).into_stream();
        if let Err(e) = cache.serve_battery(child_name, stream).await
            && !e.is_closed()
        {
            warn!("Battery connection for governor {child_name} failed: {e:?}");
        }
    });
    let charger = charger_receiver.for_each_concurrent(None, |channel| async move {
        let stream = ServerEnd::<fcharger::ChargerMarker>::new(channel).into_stream();
        if let Err(e) = cache.serve_charger(child_name, stream).await
            && !e.is_closed()
        {
            warn!("Charger connection for governor {child_name} failed: {e:?}");
        }
    });
    futures::join!(battery, charger);
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;

    /// Verifies that `governor_child_name` extracts the manifest stem and suffixes duplicate
    /// names with their index.
    #[fuchsia::test]
    fn test_governor_child_name_uses_manifest_name_and_dedupes() {
        let mut used = HashSet::new();
        assert_eq!(
            governor_child_name("fuchsia-pkg://fuchsia.com/msc#meta/msc.cm", 0, &mut used),
            "msc"
        );
        assert_eq!(
            governor_child_name("fuchsia-boot:///thermal#meta/thermal.cm", 1, &mut used),
            "thermal"
        );
        assert_eq!(
            governor_child_name("fuchsia-pkg://fuchsia.com/msc2#meta/msc.cm", 2, &mut used),
            "msc_2"
        );
        assert_eq!(governor_child_name("fuchsia-pkg://fuchsia.com/bad", 3, &mut used), "governor");
    }

    /// Verifies that `spawn_configured_governors` is a no-op and does not record a `governors`
    /// Inspect node when `governor_urls` is empty.
    #[fuchsia::test]
    async fn test_spawn_configured_governors_empty_is_noop() {
        let inspector = inspect::Inspector::default();
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let cache = Rc::new(Cache::default());
        let _scope = spawn_configured_governors(&[], &arbiter, cache, inspector.root());
        assert_data_tree!(inspector, root: {});
    }

    /// Verifies that `supervise_governor` creates and starts a child governor via `Realm`, sets
    /// `running = true` in Inspect while running, and destroys the child, sets `running = false`,
    /// increments `restarts`, and retries when `OnStop` fires.
    #[fuchsia::test]
    async fn test_supervise_governor_starts_tracks_inspect_and_restarts_on_stop() {
        let inspector = inspect::Inspector::default();
        let inspect_node = inspector.root().create_child("msc");
        let arbiter = Arc::new(ChargeLimitArbiter::new());
        let cache = Rc::new(Cache::default());
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<fcomponent::RealmMarker>();

        let scope = fasync::Scope::new();
        let _task = scope.spawn_local(supervise_governor(
            scope.to_handle(),
            realm_proxy,
            arbiter,
            cache,
            "msc".to_string(),
            "fuchsia-pkg://fuchsia.com/msc#meta/msc.cm".to_string(),
            inspect_node,
            zx::MonotonicDuration::ZERO,
        ));

        // 1. Expect CreateChild.
        let comp_ctrl_server = match realm_stream.next().await.unwrap().unwrap() {
            fcomponent::RealmRequest::CreateChild { collection, decl, args, responder } => {
                assert_eq!(collection.name, GOVERNORS_COLLECTION);
                assert_eq!(decl.name.as_deref(), Some("msc"));
                assert_eq!(decl.url.as_deref(), Some("fuchsia-pkg://fuchsia.com/msc#meta/msc.cm"));
                assert!(args.additional_inputs.is_some());
                responder.send(Ok(())).unwrap();
                args.controller.unwrap()
            }
            other => panic!("unexpected RealmRequest: {other:?}"),
        };

        // 2. Expect Controller.Start.
        let mut comp_ctrl_stream = comp_ctrl_server.into_stream();
        let exec_ctrl_server = match comp_ctrl_stream.next().await.unwrap().unwrap() {
            fcomponent::ControllerRequest::Start { execution_controller, responder, .. } => {
                responder.send(Ok(())).unwrap();
                execution_controller
            }
            other => panic!("unexpected ControllerRequest: {other:?}"),
        };

        // Yield until `run_governor_attempt` processes the `Start` response and sets
        // `running: true` before waiting on `exec_proxy.take_event_stream()`.
        loop {
            let hierarchy = inspect::reader::read(&inspector).await.unwrap();
            if hierarchy
                .get_child("msc")
                .and_then(|n| n.get_property("running"))
                .and_then(|p| p.boolean())
                == Some(true)
            {
                break;
            }
            fasync::Task::local(async {}).await;
        }
        assert_data_tree!(inspector, root: {
            msc: {
                url: "fuchsia-pkg://fuchsia.com/msc#meta/msc.cm",
                running: true,
                restarts: 0u64,
            }
        });

        let (_, exec_control_handle) = exec_ctrl_server.into_stream_and_control_handle();
        exec_control_handle
            .send_on_stop(&fcomponent::StoppedPayload {
                status: Some(zx::Status::INTERNAL.into_raw()),
                exit_code: None,
                ..Default::default()
            })
            .unwrap();

        // 3. Expect DestroyChild after OnStop.
        match realm_stream.next().await.unwrap().unwrap() {
            fcomponent::RealmRequest::DestroyChild { child, responder } => {
                assert_eq!(child.name, "msc");
                assert_eq!(child.collection.as_deref(), Some(GOVERNORS_COLLECTION));
                responder.send(Ok(())).unwrap();
            }
            other => panic!("unexpected RealmRequest: {other:?}"),
        }

        // 4. Expect the next CreateChild attempt, which proves `running: false` and `restarts: 1`
        // have been recorded.
        match realm_stream.next().await.unwrap().unwrap() {
            fcomponent::RealmRequest::CreateChild { decl, .. } => {
                assert_eq!(decl.name.as_deref(), Some("msc"));
            }
            other => panic!("unexpected RealmRequest: {other:?}"),
        }
        assert_data_tree!(inspector, root: {
            msc: {
                url: "fuchsia-pkg://fuchsia.com/msc#meta/msc.cm",
                running: false,
                restarts: 1u64,
            }
        });
    }
}
