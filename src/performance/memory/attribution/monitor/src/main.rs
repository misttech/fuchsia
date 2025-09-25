// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use attribution_data::AttributionDataProviderImpl;
use attribution_processing::AttributionDataProvider;
use attribution_processing::digest::BucketDefinition;
use cobalt::{collect_metrics_forever, collect_stalls_forever, create_metric_event_logger};
use fidl::endpoints::{ControlHandle, RequestStream};
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at_path};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::Property;
use fuchsia_inspect::health::Reporter;
use fuchsia_sync::Mutex;
use fuchsia_trace::duration;
use futures::{FutureExt, StreamExt, TryFutureExt, select};
use log::{error, info, warn};
use memory_monitor2_config::Config;
use resources::Job;
use snapshot::AttributionSnapshot;
use stalls::StallProvider;
use stalls::refaults::RefaultProvider;
use std::sync::Arc;
use traces::CATEGORY_MEMORY_CAPTURE;
use zx::{BootInstant, MonotonicInstant};

use {
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_kernel as fkernel,
    fidl_fuchsia_memory_attribution as fattribution,
    fidl_fuchsia_memory_attribution_plugin as fattribution_plugin,
    fidl_fuchsia_memorypressure as fpressure, fidl_fuchsia_metrics as fmetrics,
};

mod attribution_client;
mod attribution_data;
mod common;
mod resources;
mod snapshot;

/// All FIDL services that are exposed by this component's ServiceFs.
enum Service {
    /// The `fuchsia.memory.attribution.plugin.MemoryMonitor` protocol.
    MemoryMonitor(fattribution_plugin::MemoryMonitorRequestStream),
    /// The `fuchsia.memory.attribution.PageRefaultSink` protocol.
    PageRefaultSink(fattribution::PageRefaultSinkRequestStream),
}

const INTROSPECTOR_PATH: &str = "/svc/fuchsia.component.Introspector.root";

// Enable debug trace:
// 1. set `logging_minimum_severity = "debug"`
// 2. run `fx log --severity trace --moniker core/memory_monitor2`
#[fuchsia::main(logging_minimum_severity = "info")]
async fn main() -> Result<()> {
    info!("Starting memory_monitor 2");
    fuchsia_inspect::component::health().set_starting_up();
    let mut service_fs = ServiceFs::new();
    service_fs
        .dir("svc")
        .add_fidl_service(Service::MemoryMonitor)
        .add_fidl_service(Service::PageRefaultSink);
    service_fs.take_and_serve_directory_handle()?;

    let attribution_provider = connect_to_protocol::<fattribution::ProviderMarker>()
        .context("Failed to connect to the memory attribution provider")?;
    let introspector =
        connect_to_protocol_at_path::<fcomponent::IntrospectorMarker>(&INTROSPECTOR_PATH)
            .context("Failed to connect to the memory attribution provider")?;
    let root_job: Mutex<Box<dyn Job>> = Mutex::new(Box::new(
        connect_to_protocol::<fkernel::RootJobForInspectMarker>()
            .context("error connecting to the root job")?
            .get()
            .await?,
    ));
    let attribution_client = attribution_client::AttributionClientImpl::new(
        attribution_provider,
        introspector,
        root_job.lock().get_koid().context("Unable to get the root job's koid")?,
    );

    let kernel_stats = connect_to_protocol::<fkernel::StatsMarker>()
        .context("Failed to connect to the kernel stats provider")?;

    let stall_provider = stalls::StallProviderImpl::new(Arc::new(
        connect_to_protocol::<fkernel::StallResourceMarker>()?.get().await?,
    ))?;

    let page_refault_tracker = stalls::refaults::RefaultProviderImpl::default();

    let root_node = fuchsia_inspect::component::inspector().root();
    let task_health_node = root_node.create_child("task health");

    // Serves Fuchsia performance trace system.
    // https://fuchsia.dev/fuchsia-src/concepts/kernel/tracing-system
    // Watch trace category and trace kernel memory stats, until this variable goes out of scope.
    let mut kernel_trace_service = fuchsia_async::Task::spawn(traces::kernel::serve_forever(
        kernel_stats.clone(),
        stall_provider.clone(),
        page_refault_tracker.clone(),
    ))
    .fuse();
    let kernel_trace_health = task_health_node.create_string("kernel_trace_service", "ok");

    let attribution_data_provider = AttributionDataProviderImpl::new(attribution_client, root_job);
    let bucket_definitions: Arc<[BucketDefinition]> = read_bucket_definitions().into();
    let config = Config::take_from_startup_handle();
    // Serves Fuchsia component inspection protocol
    // https://fuchsia.dev/fuchsia-src/development/diagnostics/inspect
    let mut inspect_nodes_service = fuchsia_async::Task::spawn(inspect_nodes::serve(
        kernel_stats.clone(),
        stall_provider.clone(),
        Config { ..config }, // Config is not Clone
    )?)
    .fuse();

    let mut pressure_service = fuchsia_async::Task::spawn(
        pressure_monitoring::serve_to_inspect(
            config,
            attribution_data_provider.clone(),
            stall_provider.clone(),
            kernel_stats.clone(),
            connect_to_protocol::<fpressure::ProviderMarker>()
                .context("Failed to connect to the memory pressure provider")?,
            bucket_definitions.clone(),
            root_node.create_child("logger"),
        )
        .inspect_ok(|_| error!("Digest service exited without error"))
        .inspect_err(|e| error!("Digest service failed: {:?}", e)),
    )
    .fuse();
    let pressure_health = task_health_node.create_string("pressure_monitoring_service", "ok");

    let metric_event_logger_factory =
        connect_to_protocol::<fmetrics::MetricEventLoggerFactoryMarker>()?;
    let mut collect_metrics_task = fuchsia_async::Task::spawn(collect_metrics_forever(
        attribution_data_provider.clone(),
        kernel_stats.clone(),
        create_metric_event_logger(metric_event_logger_factory.clone()).await?,
        bucket_definitions.clone(),
    ))
    .fuse();
    let collect_metrics_health = task_health_node.create_string("collect_metrics_health", "ok");

    let mut collect_stalls_task = fuchsia_async::Task::spawn(collect_stalls_forever(
        stall_provider.clone(),
        create_metric_event_logger(metric_event_logger_factory).await?,
    ))
    .fuse();
    let collect_stalls_health = task_health_node.create_string("collect_stalls_health", "ok");
    let page_refault_tracker = stalls::refaults::RefaultProviderImpl::default();

    let mut services = service_fs.for_each_concurrent(None, |stream| async {
        match stream {
            Service::MemoryMonitor(stream) => {
                if let Err(error) = serve_client_stream(
                    stream,
                    bucket_definitions.clone(),
                    attribution_data_provider.clone(),
                    kernel_stats.clone(),
                    stall_provider.clone(),
                    page_refault_tracker.clone(),
                )
                .await
                {
                    warn!(error:%; "");
                }
            }
            Service::PageRefaultSink(stream) => {
                if let Err(e) = page_refault_tracker.listen_to_page_refaults(stream).await {
                    warn!("PageRefaultSink disconnected: {:?}", e);
                }
            }
        }
    });
    let servicefs_health = task_health_node.create_string("servicefs_health", "ok");

    fuchsia_inspect::component::health().set_ok();

    loop {
        select! {
            _ = services => {
                servicefs_health.set("stopped");
                error!("Stopped serving requests");
            },
            _ = kernel_trace_service => {
                kernel_trace_health.set("stopped");
                error!("Stopped providing traces");
            },
            _ = inspect_nodes_service => error!("No longer serving inspect!"),
            result = pressure_service => {
                pressure_health.set(&result.err().map_or_else(||"stopped".to_string(), |err| format!("{:?}", err)));
                error!("Stopped monitoring pressure");
            },
            _ = collect_metrics_task => {
                collect_metrics_health.set("stopped");
                error!("Stopped collecting metrics");
            },
            result = collect_stalls_task => {
                collect_stalls_health.set(&(result.err().map_or_else(||"stopped".to_string(), |err| format!("{:?}", err))));
                error!("Stopped collecting stalls");
            },
            complete => break,
        };
        fuchsia_inspect::component::health().set_unhealthy("One or more services unhealthy");
    }
    error!("Stopping memory_monitor 2");
    Ok(())
}

async fn serve_client_stream(
    mut stream: fattribution_plugin::MemoryMonitorRequestStream,
    bucket_definitions: Arc<[BucketDefinition]>,
    attribution_data_provider: Arc<AttributionDataProviderImpl>,
    kernel_stats_proxy: fkernel::StatsProxy,
    stall_provider: impl StallProvider,
    refault_tracker: impl RefaultProvider,
) -> Result<()> {
    while let Some(request) = stream.next().await.transpose()? {
        match request {
            fattribution_plugin::MemoryMonitorRequest::GetSnapshot { snapshot, control_handle } => {
                if let Err(err) = provide_snapshot(
                    attribution_data_provider.clone(),
                    kernel_stats_proxy.clone(),
                    stall_provider.clone(),
                    refault_tracker.clone(),
                    bucket_definitions.clone(),
                    snapshot,
                )
                .await
                {
                    // Errors from `serve_snapshot` are all internal errors, not client-induced.
                    error!(err:%; "");
                    control_handle.shutdown_with_epitaph(zx::Status::INTERNAL);
                }
            }
            fattribution_plugin::MemoryMonitorRequest::GetSystemStatistics { responder } => {
                if let Err(err) = provide_statistics(
                    kernel_stats_proxy.clone(),
                    stall_provider.clone(),
                    refault_tracker.clone(),
                    responder,
                )
                .await
                {
                    error!(err:%; "");
                }
            }
            fattribution_plugin::MemoryMonitorRequest::_UnknownMethod { .. } => {
                stream.control_handle().shutdown_with_epitaph(zx::Status::NOT_SUPPORTED);
            }
        }
    }
    Ok(())
}

/// Constructs a [Snapshot] and sends it, serialized, through the `snapshot` socket.
async fn provide_snapshot(
    attribution_data_provider: Arc<AttributionDataProviderImpl>,
    kernel_stats_proxy: fkernel::StatsProxy,
    stall_provider: impl StallProvider,
    refault_tracker: impl RefaultProvider,
    bucket_definitions: Arc<[BucketDefinition]>,
    snapshot: zx::Socket,
) -> Result<()> {
    duration!(CATEGORY_MEMORY_CAPTURE, c"provide_snapshot");
    let attribution_data = attribution_data_provider.get_attribution_data().await?;

    let kernel_stats = fattribution_plugin::KernelStatistics {
        memory_stats: Some(kernel_stats_proxy.get_memory_stats().await?),
        compression_stats: Some(kernel_stats_proxy.get_memory_stats_compression().await?),
        ..Default::default()
    };

    let memory_stalls = stall_provider.get_stall_info()?;

    let attribution_snapshot = AttributionSnapshot::new(
        attribution_data,
        kernel_stats,
        memory_stalls,
        refault_tracker,
        &*bucket_definitions,
    );
    attribution_snapshot.serve(snapshot).await
}

/// Looks for a bucket definitions configuration, to perform memory
/// aggregations for reporting purposes. Returns an empty list if no
/// such configuration was found.
fn read_bucket_definitions() -> Vec<BucketDefinition> {
    std::fs::File::open("/config/data/buckets.json")
        .inspect_err(|err| warn!(err:%; "Could not access the bucket definitions configuration"))
        .ok()
        .and_then(|file| {
            serde_json::from_reader(file)
                .inspect_err(
                    |err| warn!(err:%; "Could not read the bucket definitions configuration"),
                )
                .ok()
        })
        .unwrap_or_default()
}

async fn provide_statistics(
    kernel_stats_proxy: fkernel::StatsProxy,
    stall_provider: impl StallProvider,
    refault_tracker: impl RefaultProvider,
    responder: fattribution_plugin::MemoryMonitorGetSystemStatisticsResponder,
) -> Result<()> {
    let kernel_stats = fattribution_plugin::KernelStatistics {
        memory_stats: Some(kernel_stats_proxy.get_memory_stats().await?),
        compression_stats: Some(kernel_stats_proxy.get_memory_stats_compression().await?),
        ..Default::default()
    };

    let memory_stalls = stall_provider.get_stall_info()?;
    let refaults = refault_tracker.get_count();

    responder.send(&fattribution_plugin::MemoryStatistics {
        time: Some(fattribution_plugin::Time {
            boot_time: Some(BootInstant::get()),
            monotonic_time: Some(MonotonicInstant::get()),
            ..Default::default()
        }),
        kernel_statistics: Some(kernel_stats),
        performance_metrics: Some(fattribution_plugin::PerformanceImpactMetrics {
            some_memory_stalls_ns: Some(memory_stalls.some.as_nanos().try_into()?),
            full_memory_stalls_ns: Some(memory_stalls.full.as_nanos().try_into()?),
            page_refaults: Some(refaults),
            ..Default::default()
        }),
        ..Default::default()
    })?;
    Ok(())
}
