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
use fuchsia_component::client::connect_to_protocol;
use starnix_sync::{ActivityGovernorProxyLock, KernelsLock, LockDepMutex};
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

/// [`Kernels`] manages a collection of starnix kernels.
pub struct Kernels {
    kernels: Arc<LockDepMutex<HashMap<zx::Koid, StarnixKernel>, KernelsLock>>,
    background_tasks: ExecutionScope,
    activity_governor:
        LockDepMutex<Option<fpower::ActivityGovernorProxy>, ActivityGovernorProxyLock>,
}

impl Kernels {
    /// Creates a new [`Kernels`] instance.
    pub fn new() -> Self {
        Self {
            kernels: Default::default(),
            background_tasks: ExecutionScope::new(),
            activity_governor: LockDepMutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        kernel_job: Arc<zx::Job>,
        kernel_name: impl Into<String>,
        activity_governor: fpower::ActivityGovernorProxy,
    ) -> Result<Self, Error> {
        let kernels = Self::new();
        *kernels.activity_governor.lock() = Some(activity_governor);
        let (exposed_dir, _server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_io::DirectoryMarker>();
        let job_koid = kernel_job.koid()?;
        kernels.kernels.lock().insert(
            job_koid,
            StarnixKernel {
                name: kernel_name.into(),
                exposed_dir,
                component_instance: zx::Event::create(),
                job: kernel_job,
                wake_lease: LockDepMutex::new(None),
            },
        );
        Ok(kernels)
    }

    #[cfg(test)]
    pub(crate) fn has_wake_lease(&self, container_job: &zx::Job) -> bool {
        let Ok(job_koid) = container_job.koid() else {
            return false;
        };
        self.kernels.lock().get(&job_koid).is_some_and(|kernel| kernel.wake_lease.lock().is_some())
    }

    /// Returns a cached `ActivityGovernorProxy`, connecting to the protocol if no open connection
    /// is cached. Also returns whether the returned proxy came from the cache.
    fn activity_governor(&self) -> Result<(fpower::ActivityGovernorProxy, bool), Error> {
        let mut guard = self.activity_governor.lock();
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
    fn evict_activity_governor(&self, failed_proxy: &fpower::ActivityGovernorProxy) {
        let mut guard = self.activity_governor.lock();
        if guard.as_ref().is_some_and(|cached| {
            cached.is_closed()
                || cached.as_channel().as_ref().raw_handle()
                    == failed_proxy.as_channel().as_ref().raw_handle()
        }) {
            *guard = None;
        }
    }

    /// Requests an application activity lease for `kernel_name` via `activity_governor`,
    /// reconnecting once if a previously cached proxy disconnected.
    async fn take_application_activity_lease(
        &self,
        activity_governor: fpower::ActivityGovernorProxy,
        was_cached: bool,
        kernel_name: &str,
    ) -> Result<Option<zx::EventPair>, Error> {
        let lease_name = create_lease_name(kernel_name);
        match activity_governor.take_application_activity_lease(&lease_name).await {
            Ok(lease) => Ok(Some(lease)),
            Err(e) if was_cached && e.is_closed() => {
                self.evict_activity_governor(&activity_governor);
                let (activity_governor, _) = self.activity_governor()?;
                match activity_governor.take_application_activity_lease(&lease_name).await {
                    Ok(lease) => Ok(Some(lease)),
                    Err(e) => {
                        if e.is_closed() {
                            self.evict_activity_governor(&activity_governor);
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
                    self.evict_activity_governor(&activity_governor);
                }
                log::warn!("Failed to acquire application activity lease for kernel: {:?}", e);
                Ok(None)
            }
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
        let wake_lease = match self.activity_governor() {
            Ok((activity_governor, was_cached)) => self
                .take_application_activity_lease(activity_governor, was_cached, &kernel_name)
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

        *kernel.wake_lease.lock() = wake_lease;
        log::info!("Acquired wake lease for {:?}", kernel_job);

        self.kernels.lock().insert(kernel_koid, kernel);

        let kernels = self.kernels.clone();
        self.background_tasks.spawn(async move {
            on_stop.await;
            _ = kernels.lock().remove(&kernel_koid);
        });

        Ok(())
    }

    /// Gets a momentary snapshot of all kernel jobs.
    pub fn all_jobs(&self) -> Vec<Arc<zx::Job>> {
        self.kernels.lock().iter().map(|(_, k)| Arc::clone(k.job())).collect()
    }

    /// Drops any active wake lease for the container running in the given `container_job`.
    pub fn drop_wake_lease(&self, container_job: &zx::Job) -> Result<(), Error> {
        // LINT.IfChange
        fuchsia_trace::instant!(
            "power",
            "starnix-runner:drop-application-activity-lease",
            fuchsia_trace::Scope::Process
        );
        // LINT.ThenChange(//src/performance/lib/trace_processing/metrics/suspend.py)
        let job_koid = container_job.koid()?;
        if let Some(kernel) = self.kernels.lock().get(&job_koid) {
            kernel.wake_lease.lock().take();
            log::info!("Dropped wake lease for {:?}", container_job);
        }
        Ok(())
    }

    /// Acquires a wake lease for the container running in the given `container_job`.
    pub async fn acquire_wake_lease(&self, container_job: &zx::Job) -> Result<(), Error> {
        // LINT.IfChange
        fuchsia_trace::duration!("power", "starnix-runner:acquire-application-activity-lease");
        // LINT.ThenChange(//src/performance/lib/trace_processing/metrics/suspend.py)
        let job_koid = container_job.koid()?;

        let kernel_name = {
            let guard = self.kernels.lock();
            let Some(kernel) = guard.get(&job_koid) else {
                return Ok(());
            };
            kernel.name.clone()
        };

        let (activity_governor, was_cached) = self.activity_governor()?;
        let Some(wake_lease) = self
            .take_application_activity_lease(activity_governor, was_cached, &kernel_name)
            .await?
        else {
            return Ok(());
        };

        if let Some(kernel) = self.kernels.lock().get(&job_koid) {
            *kernel.wake_lease.lock() = Some(wake_lease);
            log::info!("Acquired wake lease for {:?}", container_job);
        }

        Ok(())
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
        kernels.drop_wake_lease(&child_job).unwrap();
        assert!(!kernels.has_wake_lease(&child_job));

        // Second wake reuses the same cached `ActivityGovernorProxy` without reconnecting.
        kernels.acquire_wake_lease(&child_job).await.unwrap();
        assert!(kernels.has_wake_lease(&child_job));
        assert_eq!(served_requests.load(std::sync::atomic::Ordering::Relaxed), 2);
        kernels.drop_wake_lease(&child_job).unwrap();

        server_task.await;

        // After the cached channel closes, `acquire_wake_lease` evicts the closed proxy and falls
        // back cleanly without panicking.
        kernels.acquire_wake_lease(&child_job).await.unwrap();
        assert!(!kernels.has_wake_lease(&child_job));
    }
}
