// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-[`Kernel`] discovery and querying of `fuchsia.hardware.power.stats.Service` providers.

use crate::device::service_watcher::watch_fidl_service;
use crate::task::Kernel;
use fidl::endpoints::{Proxy as _, SynchronousProxy as _};
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_async::TimeoutExt as _;
use starnix_logging::log_warn;
use starnix_sync::{LockDepMutex, PowerStatsManagerLock};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// How long to wait for a power stats provider to respond to a FIDL request before failing.
const PROVIDER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// Connected `fuchsia.hardware.power.stats.Service` provider instance and its reported entities.
#[derive(Debug)]
pub struct PowerStatsProvider {
    instance_name: String,
    entity_names: HashSet<String>,
    proxy: fpowerstats::DeviceSynchronousProxy,
}

impl PowerStatsProvider {
    /// Creates a [`PowerStatsProvider`] for `instance_name` serving `entity_names` over `proxy`.
    pub fn new(
        instance_name: impl Into<String>,
        entity_names: impl IntoIterator<Item = impl Into<String>>,
        proxy: fpowerstats::DeviceSynchronousProxy,
    ) -> Self {
        Self {
            instance_name: instance_name.into(),
            entity_names: entity_names.into_iter().map(Into::into).collect(),
            proxy,
        }
    }

    /// Name of the service instance within `fuchsia.hardware.power.stats.Service`.
    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    /// Names of the newly discovered power entities announced by this provider.
    ///
    /// Entities that were already announced by an earlier provider (whether still active or
    /// disconnected) are excluded so listeners observe each discovered entity name only once.
    pub fn entity_names(&self) -> &HashSet<String> {
        &self.entity_names
    }

    /// Returns whether the underlying provider channel has observed `PEER_CLOSED`.
    fn is_closed(&self) -> bool {
        self.proxy.is_closed().unwrap_or(true)
    }

    /// Queries the provider for the current residency [`fpowerstats::Stats`] of `entity_name`.
    fn entity_stats(&self, entity_name: &str) -> Result<fpowerstats::Stats, Errno> {
        let deadline = zx::MonotonicInstant::after(PROVIDER_TIMEOUT);
        let reports = self.proxy.get_stats(deadline).map_err(|e| {
            log_warn!(
                "Failed to read power stats for {entity_name} from {}: {e}",
                self.instance_name
            );
            errno!(EIO)
        })?;
        let Some(stats) = reports.into_iter().find(|stats| stats.entity == entity_name) else {
            log_warn!(
                "Power stats provider {} no longer reports {entity_name}",
                self.instance_name
            );
            return error!(EIO);
        };
        Ok(stats)
    }
}

type ProviderListener = dyn FnMut(&Kernel, &Arc<PowerStatsProvider>) + Send;

#[derive(Default)]
struct PowerStatsManagerState {
    /// Discovered provider instances that announced new entities, in discovery order.
    providers: Vec<Arc<PowerStatsProvider>>,
    /// Active or last-known provider serving each discovered entity name.
    entity_providers: HashMap<String, Arc<PowerStatsProvider>>,
    /// Listeners notified whenever a provider instance announces new entities.
    listeners: Vec<Box<ProviderListener>>,
}

impl std::fmt::Debug for PowerStatsManagerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerStatsManagerState")
            .field("providers", &self.providers)
            .field("entity_providers", &self.entity_providers)
            .field("listeners_count", &self.listeners.len())
            .finish()
    }
}

/// Internal state stored in [`Kernel::expando`] so callers must access [`PowerStatsManager`] via
/// [`PowerStatsManager::get_or_init`].
#[derive(Debug, Default)]
struct PowerStatsManagerInner {
    watch_started: AtomicBool,
    state: LockDepMutex<PowerStatsManagerState, PowerStatsManagerLock>,
}

/// Handle to the per-[`Kernel`] singleton that watches `fuchsia.hardware.power.stats.Service` on
/// the kernel's main async executor and tracks discovered power entities for kernel subsystems and
/// modules.
///
/// Discovery is performed asynchronously as providers appear in `/svc`, while
/// [`Self::entity_stats`] looks up the [`PowerStatsProvider`] serving a given entity and issues the
/// synchronous `GetStats` call outside the internal lock.
#[derive(Clone, Debug)]
pub struct PowerStatsManager {
    inner: Arc<PowerStatsManagerInner>,
}

impl PowerStatsManager {
    /// Returns the per-[`Kernel`] [`PowerStatsManager`], starting the asynchronous watcher for
    /// `/svc/fuchsia.hardware.power.stats.Service` on first access.
    pub fn get_or_init(kernel: &Kernel) -> Self {
        let inner = kernel.expando.get::<PowerStatsManagerInner>();
        if !inner.watch_started.swap(true, Ordering::AcqRel) {
            watch_fidl_service(
                kernel,
                fpowerstats::ServiceMarker,
                "power_stats",
                Self::handle_service_instance,
            );
        }
        Self { inner }
    }

    /// Returns the per-[`Kernel`] [`PowerStatsManager`] for unit testing, suppressing the
    /// background `/svc` watcher if it has not been started yet.
    pub fn get_or_init_for_testing(kernel: &Kernel) -> Self {
        let inner = kernel.expando.get::<PowerStatsManagerInner>();
        inner.watch_started.store(true, Ordering::Release);
        Self { inner }
    }

    /// Registers `provider` with the per-[`Kernel`] [`PowerStatsManager`] for unit testing,
    /// suppressing the background `/svc` watcher if it has not been started yet.
    pub fn add_provider_for_testing(kernel: &Kernel, provider: PowerStatsProvider) {
        Self::get_or_init_for_testing(kernel).register_provider(kernel, provider);
    }

    /// Registers `listener` to be invoked for every [`PowerStatsProvider`] that announces newly
    /// discovered power entities, including any providers discovered prior to this call.
    ///
    /// Each discovered entity name is announced at most once across all provider notifications:
    /// if a provider disconnects and a restarted provider re-registers the same entities,
    /// [`Self::entity_stats`] switches to the new provider while listeners are only invoked for
    /// any previously unannounced entities.
    ///
    /// Listeners are invoked while holding [`PowerStatsManagerLock`], which is ordered before
    /// [`DeviceRegistryStateLevel`](starnix_sync::DeviceRegistryStateLevel) and
    /// [`SimpleDirectoryEntriesLock`](starnix_sync::SimpleDirectoryEntriesLock) so listeners may
    /// register devices and sysfs nodes without race windows between existing and newly discovered
    /// providers. Because [`PowerStatsManagerLock`] is held during the callback, `listener` must
    /// not synchronously re-enter [`PowerStatsManager`] methods.
    pub fn watch_providers<F>(&self, kernel: &Kernel, mut listener: F)
    where
        F: FnMut(&Kernel, &Arc<PowerStatsProvider>) + Send + 'static,
    {
        let mut state = self.inner.state.lock();
        for provider in &state.providers {
            listener(kernel, provider);
        }
        state.listeners.push(Box::new(listener));
    }

    /// Queries the current residency [`fpowerstats::Stats`] for `entity_name`, returning
    /// `Ok(None)` if no provider reporting `entity_name` has been discovered yet.
    pub fn entity_stats(&self, entity_name: &str) -> Result<Option<fpowerstats::Stats>, Errno> {
        let provider = self.inner.state.lock().entity_providers.get(entity_name).cloned();
        provider.map(|provider| provider.entity_stats(entity_name)).transpose()
    }

    async fn handle_service_instance(
        weak_kernel: &Weak<Kernel>,
        instance: fpowerstats::ServiceProxy,
    ) {
        let instance_name = instance.instance_name().to_string();
        let device_proxy = match instance.connect_to_device() {
            Ok(proxy) => proxy,
            Err(e) => {
                log_warn!("Failed to connect to power stats provider {instance_name}: {e:#}");
                return;
            }
        };
        let entities = match async { Some(device_proxy.get_entities().await) }
            .on_timeout(PROVIDER_TIMEOUT, || None)
            .await
        {
            Some(Ok(entities)) => entities,
            Some(Err(e)) => {
                log_warn!("Failed to get power entities from {instance_name}: {e:#}");
                return;
            }
            None => {
                log_warn!("Timed out getting power entities from {instance_name}");
                return;
            }
        };
        let Some(kernel) = weak_kernel.upgrade() else {
            return;
        };
        let client_end = match device_proxy.into_client_end() {
            Ok(client_end) => client_end,
            Err(_) => {
                log_warn!("Failed to unwrap device proxy channel for {instance_name}");
                return;
            }
        };
        let entity_names = entities.into_iter().map(|entity| entity.name);
        let sync_proxy = fpowerstats::DeviceSynchronousProxy::new(client_end.into_channel());
        let provider = PowerStatsProvider::new(instance_name, entity_names, sync_proxy);
        let inner = kernel.expando.get::<PowerStatsManagerInner>();
        Self { inner }.register_provider(&kernel, provider);
    }

    fn register_provider(&self, kernel: &Kernel, mut provider: PowerStatsProvider) {
        let mut state = self.inner.state.lock();
        let instance_name = provider.instance_name.clone();
        let served_entities: Vec<String> = provider
            .entity_names
            .iter()
            .filter(|&entity_name| {
                if let Some(existing) = state
                    .entity_providers
                    .get(entity_name)
                    .filter(|existing| !existing.is_closed())
                {
                    log_warn!(
                        "Ignoring duplicate power entity {entity_name} from provider {instance_name} (already served by {})",
                        existing.instance_name()
                    );
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        provider
            .entity_names
            .retain(|entity_name| !state.entity_providers.contains_key(entity_name));
        let provider = Arc::new(provider);
        for entity_name in served_entities {
            state.entity_providers.insert(entity_name, provider.clone());
        }
        if !provider.entity_names.is_empty() {
            state.providers.push(provider.clone());
            for listener in &mut state.listeners {
                listener(kernel, &provider);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::service_watcher::run_fidl_service_watcher;
    use crate::testing::{spawn_kernel_and_run, spawn_kernel_and_run_sync};
    use fidl::endpoints::{ServerEnd, create_endpoints, create_sync_proxy};
    use fidl_fuchsia_io as fio;
    use fuchsia_async as fasync;
    use fuchsia_component::client as fclient;
    use fuchsia_component::server::ServiceFs;
    use futures::StreamExt;
    use futures::channel::oneshot;
    use futures::future::{Either, select};
    use std::pin::pin;
    use std::sync::atomic::AtomicUsize;

    fn state(id: i32, entry_count: u32, total_time_ms: u64) -> fpowerstats::StateResidency {
        fpowerstats::StateResidency {
            id,
            name: id.to_string(),
            total_time_ms,
            entry_count,
            last_entry_timestamp_ms: 0,
        }
    }

    fn stats(
        entity: &str,
        state_residency_data: Vec<fpowerstats::StateResidency>,
    ) -> fpowerstats::Stats {
        fpowerstats::Stats { entity: entity.to_string(), state_residency_data }
    }

    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                async move {
                    match request {
                        Ok(fpowerstats::DeviceRequest::GetEntities { responder }) => {
                            let entities: Vec<_> = reports
                                .iter()
                                .map(|stats| fpowerstats::Entity {
                                    name: stats.entity.clone(),
                                    states: vec![],
                                })
                                .collect();
                            let _ = responder.send(&entities);
                        }
                        Ok(fpowerstats::DeviceRequest::GetStats { responder }) => {
                            let _ = responder.send(reports);
                        }
                        _ => {}
                    }
                }
            })
            .await;
    }

    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    #[derive(Clone)]
    enum ProviderBehavior {
        Healthy(Vec<fpowerstats::Stats>),
        Closed,
        Hung,
    }

    fn serve_service(
        providers: Vec<(&'static str, ProviderBehavior)>,
    ) -> fclient::Service<fpowerstats::ServiceMarker> {
        let (client, server) = create_endpoints::<fio::DirectoryMarker>();
        spawn_server(move || async move {
            let mut fs = ServiceFs::new_local();
            for (instance, behavior) in providers {
                fs.add_fidl_service_instance(instance, move |request| {
                    let fpowerstats::ServiceRequest::Device(stream) = request;
                    (stream, behavior.clone())
                });
            }
            fs.serve_connection(server).expect("serve service directory");
            fs.for_each_concurrent(None, |(stream, behavior)| async move {
                match behavior {
                    ProviderBehavior::Healthy(reports) => {
                        serve_provider(stream, reports).await;
                    }
                    ProviderBehavior::Closed => {}
                    ProviderBehavior::Hung => {
                        let _stream = stream;
                        std::future::pending::<()>().await;
                    }
                }
            })
            .await;
        });
        fclient::Service::open_from_dir(client, fpowerstats::ServiceMarker).expect("open service")
    }

    #[::fuchsia::test]
    async fn discovers_entities_and_queries_stats() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);
            assert!(Arc::ptr_eq(&manager.inner, &PowerStatsManager::get_or_init(kernel).inner));
            assert_eq!(manager.entity_stats("cpu_c2"), Ok(None));

            let (tx, rx) = oneshot::channel();
            let mut tx = Some(tx);
            manager.watch_providers(kernel, move |_, _| {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            });

            let service = serve_service(vec![(
                "default",
                ProviderBehavior::Healthy(vec![
                    stats("cpu_c2", vec![state(0, 5, 120)]),
                    stats("lpcm_soc", vec![]),
                ]),
            )]);
            let weak_kernel = kernel.weak_self.clone();
            let watcher = pin!(run_fidl_service_watcher(
                &weak_kernel,
                service,
                PowerStatsManager::handle_service_instance,
            ));

            match select(watcher, rx).await {
                Either::Left((result, _)) => {
                    panic!("watcher exited before provider was discovered: {result:?}");
                }
                Either::Right((Ok(()), _)) => {}
                Either::Right((Err(e), _)) => {
                    panic!("oneshot sender dropped before signalling: {e:?}");
                }
            }

            let cpu_c2 = manager
                .entity_stats("cpu_c2")
                .expect("GetStats succeeded")
                .expect("cpu_c2 entity discovered");
            assert_eq!(cpu_c2.state_residency_data.len(), 1);
            assert_eq!(cpu_c2.state_residency_data[0].entry_count, 5);
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 120);
            assert_eq!(manager.entity_stats("unknown"), Ok(None));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn closed_provider_returns_eio_and_restarted_provider_recovers() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            let (live_proxy, live_server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            spawn_server(move || {
                serve_provider(
                    live_server.into_stream(),
                    vec![stats("cpu_c2", vec![state(0, 9, 42)])],
                )
            });
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("first", ["cpu_c2"], live_proxy),
            );

            let (closed_proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            drop::<ServerEnd<_>>(server);
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new(
                    "second",
                    ["cpu_c2", "dead_entity", "unrecovered_entity"],
                    closed_proxy,
                ),
            );

            // Listeners only observe newly announced entities on subsequent providers.
            let early_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let early_seen_for_listener = early_seen.clone();
            manager.watch_providers(kernel, move |_, provider| {
                early_seen_for_listener
                    .lock()
                    .unwrap()
                    .push((provider.instance_name().to_string(), provider.entity_names().clone()));
            });
            assert_eq!(
                *early_seen.lock().unwrap(),
                vec![
                    ("first".to_string(), HashSet::from(["cpu_c2".to_string()])),
                    (
                        "second".to_string(),
                        HashSet::from([
                            "dead_entity".to_string(),
                            "unrecovered_entity".to_string()
                        ])
                    ),
                ]
            );

            // The first provider to report `cpu_c2` continues to serve it even when a second
            // provider reports a duplicate `cpu_c2`.
            let cpu_c2 = manager
                .entity_stats("cpu_c2")
                .expect("GetStats succeeded")
                .expect("cpu_c2 entity served by first provider");
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 42);

            // Querying an entity served by a closed provider returns `EIO` until a replacement
            // provider registers for that entity, even if an unrelated provider registers in the
            // meantime.
            assert_eq!(manager.entity_stats("dead_entity"), error!(EIO));
            let (unrelated_proxy, _unrelated_server) =
                create_sync_proxy::<fpowerstats::DeviceMarker>();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("unrelated", ["other_entity"], unrelated_proxy),
            );
            assert_eq!(manager.entity_stats("dead_entity"), error!(EIO));

            // When a restarted provider registers, it replaces the closed provider for
            // `dead_entity` without re-announcing `dead_entity` to existing or late listeners,
            // while `unrecovered_entity` continues returning `EIO`.
            let (restarted_proxy, restarted_server) =
                create_sync_proxy::<fpowerstats::DeviceMarker>();
            spawn_server(move || {
                serve_provider(
                    restarted_server.into_stream(),
                    vec![stats("dead_entity", vec![state(0, 3, 99)])],
                )
            });
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("second_restarted", ["dead_entity"], restarted_proxy),
            );
            let recovered = manager
                .entity_stats("dead_entity")
                .expect("GetStats succeeded")
                .expect("dead_entity served by restarted provider");
            assert_eq!(recovered.state_residency_data[0].total_time_ms, 99);
            assert_eq!(manager.entity_stats("unrecovered_entity"), error!(EIO));

            let late_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let late_seen_for_listener = late_seen.clone();
            manager.watch_providers(kernel, move |_, provider| {
                late_seen_for_listener
                    .lock()
                    .unwrap()
                    .push((provider.instance_name().to_string(), provider.entity_names().clone()));
            });
            let expected_announcements = vec![
                ("first".to_string(), HashSet::from(["cpu_c2".to_string()])),
                (
                    "second".to_string(),
                    HashSet::from(["dead_entity".to_string(), "unrecovered_entity".to_string()]),
                ),
                ("unrelated".to_string(), HashSet::from(["other_entity".to_string()])),
            ];
            assert_eq!(*early_seen.lock().unwrap(), expected_announcements);
            assert_eq!(*late_seen.lock().unwrap(), expected_announcements);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn watch_providers_notifies_for_existing_and_new_providers() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            let (proxy1, _server1) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("first", ["first_entity"], proxy1),
            );

            let seen = Arc::new(AtomicUsize::new(0));
            let seen_for_listener = seen.clone();
            manager.watch_providers(kernel, move |_, _| {
                seen_for_listener.fetch_add(1, Ordering::SeqCst);
            });
            assert_eq!(seen.load(Ordering::SeqCst), 1);

            let (proxy2, _server2) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("second", ["second_entity"], proxy2),
            );
            assert_eq!(seen.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_returns_eio_when_entity_omitted_from_stats_report() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            spawn_server(move || {
                serve_provider(server.into_stream(), vec![stats("b", vec![state(0, 1, 10)])])
            });
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("test", ["a", "b"], proxy),
            );

            assert!(manager.entity_stats("b").expect("GetStats succeeded").is_some());
            assert_eq!(manager.entity_stats("a"), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_returns_eio_when_provider_times_out() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            // Keep `_server` open without servicing requests so `GetStats` times out.
            let (proxy, _server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("hung", ["a"], proxy),
            );

            assert_eq!(manager.entity_stats("a"), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn handle_service_instance_skips_failing_provider_and_discovers_subsequent_instances() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            let (tx, rx) = oneshot::channel();
            let mut tx = Some(tx);
            let mut seen_names = Vec::new();
            manager.watch_providers(kernel, move |_, provider| {
                seen_names.push(provider.instance_name().to_string());
                if provider.instance_name() == "healthy" {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(std::mem::take(&mut seen_names));
                    }
                }
            });

            let service = serve_service(vec![
                ("broken_closed", ProviderBehavior::Closed),
                ("broken_hung", ProviderBehavior::Hung),
                (
                    "healthy",
                    ProviderBehavior::Healthy(vec![stats("cpu_c2", vec![state(0, 4, 80)])]),
                ),
            ]);

            let weak_kernel = kernel.weak_self.clone();
            let watcher = pin!(run_fidl_service_watcher(
                &weak_kernel,
                service,
                PowerStatsManager::handle_service_instance,
            ));

            let seen_names = match select(watcher, rx).await {
                Either::Left((result, _)) => {
                    panic!("watcher exited before healthy provider was discovered: {result:?}");
                }
                Either::Right((Ok(seen_names), _)) => seen_names,
                Either::Right((Err(e), _)) => {
                    panic!("oneshot sender dropped before signalling: {e:?}");
                }
            };

            assert_eq!(seen_names, vec!["healthy".to_string()]);
            let cpu_c2 = manager
                .entity_stats("cpu_c2")
                .expect("GetStats succeeded")
                .expect("cpu_c2 entity discovered from healthy provider");
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 80);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn duplicate_entities_across_live_providers_keep_first_provider_and_serve_disjoint_entities()
     {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let manager = PowerStatsManager::get_or_init_for_testing(kernel);

            let (proxy1, server1) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            spawn_server(move || {
                serve_provider(
                    server1.into_stream(),
                    vec![
                        stats("shared", vec![state(0, 1, 10)]),
                        stats("only_first", vec![state(0, 2, 20)]),
                    ],
                )
            });
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("first", ["shared", "only_first"], proxy1),
            );

            let (proxy2, server2) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            spawn_server(move || {
                serve_provider(
                    server2.into_stream(),
                    vec![
                        stats("shared", vec![state(0, 9, 999)]),
                        stats("only_second", vec![state(0, 3, 30)]),
                    ],
                )
            });
            PowerStatsManager::add_provider_for_testing(
                kernel,
                PowerStatsProvider::new("second", ["shared", "only_second"], proxy2),
            );

            let shared = manager.entity_stats("shared").unwrap().unwrap();
            assert_eq!(shared.state_residency_data[0].total_time_ms, 10);

            let only_first = manager.entity_stats("only_first").unwrap().unwrap();
            assert_eq!(only_first.state_residency_data[0].total_time_ms, 20);

            let only_second = manager.entity_stats("only_second").unwrap().unwrap();
            assert_eq!(only_second.state_residency_data[0].total_time_ms, 30);
        })
        .await;
    }
}
