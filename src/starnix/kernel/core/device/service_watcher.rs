// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-[`Kernel`] discovery, driver fan-out, and connection lifecycle management for Fuchsia FIDL
//! service directories (`/svc/<ServiceName>`).

use crate::device::kobject::Device;
use crate::task::{CurrentTask, Kernel};
use anyhow::Context as _;
use fidl::endpoints::{
    MemberOpener, Proxy as _, ServiceMarker, ServiceProxy as _, SynchronousProxy,
};
use fidl_fuchsia_io as fio;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_fs::directory::{WatchEvent, WatchMessage, Watcher};
use futures::{StreamExt as _, TryStreamExt as _};
use starnix_logging::{log_info, log_warn};
use starnix_sync::Mutex;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use zx::Peered as _;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait MonitoredChannel: Send + Sync + 'static {
    fn as_channel(&self) -> &zx::Channel;
}

impl<P: SynchronousProxy + Send + Sync + 'static> MonitoredChannel for P {
    fn as_channel(&self) -> &zx::Channel {
        SynchronousProxy::as_channel(self)
    }
}

/// Registration scope passed to a driver's `bind` callback for a discovered FIDL service instance.
///
/// Devices registered through [`Self::add_scoped_device`] and cleanup callbacks registered via
/// [`Self::on_disconnect`] are tied to the lifetime of the service instance and any channel
/// passed to [`Self::monitor_channel`] (later calls overwrite any previously monitored channel).
/// When a monitored channel observes `ZX_CHANNEL_PEER_CLOSED`, or when the service instance
/// directory entry is removed or replaced in `/svc/<S::SERVICE_NAME>`, the watcher runs all
/// registered teardown hooks and device removals in reverse registration order on the kernel's
/// main async executor.
#[derive(Default)]
pub struct ServiceBindingScope {
    monitored_channel: Option<Arc<dyn MonitoredChannel>>,
    teardown_hooks: Vec<Box<dyn FnOnce(&CurrentTask) + Send>>,
}

impl ServiceBindingScope {
    /// Retains `proxy` for the duration of the binding and monitors its underlying Zircon
    /// channel for `ZX_CHANNEL_PEER_CLOSED`, running all registered teardown hooks when the
    /// remote peer closes its endpoint.
    pub fn monitor_channel<P: SynchronousProxy + Send + Sync + 'static>(&mut self, proxy: Arc<P>) {
        self.monitored_channel = Some(proxy);
    }

    /// Registers `hook` to be invoked on the kernel's main async executor with
    /// [`KernelThreads::system_task`](crate::task::KernelThreads::system_task) when the binding
    /// disconnects, executing in reverse registration order relative to other hooks and scoped
    /// devices.
    pub fn on_disconnect(&mut self, hook: impl FnOnce(&CurrentTask) + Send + 'static) {
        self.teardown_hooks.push(Box::new(hook));
    }

    /// Registers a disconnect hook that removes `device` via
    /// [`DeviceRegistry::remove_device`](crate::device::DeviceRegistry::remove_device) when the
    /// binding disconnects, returning `device`.
    pub fn add_scoped_device(&mut self, device: Device) -> Device {
        let device_to_remove = device.clone();
        self.on_disconnect(move |current_task| {
            current_task.kernel().device_registry.remove_device(current_task, device_to_remove);
        });
        device
    }
}

impl std::fmt::Debug for ServiceBindingScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceBindingScope")
            .field("has_monitored_channel", &self.monitored_channel.is_some())
            .field("teardown_hooks_len", &self.teardown_hooks.len())
            .finish()
    }
}

struct InstanceEntry<P> {
    name: String,
    proxy: Arc<P>,
    active: Arc<AtomicBool>,
}

impl<P> Clone for InstanceEntry<P> {
    fn clone(&self) -> Self {
        Self { name: self.name.clone(), proxy: self.proxy.clone(), active: self.active.clone() }
    }
}

struct ActiveBinding {
    instance_active: Arc<AtomicBool>,
    monitored_channel: Option<Arc<dyn MonitoredChannel>>,
    _cancel_monitor: Option<futures::channel::oneshot::Sender<()>>,
    teardown_hooks: Vec<Box<dyn FnOnce(&CurrentTask) + Send>>,
}

impl ActiveBinding {
    fn is_stale(&self) -> bool {
        !self.instance_active.load(Ordering::Acquire)
            || self
                .monitored_channel
                .as_ref()
                .is_some_and(|ch| ch.as_channel().is_closed().unwrap_or(true))
    }

    fn teardown(self, current_task: &CurrentTask) {
        for hook in self.teardown_hooks.into_iter().rev() {
            hook(current_task);
        }
    }
}

struct DriverState<Bind> {
    bind: Bind,
    active_bindings: Vec<ActiveBinding>,
}

trait ServiceDriver<S: ServiceMarker>: Send + Sync {
    fn handle_instance<'a>(
        &'a self,
        state: &'a Arc<ServiceWatcherState<S>>,
        weak_kernel: &'a Weak<Kernel>,
        instance: InstanceEntry<S::Proxy>,
    ) -> BoxFuture<'a, ()>;

    fn cleanup_stale_bindings<'a>(
        &'a self,
        state: &'a Arc<ServiceWatcherState<S>>,
        weak_kernel: &'a Weak<Kernel>,
    ) -> BoxFuture<'a, ()>;
}

struct DriverEntry<S: ServiceMarker, P, Probe, ProbeFut, Bind> {
    singleton: bool,
    probe: Probe,
    /// Protects `bind` and `active_bindings`, serializing all `teardown` and `bind` calls for
    /// this driver so an old binding's teardown always completes before a replacement or
    /// restarted instance's `bind` closure runs.
    ///
    /// To prevent lock inversion with [`ServiceWatcherState::inner`], code holding `state` must
    /// never lock `ServiceWatcherState::inner`; instance liveness and channel closure are checked
    /// via lock-free atomics (`InstanceEntry::active`) and kernel handle signals.
    state: Mutex<DriverState<Bind>>,
    _marker: PhantomData<fn(S, P, ProbeFut)>,
}

impl<S, P, Probe, ProbeFut, Bind> DriverEntry<S, P, Probe, ProbeFut, Bind>
where
    S: ServiceMarker<Proxy: Send + Sync + 'static>,
    P: Send + 'static,
    Probe: Fn(Arc<S::Proxy>) -> ProbeFut + Send + Sync + 'static,
    ProbeFut: Future<Output = Option<P>> + Send + 'static,
    Bind: FnMut(&CurrentTask, P, &mut ServiceBindingScope) + Send + 'static,
{
    fn new(singleton: bool, probe: Probe, bind: Bind) -> Arc<Self> {
        Arc::new(Self {
            singleton,
            probe,
            state: Mutex::new(DriverState { bind, active_bindings: Vec::new() }),
            _marker: PhantomData,
        })
    }

    fn cleanup_stale_bindings_locked(
        state: &mut DriverState<Bind>,
        current_task: &CurrentTask,
    ) -> bool {
        let mut removed_any = false;
        let mut i = 0;
        while i < state.active_bindings.len() {
            if state.active_bindings[i].is_stale() {
                let binding = state.active_bindings.swap_remove(i);
                binding.instance_active.store(false, Ordering::Release);
                binding.teardown(current_task);
                removed_any = true;
            } else {
                i += 1;
            }
        }
        removed_any
    }

    async fn cleanup_stale_bindings(
        &self,
        watcher_state: &Arc<ServiceWatcherState<S>>,
        weak_kernel: &Weak<Kernel>,
    ) {
        let should_promote = {
            let Some(kernel) = weak_kernel.upgrade() else {
                return;
            };
            let system_task = kernel.kthreads.system_task();
            let mut state = self.state.lock();
            let removed = Self::cleanup_stale_bindings_locked(&mut state, system_task);
            self.singleton && removed && state.active_bindings.is_empty()
        };
        watcher_state.prune_inactive_instances();
        if should_promote {
            self.promote_singleton_instance(watcher_state, weak_kernel).await;
        }
    }

    async fn promote_singleton_instance(
        &self,
        watcher_state: &Arc<ServiceWatcherState<S>>,
        weak_kernel: &Weak<Kernel>,
    ) {
        for candidate in watcher_state.active_instances() {
            if !self.state.lock().active_bindings.is_empty() {
                break;
            }
            self.probe_and_bind_instance(watcher_state, weak_kernel, candidate).await;
        }
    }

    async fn probe_and_bind_instance(
        &self,
        watcher_state: &Arc<ServiceWatcherState<S>>,
        weak_kernel: &Weak<Kernel>,
        instance: InstanceEntry<S::Proxy>,
    ) {
        let Some(probed) = (self.probe)(instance.proxy.clone()).await else {
            return;
        };
        if let Some(kernel) = weak_kernel.upgrade()
            && instance.active.load(Ordering::Acquire)
        {
            self.bind_probed(kernel.kthreads.system_task(), watcher_state, &instance, probed);
        }
    }

    fn bind_probed(
        &self,
        system_task: &CurrentTask,
        watcher_state: &Arc<ServiceWatcherState<S>>,
        instance: &InstanceEntry<S::Proxy>,
        probed: P,
    ) {
        let monitored_channel = {
            let mut state = self.state.lock();

            // Synchronously tear down any bindings whose channels have already closed or whose
            // service directory entries have been removed or replaced so a restarted provider
            // instance never races against a pending `OnSignals` or `REMOVE_FILE` wakeup.
            Self::cleanup_stale_bindings_locked(&mut state, system_task);

            if !instance.active.load(Ordering::Acquire) {
                None
            } else if state
                .active_bindings
                .iter()
                .any(|binding| Arc::ptr_eq(&binding.instance_active, &instance.active))
            {
                None
            } else if self.singleton && !state.active_bindings.is_empty() {
                log_warn!(
                    "{}: ignoring additional service instance while singleton driver is bound",
                    S::SERVICE_NAME
                );
                None
            } else {
                let mut scope = ServiceBindingScope::default();
                (state.bind)(system_task, probed, &mut scope);

                let (cancel_tx, monitored) = match scope.monitored_channel.clone() {
                    Some(channel) => {
                        let (tx, rx) = futures::channel::oneshot::channel();
                        (Some(tx), Some((channel, rx)))
                    }
                    None => (None, None),
                };
                state.active_bindings.push(ActiveBinding {
                    instance_active: instance.active.clone(),
                    monitored_channel: scope.monitored_channel,
                    _cancel_monitor: cancel_tx,
                    teardown_hooks: scope.teardown_hooks,
                });
                monitored
            }
        };

        watcher_state.prune_inactive_instances();

        if let Some((monitored_channel, cancel_rx)) = monitored_channel {
            let kernel = system_task.kernel();
            let weak_kernel = kernel.weak_self.clone();
            let watcher_state = watcher_state.clone();
            let instance_active = instance.active.clone();
            kernel.kthreads.spawn_future(
                move || async move {
                    let peer_closed = {
                        let on_signals = std::pin::pin!(fasync::OnSignals::new(
                            monitored_channel.as_channel(),
                            zx::Signals::CHANNEL_PEER_CLOSED,
                        ));
                        matches!(
                            futures::future::select(on_signals, cancel_rx).await,
                            futures::future::Either::Left(_)
                        )
                    };
                    drop(monitored_channel);
                    if peer_closed {
                        let drivers = watcher_state.deactivate_instance(&instance_active);
                        futures::future::join_all(drivers.into_iter().map(|driver| {
                            let watcher_state = &watcher_state;
                            let weak_kernel = &weak_kernel;
                            async move {
                                driver.cleanup_stale_bindings(watcher_state, weak_kernel).await;
                            }
                        }))
                        .await;
                    }
                },
                "service_watcher_disconnect_monitor",
            );
        }
    }
}

impl<S, P, Probe, ProbeFut, Bind> ServiceDriver<S> for DriverEntry<S, P, Probe, ProbeFut, Bind>
where
    S: ServiceMarker<Proxy: Send + Sync + 'static>,
    P: Send + 'static,
    Probe: Fn(Arc<S::Proxy>) -> ProbeFut + Send + Sync + 'static,
    ProbeFut: Future<Output = Option<P>> + Send + 'static,
    Bind: FnMut(&CurrentTask, P, &mut ServiceBindingScope) + Send + 'static,
{
    fn handle_instance<'a>(
        &'a self,
        state: &'a Arc<ServiceWatcherState<S>>,
        weak_kernel: &'a Weak<Kernel>,
        instance: InstanceEntry<S::Proxy>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.probe_and_bind_instance(state, weak_kernel, instance).await;
            DriverEntry::cleanup_stale_bindings(self, state, weak_kernel).await;
        })
    }

    fn cleanup_stale_bindings<'a>(
        &'a self,
        state: &'a Arc<ServiceWatcherState<S>>,
        weak_kernel: &'a Weak<Kernel>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(DriverEntry::cleanup_stale_bindings(self, state, weak_kernel))
    }
}

struct ServiceWatcherInner<S: ServiceMarker> {
    instances: Vec<InstanceEntry<S::Proxy>>,
    drivers: Vec<Arc<dyn ServiceDriver<S>>>,
}

impl<S: ServiceMarker> Default for ServiceWatcherInner<S> {
    fn default() -> Self {
        Self { instances: Vec::new(), drivers: Vec::new() }
    }
}

struct ServiceWatcherState<S: ServiceMarker> {
    watch_started: AtomicBool,
    inner: Mutex<ServiceWatcherInner<S>>,
}

impl<S: ServiceMarker> Default for ServiceWatcherState<S> {
    fn default() -> Self {
        Self {
            watch_started: AtomicBool::new(false),
            inner: Mutex::new(ServiceWatcherInner::default()),
        }
    }
}

enum WatchDispatch<S: ServiceMarker> {
    Bind(InstanceEntry<S::Proxy>, Vec<Arc<dyn ServiceDriver<S>>>),
    Remove(Vec<Arc<dyn ServiceDriver<S>>>),
}

impl<S: ServiceMarker<Proxy: Send + Sync + 'static>> ServiceWatcherState<S> {
    fn deactivate_instance(
        &self,
        instance_active: &Arc<AtomicBool>,
    ) -> Vec<Arc<dyn ServiceDriver<S>>> {
        instance_active.store(false, Ordering::Release);
        let mut inner = self.inner.lock();
        inner.instances.retain(|entry| entry.active.load(Ordering::Acquire));
        inner.drivers.clone()
    }

    fn prune_inactive_instances(&self) {
        self.inner.lock().instances.retain(|entry| entry.active.load(Ordering::Acquire));
    }

    fn active_instances(&self) -> Vec<InstanceEntry<S::Proxy>> {
        let mut inner = self.inner.lock();
        inner.instances.retain(|entry| entry.active.load(Ordering::Acquire));
        inner.instances.clone()
    }

    fn record_instance(
        &self,
        instance_name: String,
        proxy: Arc<S::Proxy>,
    ) -> (InstanceEntry<S::Proxy>, Vec<Arc<dyn ServiceDriver<S>>>) {
        let mut inner = self.inner.lock();
        inner.instances.retain(|entry| {
            if entry.name == instance_name {
                entry.active.store(false, Ordering::Release);
                false
            } else {
                entry.active.load(Ordering::Acquire)
            }
        });
        let entry =
            InstanceEntry { name: instance_name, proxy, active: Arc::new(AtomicBool::new(true)) };
        inner.instances.push(entry.clone());
        let drivers = inner.drivers.clone();
        (entry, drivers)
    }

    fn remove_instance(&self, instance_name: &str) -> Vec<Arc<dyn ServiceDriver<S>>> {
        let mut inner = self.inner.lock();
        inner.instances.retain(|entry| {
            if entry.name == instance_name {
                entry.active.store(false, Ordering::Release);
                false
            } else {
                entry.active.load(Ordering::Acquire)
            }
        });
        inner.drivers.clone()
    }

    async fn watch_service_dir(
        self: &Arc<Self>,
        weak_kernel: &Weak<Kernel>,
        service_dir: fio::DirectoryProxy,
    ) -> Result<(), anyhow::Error> {
        let watcher = Watcher::new(&service_dir)
            .await
            .with_context(|| format!("failed to watch {} service", S::SERVICE_NAME))?;
        let sync_dir = Arc::new(fio::DirectorySynchronousProxy::new(
            service_dir
                .into_channel()
                .map_err(|_| anyhow::anyhow!("service directory proxy still in use"))?
                .into_zx_channel(),
        ));
        watcher
            .take_while(|msg| {
                let keep_going = weak_kernel.strong_count() > 0
                    && !matches!(msg, Ok(WatchMessage { event: WatchEvent::DELETED, .. }));
                std::future::ready(keep_going)
            })
            .try_for_each_concurrent(/* limit = */ None, |msg| {
                let dispatch = msg.filename.to_str().filter(|&name| name != ".").and_then(
                    |filename| match msg.event {
                        WatchEvent::ADD_FILE | WatchEvent::EXISTING => {
                            let instance_name = filename.to_owned();
                            let proxy = Arc::new(S::Proxy::from_member_opener(Box::new(
                                SyncServiceInstanceOpener {
                                    service_dir: sync_dir.clone(),
                                    instance_name: instance_name.clone(),
                                },
                            )));
                            let (entry, drivers) = self.record_instance(instance_name, proxy);
                            Some(WatchDispatch::Bind(entry, drivers))
                        }
                        WatchEvent::REMOVE_FILE => {
                            Some(WatchDispatch::Remove(self.remove_instance(filename)))
                        }
                        _ => None,
                    },
                );
                async move {
                    match dispatch {
                        Some(WatchDispatch::Bind(entry, drivers)) => {
                            let futures = drivers.into_iter().map(|driver| {
                                let entry = entry.clone();
                                async move {
                                    driver.handle_instance(self, weak_kernel, entry).await;
                                }
                            });
                            futures::future::join_all(futures).await;
                        }
                        Some(WatchDispatch::Remove(drivers)) => {
                            let futures = drivers.into_iter().map(|driver| async move {
                                driver.cleanup_stale_bindings(self, weak_kernel).await;
                            });
                            futures::future::join_all(futures).await;
                        }
                        None => {}
                    }
                    Ok(())
                }
            })
            .await
            .with_context(|| format!("error watching {} service", S::SERVICE_NAME))
    }
}

/// Per-[`Kernel`] manager that watches `/svc/<S::SERVICE_NAME>` at most once and fans out
/// discovered [`ServiceMarker::Proxy`] instances to registered Starnix drivers.
///
/// Active instances are retained so drivers registered after initial discovery still receive
/// previously discovered live instances, and each bound instance can register devices and
/// disconnect hooks on a [`ServiceBindingScope`] that are automatically torn down when a monitored
/// channel closes or when the service instance directory entry is removed or replaced.
#[derive(Debug)]
pub struct ServiceWatcher<S: ServiceMarker>(PhantomData<S>);

impl<S: ServiceMarker<Proxy: Send + Sync + 'static>> ServiceWatcher<S> {
    /// Registers a driver that binds each discovered service instance on the kernel's main async
    /// executor.
    pub fn register<Bind>(kernel: &Kernel, marker: S, bind: Bind)
    where
        Bind: FnMut(&CurrentTask, Arc<S::Proxy>, &mut ServiceBindingScope) + Send + 'static,
    {
        Self::register_driver(
            kernel,
            marker,
            /* singleton = */ false,
            async |proxy| Some(proxy),
            bind,
        );
    }

    /// Registers a singleton driver that binds at most one service instance at a time on the
    /// kernel's main async executor, ignoring additional instances while bound and promoting a
    /// remaining active instance if the bound instance disconnects.
    pub fn register_singleton<Bind>(kernel: &Kernel, marker: S, bind: Bind)
    where
        Bind: FnMut(&CurrentTask, Arc<S::Proxy>, &mut ServiceBindingScope) + Send + 'static,
    {
        Self::register_driver(
            kernel,
            marker,
            /* singleton = */ true,
            async |proxy| Some(proxy),
            bind,
        );
    }

    /// Registers a driver that probes each discovered service instance asynchronously (without
    /// holding kernel locks) and, when `probe` returns `Some(probed)`, invokes `bind` on the
    /// kernel's main async executor with
    /// [`KernelThreads::system_task`](crate::task::KernelThreads::system_task) and a
    /// [`ServiceBindingScope`].
    ///
    /// `probe` futures may run concurrently across instances, whereas `bind` and teardown
    /// invocations are serialized per driver after synchronously tearing down any stale bindings.
    /// Any active service instances discovered before this call are replayed to the driver on the
    /// kernel's main async executor.
    pub fn register_with_probe<P, Probe, ProbeFut, Bind>(
        kernel: &Kernel,
        marker: S,
        probe: Probe,
        bind: Bind,
    ) where
        P: Send + 'static,
        Probe: Fn(Arc<S::Proxy>) -> ProbeFut + Send + Sync + 'static,
        ProbeFut: Future<Output = Option<P>> + Send + 'static,
        Bind: FnMut(&CurrentTask, P, &mut ServiceBindingScope) + Send + 'static,
    {
        Self::register_driver(kernel, marker, /* singleton = */ false, probe, bind);
    }

    fn ensure_watcher_started(kernel: &Kernel) -> Arc<ServiceWatcherState<S>> {
        let state = kernel.expando.get::<ServiceWatcherState<S>>();
        if !state.watch_started.swap(true, Ordering::AcqRel) {
            let weak_kernel = kernel.weak_self.clone();
            let state_for_task = state.clone();
            kernel.kthreads.spawn_future(
                move || async move {
                    // Open the service directory inside the future so its channel receiver is
                    // registered on the `kthreads` executor rather than the caller's thread-local
                    // executor.
                    let result = match fclient::open_service::<S>() {
                        Ok(service_dir) => {
                            state_for_task.watch_service_dir(&weak_kernel, service_dir).await
                        }
                        Err(e) => Err(e.context("failed to open service directory")),
                    };
                    if let Err(e) = result {
                        log_info!("{}: service unavailable: {e:#}", S::SERVICE_NAME);
                    }
                },
                S::SERVICE_NAME,
            );
        }
        state
    }

    fn register_driver<P, Probe, ProbeFut, Bind>(
        kernel: &Kernel,
        _marker: S,
        singleton: bool,
        probe: Probe,
        bind: Bind,
    ) where
        P: Send + 'static,
        Probe: Fn(Arc<S::Proxy>) -> ProbeFut + Send + Sync + 'static,
        ProbeFut: Future<Output = Option<P>> + Send + 'static,
        Bind: FnMut(&CurrentTask, P, &mut ServiceBindingScope) + Send + 'static,
    {
        let state = Self::ensure_watcher_started(kernel);
        let driver: Arc<dyn ServiceDriver<S>> = DriverEntry::new(singleton, probe, bind);
        let existing_instances = {
            let mut inner = state.inner.lock();
            inner.instances.retain(|entry| entry.active.load(Ordering::Acquire));
            let instances = inner.instances.clone();
            inner.drivers.push(driver.clone());
            instances
        };
        if !existing_instances.is_empty() {
            let weak_kernel = kernel.weak_self.clone();
            kernel.kthreads.spawn_future(
                move || async move {
                    futures::future::join_all(
                        existing_instances
                            .into_iter()
                            .map(|instance| driver.handle_instance(&state, &weak_kernel, instance)),
                    )
                    .await;
                },
                "service_watcher_replay",
            );
        }
    }
}

struct SyncServiceInstanceOpener {
    service_dir: Arc<fio::DirectorySynchronousProxy>,
    instance_name: String,
}

impl MemberOpener for SyncServiceInstanceOpener {
    fn open_member(&self, member: &str, server_end: zx::Channel) -> Result<(), fidl::Error> {
        let path = format!("{}/{}", self.instance_name, member);
        self.service_dir.open(
            &path,
            fio::Flags::PROTOCOL_SERVICE,
            &fio::Options::default(),
            server_end,
        )
    }

    fn instance_name(&self) -> &str {
        &self.instance_name
    }
}

/// Connects `service_dir` to the per-[`Kernel`] [`ServiceWatcher`] for `S` on the kernel's main
/// async executor while suppressing the default `/svc/<S::SERVICE_NAME>` lookup.
#[cfg(test)]
pub(crate) fn watch_service_dir_for_testing<S>(kernel: &Kernel, service_dir: fio::DirectoryProxy)
where
    S: ServiceMarker<Proxy: Send + Sync + 'static>,
{
    let state = kernel.expando.get::<ServiceWatcherState<S>>();
    state.watch_started.store(true, Ordering::Release);
    let weak_kernel = kernel.weak_self.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            let _ = state.watch_service_dir(&weak_kernel, service_dir).await;
        },
        "service_watcher_test_dir",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mem::DevNull;
    use crate::device::{DeviceMode, simple_device_ops};
    use crate::testing::spawn_kernel_and_run;
    use fidl_fuchsia_hardware_power_stats as fpowerstats;
    use futures::channel::{mpsc, oneshot};
    use starnix_uapi::error;
    use std::collections::HashSet;
    use vfs::directory::helper::DirectlyMutable as _;
    use vfs::directory::immutable::Simple;

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
    async fn watches_multiple_instances_concurrently() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = vfs::directory::immutable::simple();
            add_instance(&dir, "first", |_| {});
            add_instance(&dir, "second", |_| {});

            let (second_tx, second_rx) = oneshot::channel();
            let second_tx = Arc::new(Mutex::new(Some(second_tx)));
            let second_rx = Arc::new(Mutex::new(Some(second_rx)));
            let (done_tx, done_rx) = oneshot::channel();
            let done_tx = Arc::new(Mutex::new(Some(done_tx)));
            let seen = Arc::new(Mutex::new(HashSet::new()));

            ServiceWatcher::register_with_probe(
                kernel,
                fpowerstats::ServiceMarker,
                move |instance| {
                    let second_tx = second_tx.clone();
                    let second_rx = second_rx.clone();
                    async move {
                        let name = instance.instance_name().to_string();
                        if name == "first" {
                            // Wait for `"second"` to enter `probe` before completing `"first"`,
                            // verifying that `probe` runs concurrently across instances.
                            let rx = second_rx.lock().take().expect("second_rx taken once");
                            let _ = rx.await;
                        } else if name == "second" {
                            if let Some(tx) = second_tx.lock().take() {
                                let _ = tx.send(());
                            }
                        }
                        Some(name)
                    }
                },
                move |_current_task, name, _scope| {
                    let mut seen = seen.lock();
                    seen.insert(name);
                    if seen.len() == 2
                        && let Some(tx) = done_tx.lock().take()
                    {
                        let _ = tx.send(std::mem::take(&mut *seen));
                    }
                },
            );

            let proxy = vfs::directory::serve_read_only(dir, vfs::ExecutionScope::new());
            watch_service_dir_for_testing::<fpowerstats::ServiceMarker>(kernel, proxy);

            let seen = done_rx.await.expect("both instances discovered concurrently");
            assert_eq!(seen, HashSet::from(["first".to_string(), "second".to_string()]));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn fans_out_to_multiple_drivers_and_replays_to_late_subscribers() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);

            let (early_tx, mut early_rx) = mpsc::unbounded();
            ServiceWatcher::register(
                kernel,
                fpowerstats::ServiceMarker,
                move |_current_task, instance, _scope| {
                    let _ = early_tx.unbounded_send(instance.instance_name().to_string());
                },
            );

            add_instance(&dir, "first", |_| {});
            assert_eq!(early_rx.next().await.as_deref(), Some("first"));

            // Register a second subscriber after `"first"` has already been discovered.
            let (late_tx, mut late_rx) = mpsc::unbounded();
            ServiceWatcher::register(
                kernel,
                fpowerstats::ServiceMarker,
                move |_current_task, instance, _scope| {
                    let _ = late_tx.unbounded_send(instance.instance_name().to_string());
                },
            );
            assert_eq!(late_rx.next().await.as_deref(), Some("first"));

            // Subsequent instances fan out to both early and late subscribers.
            add_instance(&dir, "second", |_| {});
            assert_eq!(early_rx.next().await.as_deref(), Some("second"));
            assert_eq!(late_rx.next().await.as_deref(), Some("second"));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn tears_down_scoped_devices_and_hooks_on_peer_close() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);
            let (event_tx, mut event_rx) = mpsc::unbounded();
            let (all_seen_tx, mut all_seen_rx) = mpsc::unbounded();
            let devts = Arc::new(Mutex::new(Vec::new()));
            let devts_for_bind = devts.clone();

            ServiceWatcher::register_singleton(
                kernel,
                fpowerstats::ServiceMarker,
                move |current_task, instance, scope| {
                    let name = instance.instance_name().to_string();
                    let proxy =
                        Arc::new(instance.connect_to_device_sync().expect("connect to device"));
                    scope.monitor_channel(proxy);

                    let tx1 = event_tx.clone();
                    let name1 = name.clone();
                    scope.on_disconnect(move |_| {
                        let _ = tx1.unbounded_send(format!("hook1:{name1}"));
                    });
                    let tx2 = event_tx.clone();
                    let name2 = name.clone();
                    scope.on_disconnect(move |_| {
                        let _ = tx2.unbounded_send(format!("hook2:{name2}"));
                    });

                    let kernel = current_task.kernel();
                    let misc_dev = scope.add_scoped_device(
                        kernel
                            .device_registry
                            .add_misc_device(
                                kernel,
                                "watcher_misc".into(),
                                simple_device_ops::<DevNull>,
                            )
                            .expect("add misc device"),
                    );
                    devts_for_bind.lock().push(misc_dev.metadata().expect("metadata").devt);
                    let _ = event_tx.unbounded_send(format!("bind:{name}"));
                },
            );
            ServiceWatcher::register(
                kernel,
                fpowerstats::ServiceMarker,
                move |_current_task, instance, _scope| {
                    let _ = all_seen_tx.unbounded_send(instance.instance_name().to_string());
                },
            );

            let (first_srv_tx, mut first_srv_rx) = mpsc::unbounded();
            add_instance(&dir, "first", move |ch| {
                let _ = first_srv_tx.unbounded_send(ch);
            });
            assert_eq!(event_rx.next().await.as_deref(), Some("bind:first"));
            assert_eq!(all_seen_rx.next().await.as_deref(), Some("first"));
            let first_server = first_srv_rx.next().await.expect("first server channel");

            let registry = &kernel.device_registry;
            let first_devt = devts.lock()[0];
            assert!(
                registry.objects.root.lookup("devices/virtual/misc/watcher_misc".into()).is_some()
            );
            assert!(registry.get_device(first_devt, DeviceMode::Char).is_ok());

            // Adding `"second"` while `"first"` is active is ignored by the singleton driver.
            let (second_srv_tx, mut second_srv_rx) = mpsc::unbounded();
            add_instance(&dir, "second", move |ch| {
                let _ = second_srv_tx.unbounded_send(ch);
            });
            assert_eq!(all_seen_rx.next().await.as_deref(), Some("second"));
            assert!(event_rx.try_recv().is_err());

            // Closing `"first"` runs teardown hooks in reverse registration order, frees the misc
            // device ID, and promotes `"second"`.
            drop(first_server);
            assert_eq!(event_rx.next().await.as_deref(), Some("hook2:first"));
            assert_eq!(event_rx.next().await.as_deref(), Some("hook1:first"));
            assert_eq!(event_rx.next().await.as_deref(), Some("bind:second"));
            let second_server = second_srv_rx.next().await.expect("second server channel");
            assert_eq!(devts.lock()[1], first_devt);

            // Closing `"second"` tears down the promoted binding and removes the misc device.
            drop(second_server);
            assert_eq!(event_rx.next().await.as_deref(), Some("hook2:second"));
            assert_eq!(event_rx.next().await.as_deref(), Some("hook1:second"));
            assert!(
                registry.objects.root.lookup("devices/virtual/misc/watcher_misc".into()).is_none()
            );
            assert_eq!(
                registry.get_device(first_devt, DeviceMode::Char).map(|_| ()),
                error!(ENODEV)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn tears_down_binding_on_directory_remove_or_replace() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);
            let (event_tx, mut event_rx) = mpsc::unbounded();
            let mut generation = 0u32;

            ServiceWatcher::register(
                kernel,
                fpowerstats::ServiceMarker,
                move |current_task, _instance, scope| {
                    generation += 1;
                    let current_gen = generation;
                    let tx_disconnect = event_tx.clone();
                    scope.on_disconnect(move |_| {
                        let _ = tx_disconnect.unbounded_send(format!("disconnect:{current_gen}"));
                    });
                    let kernel = current_task.kernel();
                    scope.add_scoped_device(
                        kernel
                            .device_registry
                            .add_misc_device(
                                kernel,
                                "watcher_replace_misc".into(),
                                simple_device_ops::<DevNull>,
                            )
                            .expect("add misc device"),
                    );
                    let _ = event_tx.unbounded_send(format!("bind:{current_gen}"));
                },
            );

            add_instance(&dir, "first", |_| {});
            assert_eq!(event_rx.next().await.as_deref(), Some("bind:1"));
            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("devices/virtual/misc/watcher_replace_misc".into()).is_some());

            // Replacing `"first"` in the watched directory tears down the existing binding (even
            // without a monitored channel) before binding the replacement.
            add_instance(&dir, "first", |_| {});
            assert_eq!(event_rx.next().await.as_deref(), Some("disconnect:1"));
            assert_eq!(event_rx.next().await.as_deref(), Some("bind:2"));
            assert!(root.lookup("devices/virtual/misc/watcher_replace_misc".into()).is_some());

            // Removing `"first"` from the directory tears down the active binding.
            dir.remove_entry("first", /* must_be_directory = */ false)
                .expect("remove service instance");
            assert_eq!(event_rx.next().await.as_deref(), Some("disconnect:2"));
            assert!(root.lookup("devices/virtual/misc/watcher_replace_misc".into()).is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn stops_watching_when_kernel_is_dropped() {
        let dir = vfs::directory::immutable::simple();
        add_instance(&dir, "first", |_| {});
        let service_dir = vfs::directory::serve_read_only(dir, vfs::ExecutionScope::new());
        let state = Arc::new(ServiceWatcherState::<fpowerstats::ServiceMarker>::default());
        let called = Arc::new(AtomicBool::new(false));
        let called_for_bind = called.clone();
        state.inner.lock().drivers.push(DriverEntry::new(
            /* singleton = */ false,
            async |_| Some(()),
            move |_, (), _| {
                called_for_bind.store(true, Ordering::SeqCst);
            },
        ));

        state
            .watch_service_dir(&Weak::new(), service_dir)
            .await
            .expect("service watcher should exit cleanly when kernel is dropped");

        assert!(!called.load(Ordering::SeqCst));
    }
}
