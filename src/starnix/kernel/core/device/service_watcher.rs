// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Asynchronous watching for Fuchsia FIDL service directories (`/svc/<ServiceName>`).

use crate::task::Kernel;
use anyhow::Context as _;
use fidl::endpoints::ServiceMarker;
use fuchsia_component::client as fclient;
use futures::{StreamExt as _, TryStreamExt as _};
use starnix_logging::log_info;
use std::sync::Weak;

/// Opens `marker` in the process's `/svc` namespace and watches for service instances on the
/// kernel's main async executor.
///
/// Discovery runs as a non-blocking task scheduled via
/// [`KernelThreads::spawn_future`](crate::task::KernelThreads::spawn_future) rather than on a
/// dedicated blocking kernel thread. `on_instance` is invoked concurrently for each discovered
/// instance and receives a [`Weak<Kernel>`] so callers can perform any asynchronous initialization
/// on the instance before upgrading to a strong [`Kernel`] reference.
pub fn watch_fidl_service<S, F>(kernel: &Kernel, marker: S, task_name: &'static str, on_instance: F)
where
    S: ServiceMarker + Send + 'static,
    F: AsyncFn(&Weak<Kernel>, S::Proxy) + Send + 'static,
{
    let weak_kernel = kernel.weak_self.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            // Open the service directory inside the future so its channel receiver is registered
            // on the `kthreads` executor rather than the caller's thread-local executor.
            let result = match fclient::Service::open(marker) {
                Ok(service) => run_fidl_service_watcher(&weak_kernel, service, on_instance).await,
                Err(e) => Err(e.context("failed to open service directory")),
            };
            if let Err(e) = result {
                log_info!("{}: service unavailable: {e:#}", S::SERVICE_NAME);
            }
        },
        task_name,
    );
}

/// Drives a [`fclient::ServiceInstanceStream`] for `service`, invoking `on_instance` concurrently
/// for each discovered instance while `kernel` remains alive.
pub(crate) async fn run_fidl_service_watcher<S, F>(
    kernel: &Weak<Kernel>,
    service: fclient::Service<S>,
    on_instance: F,
) -> Result<(), anyhow::Error>
where
    S: ServiceMarker,
    F: AsyncFn(&Weak<Kernel>, S::Proxy),
{
    let on_instance = &on_instance;
    service
        .watch()
        .await
        .with_context(|| format!("failed to watch {} service", S::SERVICE_NAME))?
        .take_while(|_| std::future::ready(kernel.strong_count() > 0))
        .try_for_each_concurrent(None, |instance| async move {
            on_instance(kernel, instance).await;
            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use fidl::endpoints::create_endpoints;
    use fidl_fuchsia_hardware_power_stats as fpowerstats;
    use fidl_fuchsia_io as fio;
    use fuchsia_async as fasync;
    use fuchsia_component::server::ServiceFs;
    use futures::channel::oneshot;
    use futures::future::{Either, select};
    use starnix_sync::Mutex;
    use std::collections::HashSet;
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn serve_instances(
        names: &'static [&'static str],
    ) -> fclient::Service<fpowerstats::ServiceMarker> {
        let (client, server) = create_endpoints::<fio::DirectoryMarker>();
        std::thread::spawn(move || {
            fasync::LocalExecutor::default().run_singlethreaded(async move {
                let mut fs = ServiceFs::new_local();
                for name in names {
                    fs.add_fidl_service_instance(*name, |_request: fpowerstats::ServiceRequest| ());
                }
                fs.serve_connection(server).expect("serve service directory");
                fs.collect::<()>().await;
            })
        });
        fclient::Service::open_from_dir(client, fpowerstats::ServiceMarker).expect("open service")
    }

    #[::fuchsia::test]
    async fn watches_multiple_instances() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let service = serve_instances(&["first", "second"]);
            let seen = Mutex::new(HashSet::new());
            let (second_tx, second_rx) = oneshot::channel();
            let second_tx = Mutex::new(Some(second_tx));
            let second_rx = Mutex::new(Some(second_rx));
            let (done_tx, done_rx) = oneshot::channel();
            let done_tx = Mutex::new(Some(done_tx));

            let watcher = pin!(run_fidl_service_watcher(
                &kernel.weak_self,
                service,
                async |weak_kernel, instance| {
                    assert!(weak_kernel.upgrade().is_some());
                    let name = instance.instance_name().to_string();
                    if name == "first" {
                        // Wait for `"second"` to enter `on_instance` before completing `"first"`,
                        // verifying that `on_instance` runs concurrently across instances.
                        let rx = second_rx.lock().take().expect("second_rx taken once");
                        let _ = rx.await;
                    } else if name == "second" {
                        if let Some(tx) = second_tx.lock().take() {
                            let _ = tx.send(());
                        }
                    }
                    let mut seen = seen.lock();
                    seen.insert(name);
                    if seen.len() == 2 {
                        if let Some(tx) = done_tx.lock().take() {
                            let _ = tx.send(std::mem::take(&mut *seen));
                        }
                    }
                }
            ));

            let seen = match select(watcher, done_rx).await {
                Either::Left((result, _)) => {
                    panic!("watcher exited before both instances were discovered: {result:?}");
                }
                Either::Right((Ok(seen), _)) => seen,
                Either::Right((Err(e), _)) => {
                    panic!("oneshot sender dropped before signalling: {e:?}");
                }
            };

            assert_eq!(seen, HashSet::from(["first".to_string(), "second".to_string()]));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn stops_watching_when_kernel_is_dropped() {
        let service = serve_instances(&["first", "second"]);
        let called = AtomicBool::new(false);

        run_fidl_service_watcher(&Weak::new(), service, async |_, _| {
            called.store(true, Ordering::SeqCst);
        })
        .await
        .expect("service watcher should exit cleanly when kernel is dropped");

        assert!(!called.load(Ordering::SeqCst));
    }
}
