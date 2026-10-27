// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{StarnixKernel, generate_kernel_name};
use anyhow::Error;
use fidl::endpoints::{Proxy, ServerEnd};
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_power_system as fpower;
use frunner::{ComponentControllerMarker, ComponentStartInfo};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use futures::FutureExt;
use futures::channel::oneshot;
use starnix_sync::{
    ActivityGovernorProxyLock, KernelsLock, LockDepMutex, StarnixKernelWakeLeaseLock,
};
use std::collections::HashMap;
use std::sync::Arc;
use vfs::execution_scope::ExecutionScope;
use zx;

/// The component URL of the Starnix kernel.
const KERNEL_URL: &str = "starnix_kernel#meta/starnix_kernel.cm";

/// Create the power lease name for better readability based on the Starnix kernel name.
fn create_lease_name(kernel_name: &str) -> String {
    format!("starnix-kernel-{}", kernel_name)
}

type InFlightWakeLease = futures::future::Shared<oneshot::Receiver<()>>;

#[derive(Default)]
struct WakeLeaseState {
    /// Monotonically increasing generation bumped on every acquire or drop transition so stale
    /// in-flight completions never resurrect a dropped or superseded wake lease.
    generation: u64,
    /// Generation of the task currently stored in `in_flight`, so the latest background task clears
    /// `in_flight` upon completion even if `drop_wake_lease` bumped `generation` in the meantime.
    in_flight_generation: u64,
    /// The currently active wake lease for the container.
    lease: Option<zx::EventPair>,
    /// Completion handle for the latest in-flight `TakeApplicationActivityLease` task, which
    /// transitively awaits any earlier in-flight task for this kernel.
    in_flight: Option<InFlightWakeLease>,
}

/// Per-kernel wake lease tracker shared between [`Kernels`] and background acquisition tasks.
pub(crate) struct KernelWakeLease {
    lease_name: String,
    state: LockDepMutex<WakeLeaseState, StarnixKernelWakeLeaseLock>,
}

impl KernelWakeLease {
    pub(crate) fn new(kernel_name: &str) -> Self {
        Self {
            lease_name: create_lease_name(kernel_name),
            state: LockDepMutex::new(WakeLeaseState::default()),
        }
    }

    fn set_initial_lease(&self, lease: Option<zx::EventPair>) {
        let replaced = std::mem::replace(&mut self.state.lock().lease, lease);
        drop(replaced);
    }

    fn clear_on_stop(&self) {
        let stale_lease = {
            let mut state = self.state.lock();
            state.generation += 1;
            state.in_flight_generation = state.generation;
            state.in_flight = None;
            state.lease.take()
        };
        drop(stale_lease);
    }

    fn current_generation(&self) -> u64 {
        self.state.lock().generation
    }

    /// Stores `lease` if `target_generation` still matches the latest generation; otherwise returns
    /// `Err(lease)` so the caller drops the superseded lease outside the lock.
    fn finish_acquire(
        &self,
        target_generation: u64,
        lease: zx::EventPair,
    ) -> Result<(), zx::EventPair> {
        let replaced = {
            let mut state = self.state.lock();
            if state.generation != target_generation {
                return Err(lease);
            }
            state.lease.replace(lease)
        };
        drop(replaced);
        Ok(())
    }

    fn clear_in_flight_if_current(&self, target_generation: u64) {
        let mut state = self.state.lock();
        if state.in_flight_generation == target_generation {
            state.in_flight = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn has_lease(&self) -> bool {
        self.state.lock().lease.is_some()
    }
}

#[derive(Default)]
struct ActivityGovernorCache {
    proxy: LockDepMutex<Option<fpower::ActivityGovernorProxy>, ActivityGovernorProxyLock>,
}

impl ActivityGovernorCache {
    /// Returns a cached `ActivityGovernorProxy`, connecting to the protocol if no open connection
    /// is cached. Also returns whether the returned proxy came from the cache.
    fn get(&self) -> Result<(fpower::ActivityGovernorProxy, bool), Error> {
        let mut guard = self.proxy.lock();
        if let Some(proxy) = guard.as_ref() {
            if !proxy.is_closed() {
                return Ok((proxy.clone(), true));
            }
        }
        let proxy = connect_to_protocol::<fpower::ActivityGovernorMarker>()?;
        *guard = Some(proxy.clone());
        Ok((proxy, false))
    }

    /// Evicts `failed_proxy` from the cache if it is still the cached proxy or has closed.
    fn evict(&self, failed_proxy: &fpower::ActivityGovernorProxy) {
        let mut guard = self.proxy.lock();
        if guard.as_ref().is_some_and(|cached| {
            cached.is_closed()
                || cached.as_channel().as_ref().raw_handle()
                    == failed_proxy.as_channel().as_ref().raw_handle()
        }) {
            *guard = None;
        }
    }

    /// Requests an application activity lease for `lease_name` via `activity_governor`,
    /// reconnecting once if a previously cached proxy disconnected.
    async fn take_application_activity_lease(
        &self,
        activity_governor: fpower::ActivityGovernorProxy,
        was_cached: bool,
        lease_name: &str,
    ) -> Result<Option<zx::EventPair>, Error> {
        match activity_governor.take_application_activity_lease(lease_name).await {
            Ok(lease) => Ok(Some(lease)),
            Err(e) if was_cached && e.is_closed() => {
                self.evict(&activity_governor);
                let (activity_governor, _) = self.get()?;
                match activity_governor.take_application_activity_lease(lease_name).await {
                    Ok(lease) => Ok(Some(lease)),
                    Err(e) => {
                        if e.is_closed() {
                            self.evict(&activity_governor);
                        }
                        log::warn!(
                            "Failed to acquire application activity lease for kernel: {:?}",
                            e
                        );
                        Ok(None)
                    }
                }
            }
            Err(e) => {
                if e.is_closed() {
                    self.evict(&activity_governor);
                }
                log::warn!("Failed to acquire application activity lease for kernel: {:?}", e);
                Ok(None)
            }
        }
    }
}

/// [`Kernels`] manages a collection of starnix kernels.
pub struct Kernels {
    kernels: Arc<LockDepMutex<HashMap<zx::Koid, StarnixKernel>, KernelsLock>>,
    background_tasks: ExecutionScope,
    wake_lease_scope: fasync::Scope,
    activity_governor: Arc<ActivityGovernorCache>,
}

impl Kernels {
    /// Creates a new [`Kernels`] instance.
    pub fn new() -> Self {
        Self {
            kernels: Default::default(),
            background_tasks: ExecutionScope::new(),
            wake_lease_scope: fasync::Scope::new_with_name("starnix_wake_leases"),
            activity_governor: Arc::new(ActivityGovernorCache::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        kernel_job: Arc<zx::Job>,
        kernel_name: impl Into<String>,
        activity_governor: fpower::ActivityGovernorProxy,
    ) -> Result<Self, Error> {
        let kernels = Self::new();
        *kernels.activity_governor.proxy.lock() = Some(activity_governor);
        let (exposed_dir, _server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::DirectoryMarker>();
        let job_koid = kernel_job.koid()?;
        let name = kernel_name.into();
        let wake_lease = Arc::new(KernelWakeLease::new(&name));
        kernels.kernels.lock().insert(
            job_koid,
            StarnixKernel {
                name,
                exposed_dir,
                component_instance: zx::Event::create(),
                job: kernel_job,
                wake_lease,
            },
        );
        Ok(kernels)
    }

    #[cfg(test)]
    pub(crate) fn has_wake_lease(&self, container_job: &zx::Job) -> bool {
        let Ok(job_koid) = container_job.koid() else {
            return false;
        };
        self.kernels.lock().get(&job_koid).is_some_and(|kernel| kernel.wake_lease.has_lease())
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_in_flight_wake_lease(&self, container_job: &zx::Job) {
        let Ok(job_koid) = container_job.koid() else {
            return;
        };
        let in_flight = self
            .kernels
            .lock()
            .get(&job_koid)
            .and_then(|kernel| kernel.wake_lease.state.lock().in_flight.clone());
        if let Some(in_flight) = in_flight {
            let _ = in_flight.await;
        }
    }

    /// Runs a new starnix kernel and adds it to the collection.
    pub async fn start(
        &self,
        start_info: ComponentStartInfo,
        controller: ServerEnd<ComponentControllerMarker>,
    ) -> Result<(), Error> {
        let realm =
            connect_to_protocol::<fcomponent::RealmMarker>().expect("Failed to connect to realm.");

        let kernel_name = generate_kernel_name(&start_info)?;
        let lease_name = create_lease_name(&kernel_name);
        let wake_lease = match self.activity_governor.get() {
            Ok((activity_governor, was_cached)) => self
                .activity_governor
                .take_application_activity_lease(activity_governor, was_cached, &lease_name)
                .await
                .unwrap_or(None),
            Err(_) => None,
        };

        if let Ok(boot_control) = connect_to_protocol::<fpower::BootControlMarker>() {
            log::info!(
                "Notifying Fuchsia SystemActivityGovernor that Starnix has booted to allow suspend"
            );
            let _ = boot_control.set_boot_complete().await;
            if let Err(e) = boot_control.set_boot_complete().await {
                log::warn!(
                    "Failed to notify Fuchsia SystemActivityGovernor that Starnix has booted: {:?}",
                    e
                );
            }
        }

        let (kernel, on_stop) =
            StarnixKernel::create(realm, KERNEL_URL, start_info, controller).await?;
        let kernel_job = kernel.job.clone();
        let kernel_koid = kernel.job.koid()?;

        kernel.wake_lease.set_initial_lease(wake_lease);
        log::info!("Acquired wake lease for {:?}", kernel_job);

        self.kernels.lock().insert(kernel_koid, kernel);

        let kernels = self.kernels.clone();
        self.background_tasks.spawn(async move {
            on_stop.await;
            let removed = kernels.lock().remove(&kernel_koid);
            if let Some(removed) = removed {
                removed.wake_lease.clear_on_stop();
            }
        });

        Ok(())
    }

    /// Gets a momentary snapshot of all kernel jobs.
    pub fn all_jobs(&self) -> Vec<Arc<zx::Job>> {
        self.kernels.lock().iter().map(|(_, k)| Arc::clone(k.job())).collect()
    }

    /// Drops any active wake lease for the container running in the given `container_job`,
    /// waiting for any in-flight `TakeApplicationActivityLease` request for that container to
    /// resolve and drop its lease first.
    pub async fn drop_wake_lease(&self, container_job: &zx::Job) -> Result<(), Error> {
        // LINT.IfChange
        fuchsia_trace::instant!(
            "power",
            "starnix-runner:drop-application-activity-lease",
            fuchsia_trace::Scope::Process
        );
        // LINT.ThenChange(//src/performance/lib/trace_processing/metrics/suspend.py)
        let job_koid = container_job.koid()?;
        let wake_lease_tracker =
            self.kernels.lock().get(&job_koid).map(|kernel| Arc::clone(&kernel.wake_lease));
        if let Some(wake_lease_tracker) = wake_lease_tracker {
            let (active_lease, in_flight) = {
                let mut state = wake_lease_tracker.state.lock();
                state.generation += 1;
                (state.lease.take(), state.in_flight.clone())
            };
            drop(active_lease);
            if let Some(in_flight) = in_flight {
                let _ = in_flight.await;
            }
            log::info!("Dropped wake lease for {:?}", container_job);
        }
        Ok(())
    }

    /// Synchronously writes a `TakeApplicationActivityLease` request to System Activity Governor
    /// for `container_job` and spawns a background task on [`Kernels`]'s scope to store the lease
    /// once SAG replies.
    pub fn start_acquire_wake_lease(&self, container_job: &zx::Job) -> Result<(), Error> {
        let _ = self.start_acquire_wake_lease_inner(container_job)?;
        Ok(())
    }

    /// Acquires a wake lease for the container running in the given `container_job` and waits for
    /// SAG to return and store the lease.
    pub async fn acquire_wake_lease(&self, container_job: &zx::Job) -> Result<(), Error> {
        if let Some(in_flight) = self.start_acquire_wake_lease_inner(container_job)? {
            let _ = in_flight.await;
        }
        Ok(())
    }

    fn start_acquire_wake_lease_inner(
        &self,
        container_job: &zx::Job,
    ) -> Result<Option<InFlightWakeLease>, Error> {
        // LINT.IfChange
        fuchsia_trace::duration!("power", "starnix-runner:acquire-application-activity-lease");
        // LINT.ThenChange(//src/performance/lib/trace_processing/metrics/suspend.py)
        let job_koid = container_job.koid()?;

        let Some(wake_lease_tracker) =
            self.kernels.lock().get(&job_koid).map(|kernel| Arc::clone(&kernel.wake_lease))
        else {
            return Ok(None);
        };

        let (mut activity_governor, mut was_cached) = self.activity_governor.get()?;
        // Synchronously encode and write `TakeApplicationActivityLease` into SAG's channel before
        // returning so SAG starts resuming drivers immediately.
        let lease_fut = match activity_governor
            .take_application_activity_lease(&wake_lease_tracker.lease_name)
            .check()
        {
            Ok(fut) => fut,
            Err(e) if was_cached && e.is_closed() => {
                self.activity_governor.evict(&activity_governor);
                let (reconnected, _) = self.activity_governor.get()?;
                activity_governor = reconnected;
                was_cached = false;
                match activity_governor
                    .take_application_activity_lease(&wake_lease_tracker.lease_name)
                    .check()
                {
                    Ok(fut) => fut,
                    Err(e) => {
                        if e.is_closed() {
                            self.activity_governor.evict(&activity_governor);
                        }
                        log::warn!(
                            "Failed to acquire application activity lease for kernel: {:?}",
                            e
                        );
                        return Ok(None);
                    }
                }
            }
            Err(e) => {
                if e.is_closed() {
                    self.activity_governor.evict(&activity_governor);
                }
                log::warn!("Failed to acquire application activity lease for kernel: {:?}", e);
                return Ok(None);
            }
        };

        let (done_tx, done_rx) = oneshot::channel::<()>();
        let in_flight = done_rx.shared();
        let (target_generation, prev_in_flight) = {
            let mut state = wake_lease_tracker.state.lock();
            state.generation += 1;
            state.in_flight_generation = state.generation;
            let prev = state.in_flight.replace(in_flight.clone());
            (state.generation, prev)
        };

        let activity_governor_cache = Arc::clone(&self.activity_governor);
        let _join_handle = self.wake_lease_scope.spawn(async move {
            let _done_guard = scopeguard::guard(done_tx, |tx| {
                let _ = tx.send(());
            });

            let maybe_lease = match lease_fut.await {
                Ok(lease) => Some(lease),
                Err(e) if was_cached && e.is_closed() => {
                    activity_governor_cache.evict(&activity_governor);
                    if wake_lease_tracker.current_generation() != target_generation {
                        None
                    } else {
                        match activity_governor_cache.get() {
                            Ok((reconnected, _)) => {
                                match reconnected
                                    .take_application_activity_lease(&wake_lease_tracker.lease_name)
                                    .await
                                {
                                    Ok(lease) => Some(lease),
                                    Err(e) => {
                                        if e.is_closed() {
                                            activity_governor_cache.evict(&reconnected);
                                        }
                                        log::warn!(
                                            "Failed to acquire application activity lease for kernel: {:?}",
                                            e
                                        );
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to reconnect to ActivityGovernor for kernel wake lease: {:?}",
                                    e
                                );
                                None
                            }
                        }
                    }
                }
                Err(e) => {
                    if e.is_closed() {
                        activity_governor_cache.evict(&activity_governor);
                    }
                    log::warn!("Failed to acquire application activity lease for kernel: {:?}", e);
                    None
                }
            };

            if let Some(wake_lease) = maybe_lease {
                match wake_lease_tracker.finish_acquire(target_generation, wake_lease) {
                    Ok(()) => {
                        log::info!("Acquired wake lease for job koid {:?}", job_koid);
                    }
                    Err(stale_lease) => {
                        drop(stale_lease);
                        log::info!(
                            "Dropped superseded in-flight wake lease for job koid {:?}",
                            job_koid
                        );
                    }
                }
            }

            if let Some(prev) = prev_in_flight {
                let _ = prev.await;
            }
            wake_lease_tracker.clear_in_flight_if_current(target_generation);
        });

        Ok(Some(in_flight))
    }
}

impl Drop for Kernels {
    fn drop(&mut self) {
        self.background_tasks.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::pin::pin;
    use std::task::Poll;

    #[fuchsia::test]
    async fn acquire_wake_lease_reuses_cached_proxy_and_handles_disconnect() {
        let child_job = Arc::new(fuchsia_runtime::job_default().create_child_job().unwrap());
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Kernels::new_for_test(child_job.clone(), "test-kernel", proxy).unwrap();

        let served_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served_requests_clone = served_requests.clone();
        let server_task = fuchsia_async::Task::local(async move {
            for _ in 0..2 {
                let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
                    name,
                    responder,
                })) = stream.next().await
                else {
                    panic!("expected TakeApplicationActivityLease request");
                };
                assert_eq!(name, "starnix-kernel-test-kernel");
                served_requests_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (lease_client, _lease_server) = zx::EventPair::create();
                responder.send(lease_client).unwrap();
            }
            // Drop `stream` after two leases to simulate channel disconnection.
        });

        kernels.acquire_wake_lease(&child_job).await.unwrap();
        assert!(kernels.has_wake_lease(&child_job));
        kernels.drop_wake_lease(&child_job).await.unwrap();
        assert!(!kernels.has_wake_lease(&child_job));

        // Second wake reuses the same cached `ActivityGovernorProxy` without reconnecting.
        kernels.acquire_wake_lease(&child_job).await.unwrap();
        assert!(kernels.has_wake_lease(&child_job));
        assert_eq!(served_requests.load(std::sync::atomic::Ordering::Relaxed), 2);
        kernels.drop_wake_lease(&child_job).await.unwrap();

        server_task.await;

        // After the cached channel closes, `acquire_wake_lease` evicts the closed proxy and falls
        // back cleanly without panicking.
        kernels.acquire_wake_lease(&child_job).await.unwrap();
        assert!(!kernels.has_wake_lease(&child_job));
    }

    #[fuchsia::test]
    async fn start_acquire_wake_lease_writes_request_immediately_and_stores_on_reply() {
        let child_job = Arc::new(fuchsia_runtime::job_default().create_child_job().unwrap());
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Kernels::new_for_test(child_job.clone(), "test-kernel", proxy).unwrap();

        // `start_acquire_wake_lease` returns synchronously after writing the FIDL request to the
        // channel, before SAG replies.
        kernels.start_acquire_wake_lease(&child_job).unwrap();
        assert!(!kernels.has_wake_lease(&child_job));

        let responder = {
            let mut next_request = pin!(stream.next());
            let Poll::Ready(Some(Ok(
                fpower::ActivityGovernorRequest::TakeApplicationActivityLease { name, responder },
            ))) = futures::poll!(&mut next_request)
            else {
                panic!("expected TakeApplicationActivityLease already queued in channel");
            };
            assert_eq!(name, "starnix-kernel-test-kernel");
            responder
        };
        assert!(!kernels.has_wake_lease(&child_job));

        // Once SAG replies, the background scope task stores the lease and keeps it alive until
        // `drop_wake_lease`.
        let (lease_client, lease_server) = zx::EventPair::create();
        responder.send(lease_client).unwrap();
        kernels.wait_for_in_flight_wake_lease(&child_job).await;
        assert!(kernels.has_wake_lease(&child_job));
        assert_eq!(
            lease_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result(),
            Err(zx::Status::TIMED_OUT)
        );

        kernels.drop_wake_lease(&child_job).await.unwrap();
        assert!(!kernels.has_wake_lease(&child_job));
        assert!(
            lease_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result()
                .is_ok()
        );
    }

    #[fuchsia::test]
    async fn drop_wake_lease_waits_for_in_flight_acquire_and_drops_lease() {
        let child_job = Arc::new(fuchsia_runtime::job_default().create_child_job().unwrap());
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Kernels::new_for_test(child_job.clone(), "test-kernel", proxy).unwrap();

        kernels.start_acquire_wake_lease(&child_job).unwrap();
        let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
            name,
            responder,
        })) = stream.next().await
        else {
            panic!("expected TakeApplicationActivityLease request");
        };
        assert_eq!(name, "starnix-kernel-test-kernel");

        // Rapid re-suspend calls `drop_wake_lease` while `TakeApplicationActivityLease` is still in
        // flight on SAG. `drop_wake_lease` must remain pending until SAG replies and the stale
        // lease is dropped.
        let mut drop_fut = pin!(kernels.drop_wake_lease(&child_job));
        assert!(matches!(futures::poll!(&mut drop_fut), Poll::Pending));

        let (lease_client, lease_server) = zx::EventPair::create();
        responder.send(lease_client).unwrap();
        drop_fut.await.unwrap();

        assert!(!kernels.has_wake_lease(&child_job));
        assert!(
            lease_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result()
                .is_ok()
        );
    }

    #[fuchsia::test]
    async fn newer_acquire_supersedes_out_of_order_earlier_in_flight_lease() {
        let child_job = Arc::new(fuchsia_runtime::job_default().create_child_job().unwrap());
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Kernels::new_for_test(child_job.clone(), "test-kernel", proxy).unwrap();

        kernels.start_acquire_wake_lease(&child_job).unwrap();
        kernels.start_acquire_wake_lease(&child_job).unwrap();

        let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
            responder: responder1,
            ..
        })) = stream.next().await
        else {
            panic!("expected first TakeApplicationActivityLease request");
        };
        let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
            responder: responder2,
            ..
        })) = stream.next().await
        else {
            panic!("expected second TakeApplicationActivityLease request");
        };

        let (lease2_client, lease2_server) = zx::EventPair::create();
        responder2.send(lease2_client).unwrap();
        let (lease1_client, lease1_server) = zx::EventPair::create();
        responder1.send(lease1_client).unwrap();

        kernels.wait_for_in_flight_wake_lease(&child_job).await;
        assert!(kernels.has_wake_lease(&child_job));
        // Earlier lease must be closed immediately; latest lease must remain active.
        assert!(
            lease1_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result()
                .is_ok()
        );
        assert_eq!(
            lease2_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result(),
            Err(zx::Status::TIMED_OUT)
        );
    }

    #[fuchsia::test]
    async fn cancelled_drop_wake_lease_preserves_in_flight_tracking_for_retry() {
        let child_job = Arc::new(fuchsia_runtime::job_default().create_child_job().unwrap());
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Kernels::new_for_test(child_job.clone(), "test-kernel", proxy).unwrap();

        kernels.start_acquire_wake_lease(&child_job).unwrap();
        let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
            responder,
            ..
        })) = stream.next().await
        else {
            panic!("expected TakeApplicationActivityLease request");
        };

        // Poll `drop_wake_lease` once (pending on SAG's reply) and drop/cancel the future.
        {
            let mut cancelled_drop = pin!(kernels.drop_wake_lease(&child_job));
            assert!(matches!(futures::poll!(&mut cancelled_drop), Poll::Pending));
        }

        // A subsequent `drop_wake_lease` must still observe the in-flight acquisition and wait
        // until SAG replies and the superseded lease is dropped.
        let mut retry_drop = pin!(kernels.drop_wake_lease(&child_job));
        assert!(matches!(futures::poll!(&mut retry_drop), Poll::Pending));

        let (lease_client, lease_server) = zx::EventPair::create();
        responder.send(lease_client).unwrap();
        retry_drop.await.unwrap();

        assert!(!kernels.has_wake_lease(&child_job));
        assert!(
            lease_server
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
                .to_result()
                .is_ok()
        );
        // Once the background task completes, `in_flight` must be cleared despite `drop_wake_lease`
        // having bumped `state.generation`.
        let job_koid = child_job.koid().unwrap();
        assert!(
            kernels
                .kernels
                .lock()
                .get(&job_koid)
                .unwrap()
                .wake_lease
                .state
                .lock()
                .in_flight
                .is_none()
        );
    }
}
