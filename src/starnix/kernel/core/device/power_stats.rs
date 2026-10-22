// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-[`Kernel`] discovery and querying of `fuchsia.hardware.power.stats.Service` providers.

use crate::device::service_watcher::{ServiceBindingScope, ServiceWatcher};
use crate::task::Kernel;
use fidl::endpoints::{Proxy as _, SynchronousProxy as _};
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_async::TimeoutExt as _;
use starnix_logging::log_warn;
use starnix_sync::{LockDepMutex, PowerStatsManagerLock};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// How long to wait for a power stats provider to respond to a FIDL request before failing.
const PROVIDER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// Connected `fuchsia.hardware.power.stats.Service` provider instance and its reported entities.
#[derive(Debug)]
pub struct PowerStatsProvider {
    instance_name: String,
    entity_names: HashSet<String>,
    proxy: Arc<fpowerstats::DeviceSynchronousProxy>,
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
            proxy: Arc::new(proxy),
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
    /// Active provider serving each discovered entity name, or `None` when the provider that was
    /// serving that entity has disconnected and has not yet been replaced.
    entity_providers: HashMap<String, Option<Arc<PowerStatsProvider>>>,
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
    driver_registered: AtomicBool,
    state: LockDepMutex<PowerStatsManagerState, PowerStatsManagerLock>,
}

/// Handle to the per-[`Kernel`] singleton that discovers
/// `fuchsia.hardware.power.stats.Service` providers and queries per-entity residency statistics.
#[derive(Clone, Debug)]
pub struct PowerStatsManager {
    inner: Arc<PowerStatsManagerInner>,
}

impl PowerStatsManager {
    /// Returns the per-[`Kernel`] [`PowerStatsManager`], starting the asynchronous watcher for
    /// `/svc/fuchsia.hardware.power.stats.Service` on first access.
    pub fn get_or_init(kernel: &Kernel) -> Self {
        let inner = kernel.expando.get::<PowerStatsManagerInner>();
        let manager = Self { inner };
        if !manager.inner.driver_registered.swap(true, Ordering::AcqRel) {
            let manager_for_bind = manager.clone();
            ServiceWatcher::register_with_probe(
                kernel,
                fpowerstats::ServiceMarker,
                Self::probe_service_instance,
                move |current_task, provider, scope| {
                    manager_for_bind.bind_provider(current_task.kernel(), provider, scope);
                },
            );
        }
        manager
    }

    /// Returns the per-[`Kernel`] [`PowerStatsManager`] for unit testing, suppressing the
    /// background `/svc` watcher if it has not been started yet.
    pub fn get_or_init_for_testing(kernel: &Kernel) -> Self {
        let inner = kernel.expando.get::<PowerStatsManagerInner>();
        inner.driver_registered.store(true, Ordering::Release);
        Self { inner }
    }

    /// Registers `provider` with the per-[`Kernel`] [`PowerStatsManager`] for unit testing,
    /// suppressing the background `/svc` watcher if it has not been started yet.
    pub fn add_provider_for_testing(kernel: &Kernel, provider: PowerStatsProvider) {
        let _ = Self::get_or_init_for_testing(kernel).register_provider(kernel, provider);
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

    /// Queries the current residency [`fpowerstats::Stats`] for `entity_name`.
    ///
    /// Returns `Ok(None)` if no provider reporting `entity_name` has been discovered yet, or
    /// `Err(EIO)` if the provider serving `entity_name` has disconnected, timed out, or omitted
    /// `entity_name` from its report.
    pub fn entity_stats(&self, entity_name: &str) -> Result<Option<fpowerstats::Stats>, Errno> {
        let provider = match self.inner.state.lock().entity_providers.get(entity_name).cloned() {
            None => return Ok(None),
            Some(None) => return error!(EIO),
            Some(Some(provider)) => provider,
        };
        provider.entity_stats(entity_name).map(Some)
    }

    async fn probe_service_instance(
        instance: Arc<fpowerstats::ServiceProxy>,
    ) -> Option<PowerStatsProvider> {
        let instance_name = instance.instance_name().to_string();
        let device_proxy = match instance.connect_to_device() {
            Ok(proxy) => proxy,
            Err(e) => {
                log_warn!("Failed to connect to power stats provider {instance_name}: {e:#}");
                return None;
            }
        };
        let entities = match async { Some(device_proxy.get_entities().await) }
            .on_timeout(PROVIDER_TIMEOUT, || None)
            .await
        {
            Some(Ok(entities)) => entities,
            Some(Err(e)) => {
                log_warn!("Failed to get power entities from {instance_name}: {e:#}");
                return None;
            }
            None => {
                log_warn!("Timed out getting power entities from {instance_name}");
                return None;
            }
        };
        let client_end = match device_proxy.into_client_end() {
            Ok(client_end) => client_end,
            Err(_) => {
                log_warn!("Failed to unwrap device proxy channel for {instance_name}");
                return None;
            }
        };
        let entity_names = entities.into_iter().map(|entity| entity.name);
        let sync_proxy = fpowerstats::DeviceSynchronousProxy::new(client_end.into_channel());
        Some(PowerStatsProvider::new(instance_name, entity_names, sync_proxy))
    }

    fn bind_provider(
        &self,
        kernel: &Kernel,
        provider: PowerStatsProvider,
        scope: &mut ServiceBindingScope,
    ) {
        scope.monitor_channel(provider.proxy.clone());
        let provider = self.register_provider(kernel, provider);
        let manager = self.clone();
        scope.on_disconnect(move |_current_task| {
            manager.disconnect_provider(&provider);
        });
    }

    fn register_provider(
        &self,
        kernel: &Kernel,
        mut provider: PowerStatsProvider,
    ) -> Arc<PowerStatsProvider> {
        let mut state = self.inner.state.lock();
        let instance_name = provider.instance_name.clone();
        let served_entities: Vec<String> = provider
            .entity_names
            .iter()
            .filter(|&entity_name| {
                if let Some(existing) = state
                    .entity_providers
                    .get(entity_name)
                    .and_then(|slot| slot.as_ref())
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
            state.entity_providers.insert(entity_name, Some(provider.clone()));
        }
        if !provider.entity_names.is_empty() {
            state.providers.push(provider.clone());
            for listener in &mut state.listeners {
                listener(kernel, &provider);
            }
        }
        provider
    }

    fn disconnect_provider(&self, disconnected: &Arc<PowerStatsProvider>) {
        let mut state = self.inner.state.lock();
        for slot in state.entity_providers.values_mut() {
            if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, disconnected)) {
                *slot = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::service_watcher::watch_service_dir_for_testing;
    use crate::testing::{spawn_kernel_and_run, spawn_kernel_and_run_sync};
    use fidl::endpoints::{ServerEnd, create_sync_proxy};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use futures::channel::{mpsc, oneshot};
    use futures::future::select;
    use starnix_sync::Mutex;
    use std::pin::pin;
    use std::sync::atomic::AtomicUsize;
    use vfs::directory::helper::DirectlyMutable as _;
    use vfs::directory::immutable::Simple;

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

    fn start_test_service_dir(kernel: &Kernel) -> Arc<Simple> {
        let dir = vfs::directory::immutable::simple();
        let proxy = vfs::directory::serve_read_only(dir.clone(), vfs::ExecutionScope::new());
        watch_service_dir_for_testing::<fpowerstats::ServiceMarker>(kernel, proxy);
        dir
    }

    fn add_instance(
        dir: &Arc<Simple>,
        name: &str,
        on_open: impl Fn(zx::Channel) + Send + Sync + 'static,
    ) {
        let instance_dir = vfs::pseudo_directory! {
            "device" => vfs::service::endpoint(move |_scope, channel| {
                on_open(channel.into_zx_channel());
            }),
        };
        dir.add_entry_may_overwrite(name, instance_dir, /* overwrite = */ true)
            .expect("add service instance");
    }

    #[::fuchsia::test]
    async fn discovers_entities_and_queries_stats() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);
            let manager = PowerStatsManager::get_or_init(kernel);
            assert!(Arc::ptr_eq(&manager.inner, &PowerStatsManager::get_or_init(kernel).inner));
            assert_eq!(manager.entity_stats("cpu_c2"), Ok(None));

            let (notified_tx, mut notified_rx) = mpsc::unbounded();
            manager.watch_providers(kernel, move |_, provider| {
                let _ = notified_tx.unbounded_send(provider.instance_name().to_string());
            });

            add_instance(&dir, "default", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(
                        stream,
                        vec![stats("cpu_c2", vec![state(0, 5, 120)]), stats("lpcm_soc", vec![])],
                    )
                });
            });

            assert_eq!(notified_rx.next().await.as_deref(), Some("default"));
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
    async fn probe_service_instance_skips_failing_provider_and_discovers_subsequent_instances() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);
            let manager = PowerStatsManager::get_or_init(kernel);

            let (notified_tx, mut notified_rx) = mpsc::unbounded();
            manager.watch_providers(kernel, move |_, provider| {
                let _ = notified_tx.unbounded_send(provider.instance_name().to_string());
            });

            add_instance(&dir, "broken_closed", |_| {});
            add_instance(&dir, "healthy", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(stream, vec![stats("cpu_c2", vec![state(0, 4, 80)])])
                });
            });

            assert_eq!(notified_rx.next().await.as_deref(), Some("healthy"));
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

            let announcements = Arc::new(Mutex::new(Vec::new()));
            let announcements_for_listener = announcements.clone();
            manager.watch_providers(kernel, move |_, provider| {
                announcements_for_listener
                    .lock()
                    .push((provider.instance_name().to_string(), provider.entity_names().clone()));
            });
            assert_eq!(
                *announcements.lock(),
                vec![
                    (
                        "first".to_string(),
                        HashSet::from(["shared".to_string(), "only_first".to_string()])
                    ),
                    ("second".to_string(), HashSet::from(["only_second".to_string()])),
                ]
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

    #[::fuchsia::test]
    async fn watcher_disconnect_clears_provider_and_restarted_instance_recovers() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);
            let manager = PowerStatsManager::get_or_init(kernel);

            let (announced_tx, mut announced_rx) = mpsc::unbounded();
            manager.watch_providers(kernel, move |_, provider| {
                let _ = announced_tx.unbounded_send((
                    provider.instance_name().to_string(),
                    provider.entity_names().clone(),
                ));
            });

            let (stop_tx, stop_rx) = oneshot::channel::<()>();
            let stop_rx = Arc::new(Mutex::new(Some(stop_rx)));
            let (stopped_tx, stopped_rx) = oneshot::channel::<()>();
            let stopped_tx = Arc::new(Mutex::new(Some(stopped_tx)));
            add_instance(&dir, "first", move |server_end| {
                let stop_rx = stop_rx.lock().take().expect("stop_rx taken once");
                let stopped_tx = stopped_tx.lock().take().expect("stopped_tx taken once");
                spawn_server(move || async move {
                    {
                        let stream =
                            ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                        let serve_fut = pin!(serve_provider(
                            stream,
                            vec![
                                stats("cpu_c2", vec![state(0, 5, 100)]),
                                stats("unrecovered_entity", vec![state(0, 1, 10)]),
                            ],
                        ));
                        let _ = select(serve_fut, stop_rx).await;
                    }
                    let _ = stopped_tx.send(());
                });
            });

            assert_eq!(
                announced_rx.next().await,
                Some((
                    "first".to_string(),
                    HashSet::from(["cpu_c2".to_string(), "unrecovered_entity".to_string()]),
                ))
            );
            let cpu_c2 = manager.entity_stats("cpu_c2").unwrap().unwrap();
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 100);

            // Stop `"first"`; queries for its entities now return `EIO`.
            let _ = stop_tx.send(());
            let _ = stopped_rx.await;
            assert_eq!(manager.entity_stats("cpu_c2"), error!(EIO));
            assert_eq!(manager.entity_stats("unrecovered_entity"), error!(EIO));

            // Inject a restarted instance serving `cpu_c2` plus `new_entity`; `cpu_c2` recovers
            // without being re-announced to listeners, while `unrecovered_entity` still returns
            // `EIO`.
            add_instance(&dir, "first_restarted", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(
                        stream,
                        vec![
                            stats("cpu_c2", vec![state(0, 7, 250)]),
                            stats("new_entity", vec![state(0, 1, 20)]),
                        ],
                    )
                });
            });

            assert_eq!(
                announced_rx.next().await,
                Some(("first_restarted".to_string(), HashSet::from(["new_entity".to_string()])))
            );
            let recovered = manager.entity_stats("cpu_c2").unwrap().unwrap();
            assert_eq!(recovered.state_residency_data[0].total_time_ms, 250);
            assert_eq!(manager.entity_stats("unrecovered_entity"), error!(EIO));
        })
        .await;
    }
}
