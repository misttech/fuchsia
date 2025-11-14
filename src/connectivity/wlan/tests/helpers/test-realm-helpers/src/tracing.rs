// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, format_err};
use fidl::endpoints::Proxy;
use fuchsia_component::client::connect_to_protocol_at;
use zx::HandleBased;

use futures::AsyncReadExt;
use log::{info, warn};
use std::io::Write;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

/// An RAII-style struct that starts tracing in the test realm upon creation via `Tracing::start`
/// and collects and writes the trace when the struct is dropped.
pub struct Tracing {
    inner_ptr: AtomicPtr<TracingInner>,
}

struct TracingInner {
    controller: fidl_fuchsia_tracing_controller::SessionSynchronousProxy,
    collector: std::thread::JoinHandle<Result<Vec<u8>, anyhow::Error>>,
}

impl Tracing {
    pub async fn start(test_ns_prefix: &str) -> Result<Arc<Self>, anyhow::Error> {
        let launcher =
            connect_to_protocol_at::<fidl_fuchsia_tracing_controller::ProvisionerMarker>(
                test_ns_prefix,
            )
            .map_err(|e| format_err!("Failed to get tracing controller: {e:?}"))?;
        let (socket_read, socket_write) = fidl::Socket::create_stream();
        let (controller, controller_server) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_tracing_controller::SessionMarker>();
        launcher
            .initialize_tracing(
                controller_server,
                &fidl_fuchsia_tracing_controller::TraceConfig {
                    categories: Some(vec!["*".to_string()]),
                    buffer_size_megabytes_hint: Some(64),
                    ..Default::default()
                },
                socket_write,
            )
            .map_err(|e| format_err!("Failed to initialize tracing: {e:?}"))?;

        let collector = std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut tracing_socket = fuchsia_async::Socket::from_socket(socket_read);
                info!("draining trace record socket...");
                let mut buf = Vec::new();
                tracing_socket
                    .read_to_end(&mut buf)
                    .await
                    .map_err(|e| format_err!("Failed to drain trace record socket: {e:?}"))?;
                info!("trace record socket drained: {} bytes", buf.len());
                Ok(buf)
            })
        });

        controller
            .start_tracing(&fidl_fuchsia_tracing_controller::StartOptions::default())
            .await
            .map_err(|e| format_err!("Encountered FIDL error when starting trace: {e:?}"))?
            .map_err(|e| format_err!("Failed to start tracing: {e:?}"))?;

        let controller = fidl_fuchsia_tracing_controller::SessionSynchronousProxy::new(
            fidl::Channel::from_handle(
                controller
                    .into_channel()
                    .map_err(|e| format_err!("Failed to get fidl::AsyncChannel from proxy: {e:?}"))?
                    .into_zx_channel()
                    .into_handle(),
            ),
        );

        // The goal of this arrangement is to enable lockless termination of a trace upon either
        // dropping Tracing or panic in this process, and the termination only happens once. The
        // following achieves that by returning an Arc<Tracing> and wrapping a leaked TracingInner
        // with an AtomicPtr.
        let inner = Box::new(TracingInner { controller, collector });
        let inner_ptr = AtomicPtr::new(Box::leak(inner));
        let self_ = Arc::new(Self { inner_ptr });
        let tracing = Arc::downgrade(&self_);

        // Set a panic hook so a trace will be written upon panic. This is required because Fuchsia
        // uses the abort panic strategy. If the unwind strategy were used, then the Tracing
        // destructors would run and this hook (and the AtomicPtr) would not be necessary.
        let panic_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            tracing.upgrade().and_then(|tracing| {
                tracing.take_inner().map(|inner| {
                    let _ = inner.terminate_and_collect_trace().map_err(|e| warn!("{e:?}"));
                })
            });
            panic_hook(panic_info);
        }));

        Ok(self_)
    }

    fn take_inner(&self) -> Option<Box<TracingInner>> {
        let ptr = self.inner_ptr.swap(null_mut(), Ordering::SeqCst);
        (!ptr.is_null()).then(|| {
            // SAFETY: This pointer is non-null and the AtomicPtr::swap ensures it's the only copy.
            unsafe { Box::from_raw(ptr) }
        })
    }
}

impl Drop for Tracing {
    fn drop(&mut self) {
        self.take_inner().map(|inner| {
            let _: Result<(), ()> = inner.terminate_and_collect_trace().map_err(|e| warn!("{e:?}"));
        });
    }
}

impl TracingInner {
    fn terminate_and_collect_trace(self) -> Result<(), anyhow::Error> {
        // Stop with write_results set to true. Otherwise, at least some part of the trace will not be written.
        let stop_result = self
            .controller
            .stop_tracing(
                &fidl_fuchsia_tracing_controller::StopOptions {
                    write_results: Some(true),
                    ..Default::default()
                },
                zx::MonotonicInstant::INFINITE,
            )
            .map_err(|e| format_err!("Failed to stop tracing: {e:?}"))?
            .map_err(|e| format_err!("Failed to stop tracing: {e:?}"))?;
        info!("Trace stopped. Result: {stop_result:?}");

        // Drop the Session proxy to terminate the trace. This triggers the socket reading the trace to close
        // after the last trace record is written.
        drop(self.controller);

        let trace = self
            .collector
            .join()
            .map_err(|e| format_err!("Failed to join tracing collector thread: {e:?}"))?
            .context(format_err!("Failed to collect trace."))?;

        let fxt_path = format!("/custom_artifacts/trace.fxt");
        let mut fxt_file = std::fs::File::create(&fxt_path)
            .map_err(|e| format_err!("Failed to create {}: {e:?}", &fxt_path))?;
        fxt_file
            .write_all(&trace[..])
            .map_err(|e| format_err!("Failed to write to {}: {e:?}", &fxt_path))?;
        fxt_file.sync_all().map_err(|e| format_err!("Failed to sync to {}: {e:?}", &fxt_path))?;
        Ok(())
    }
}
