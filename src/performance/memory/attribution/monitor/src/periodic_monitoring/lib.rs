// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Result};
use attribution_processing::digest::{BucketDefinition, Digest};
use attribution_processing::summary::MemorySummary;
use attribution_processing::{
    AttributionData, AttributionDataProvider, ProcessedAttributionData, attribute_vmos,
};
use fuchsia_async::WakeupTime;
use fuchsia_inspect::{ArrayProperty, Node, StringProperty};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_trace::{category_enabled, duration};
use fuchsia_trace_observer::TraceObserver;
use futures::stream::FusedStream;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, join, select, try_join};
use humansize::{BINARY, FormatSizeOptions, format_size};
use stalls::StallProvider;
use stalls::refaults::PageFaultProvider;
use std::future::Future;
use traces::{CATEGORY_MEMORY_CAPTURE, CATEGORY_MEMORY_SUPPRESS_PERIODIC};

use fidl_fuchsia_kernel as fkernel;
use fidl_fuchsia_metrics as fmetrics;

const PERIODIC_MONITORING_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_minutes(5);

/// Waits while periodic monitoring is suppressed (`is_suppressed() == true`), awaiting
/// `on_state_changed()` between checks. Breaks out and falls through if `on_state_changed()`
/// returns an error.
async fn wait_while_periodic_suppressed<Fut, T, E>(
    mut is_suppressed: impl FnMut() -> bool,
    mut on_state_changed: impl FnMut() -> Fut,
) where
    Fut: Future<Output = Result<T, E>>,
{
    while is_suppressed() {
        if on_state_changed().await.is_err() {
            break;
        }
    }
}

/// Data collected during an on-demand snapshot that can be reused to update Inspect historical
/// data without performing a redundant periodic collection.
pub struct ManualMonitoringData {
    pub timestamp: zx::BootInstant,
    pub attribution_data: AttributionData,
    pub kmem_stats: fkernel::MemoryStats,
    pub kmem_stats_compression: fkernel::MemoryStatsCompression,
    pub done_tx: Option<futures::channel::oneshot::Sender<()>>,
}

/// Notification sent by the on-demand snapshot handler to `periodic_monitoring`.
pub enum ManualMonitoringEvent {
    /// Signals that an on-demand snapshot has started, resetting the periodic timer immediately
    /// so a periodic collection does not race with the in-progress snapshot.
    Started,
    /// Delivers the collected snapshot data to be recorded in Inspect history and resets the
    /// periodic timer.
    Completed(Box<ManualMonitoringData>),
}

/// Waits for [PERIODIC_MONITORING_INTERVAL], restarting the countdown when an on-demand snapshot
/// starts and returning `Some(data)` immediately if a completed manual snapshot newer than
/// `last_timestamp` arrives to be recorded in Inspect history, or `None` when the periodic timer
/// fires. Queued events are drained and coalesced so only the newest `Completed` snapshot is
/// processed.
async fn wait_for_next_collection(
    manual_events: &mut (impl FusedStream<Item = ManualMonitoringEvent> + Unpin),
    last_timestamp: zx::BootInstant,
) -> Option<Box<ManualMonitoringData>> {
    let mut timer = std::pin::pin!(PERIODIC_MONITORING_INTERVAL.into_timer().fuse());
    loop {
        select! {
            _ = timer => return None,
            first_event = manual_events.select_next_some() => {
                timer.set(PERIODIC_MONITORING_INTERVAL.into_timer().fuse());
                let mut latest_completed: Option<Box<ManualMonitoringData>> = None;
                let mut next_event = Some(first_event);
                while let Some(event) = next_event {
                    if let ManualMonitoringEvent::Completed(data) = event {
                        let is_newer_than_last = data.timestamp > last_timestamp;
                        let is_newer_than_candidate = latest_completed
                            .as_ref()
                            .is_none_or(|prev| data.timestamp >= prev.timestamp);
                        if is_newer_than_last && is_newer_than_candidate {
                            latest_completed = Some(data);
                        }
                    }
                    next_event = if manual_events.is_terminated() {
                        None
                    } else {
                        manual_events.next().now_or_never().flatten()
                    };
                }
                if latest_completed.is_some() {
                    return latest_completed;
                }
            }
        }
    }
}

/// Periodically collect and report memory attribution data.
///
/// This produces a simplified schedule where, instead of having
/// independent reports on their own cadence, we collect based on
/// cobalt's frequency and produce all other reports based on that one
/// collection, saving significant CPU at the expense of flexibility.
pub async fn periodic_monitoring(
    kernel_stats_proxy: fkernel::StatsProxy,
    attribution_data_service: &impl AttributionDataProvider,
    stall_provider: &impl StallProvider,
    page_fault_provider: &impl PageFaultProvider,
    metric_event_logger: &fmetrics::MetricEventLoggerProxy,
    bucket_definitions: &[BucketDefinition],
    inspect_root: Node,
    manual_events: impl Stream<Item = ManualMonitoringEvent> + Unpin,
) -> Result<()> {
    let mut _current: Option<StringProperty> = None; // Ensure the inspect property is kept as long as necessary.
    let mut bucket_list_node = std::cell::OnceCell::new();
    let bucket_names = std::cell::OnceCell::new();
    let bucket_codes = cobalt::prepare_bucket_codes(bucket_definitions);
    let trace_observer = TraceObserver::new();
    let mut manual_events = manual_events.fuse();
    let mut manual_collection: Option<Box<ManualMonitoringData>> = None;
    loop {
        if manual_collection.is_none() {
            // Defer periodic collection while a trace session with `memory:suppress_periodic` is
            // actively recording so the expensive VMO/VMAR walk does not perturb traced benchmarks.
            wait_while_periodic_suppressed(
                || category_enabled(CATEGORY_MEMORY_SUPPRESS_PERIODIC),
                || trace_observer.on_state_changed(),
            )
            .await;
        }
        let mut manual_done_tx = None;
        let timestamp = {
            duration!(CATEGORY_MEMORY_CAPTURE, c"periodic_monitoring");
            let (timestamp, kmem_stats, kmem_stats_compression, raw_attribution_data) =
                match manual_collection.take() {
                    None => {
                        let timestamp = zx::BootInstant::get();
                        // Retrieve (concurrently) the data necessary to perform the aggregation.
                        let (kmem_stats, kmem_stats_compression) = try_join!(
                            kernel_stats_proxy.get_memory_stats().map_err(anyhow::Error::from),
                            kernel_stats_proxy
                                .get_memory_stats_compression()
                                .map_err(anyhow::Error::from)
                        )
                        .with_context(|| "Failed to get kernel memory stats")?;
                        // This is the very expensive operation.
                        let raw_attribution_data =
                            attribution_data_service.get_attribution_data()?;
                        (timestamp, kmem_stats, kmem_stats_compression, raw_attribution_data)
                    }
                    Some(manual) => {
                        let ManualMonitoringData {
                            timestamp,
                            attribution_data,
                            kmem_stats,
                            kmem_stats_compression,
                            done_tx,
                        } = *manual;
                        manual_done_tx = done_tx;
                        (timestamp, kmem_stats, kmem_stats_compression, attribution_data)
                    }
                };
            let attribution_data = attribute_vmos(raw_attribution_data);
            let digest = Digest::compute(
                &attribution_data,
                &kmem_stats,
                &kmem_stats_compression,
                bucket_definitions,
                false,
            )?;
            drop(_current.take());
            _current = Some(update_inspect_summary(
                attribution_data,
                timestamp,
                &kmem_stats,
                &inspect_root,
            ));
            cobalt::upload_metrics(
                timestamp,
                &kmem_stats,
                metric_event_logger,
                &digest,
                &bucket_codes,
            )
            .await?;
            {
                // Initialize the inspect property containing the buckets names, if necessary.
                let _ = bucket_names.get_or_init(|| {
                    // Create inspect node to store buckets related information.
                    let bucket_names =
                        inspect_root.create_string_array("buckets", digest.buckets.len());
                    for (i, attribution_processing::digest::Bucket { name, .. }) in
                        digest.buckets.iter().enumerate()
                    {
                        bucket_names.set(i, name);
                    }
                    bucket_names
                });
            }
            update_inspect_history(
                timestamp,
                &digest,
                stall_provider,
                page_fault_provider,
                &mut bucket_list_node,
                &inspect_root,
            )?;
            timestamp
        };
        let (_, next) = join!(
            fuchsia_async::Task::local(async move {
                let _ = scudo::mallopt(scudo::M_PURGE_ALL, 0);
                if let Some(done_tx) = manual_done_tx {
                    let _ = done_tx.send(());
                }
            }),
            wait_for_next_collection(&mut manual_events, timestamp)
        );
        manual_collection = next;
    }
}

fn update_inspect_summary(
    attribution_data: ProcessedAttributionData,
    timestamp: zx::BootInstant,
    kmem_stats: &fkernel::MemoryStats,
    inspect_root: &Node,
) -> StringProperty {
    inspect_root.create_string(
        "current",
        record_summary(attribution_data.summary(), timestamp, &kmem_stats),
    )
}

/// Update inspect data with collected memory information.
fn update_inspect_history(
    timestamp: zx::BootInstant,
    digest: &Digest,
    stall_provider: &impl StallProvider,
    page_fault_provider: &impl PageFaultProvider,
    bucket_list_node: &mut std::cell::OnceCell<BoundedListNode>,
    inspect_root: &Node,
) -> Result<()> {
    let stall_values =
        stall_provider.get_stall_info().with_context(|| "Unable to retrieve stall information")?;
    let page_faults = page_fault_provider.get_fault_count();
    let page_refaults = page_fault_provider.get_refault_count();
    // Add an entry for the current aggregation.
    let _ = bucket_list_node
        .get_or_init(|| BoundedListNode::new(inspect_root.create_child("measurements"), 100));
    bucket_list_node.get_mut().unwrap().add_entry(|n| {
        n.record_int("timestamp", timestamp.into_nanos());
        {
            let committed_sizes = n.create_uint_array("bucket_sizes", digest.buckets.len());
            let populated_sizes =
                n.create_uint_array("bucket_sizes_populated", digest.buckets.len());
            for (i, b) in digest.buckets.iter().enumerate() {
                committed_sizes.set(i, b.committed_size as u64);
                populated_sizes.set(i, b.populated_size as u64);
            }
            n.record(committed_sizes);
            n.record(populated_sizes);
        }

        n.record_child("stalls", |child| {
            child.record_uint(
                "some_ms",
                stall_values.some.as_millis().try_into().unwrap_or(u64::MAX),
            );
            child.record_uint(
                "full_ms",
                stall_values.full.as_millis().try_into().unwrap_or(u64::MAX),
            );
        });

        n.record_child("faults", |child| {
            child.record_uint("count", page_faults);
        });

        n.record_child("refaults", |child| {
            child.record_uint("count", page_refaults);
        });
    });
    Ok(())
}

fn record_summary(
    mut summary: MemorySummary,
    timestamp: zx::Instant<zx::BootTimeline>,
    kmem_stats: &fkernel::MemoryStats,
) -> String {
    let size_options = FormatSizeOptions::from(BINARY).space_after_value(false);
    summary.principals.sort_by_key(|p| std::cmp::Reverse(p.populated_private));
    format!(
        "Time: {} VMO: {} Free: {}\n{}",
        timestamp.into_nanos(),
        kmem_stats
            .vmo_bytes
            .and_then(|b| Some(format_size(b, size_options)))
            .unwrap_or_else(|| "?".to_string()),
        kmem_stats
            .free_bytes
            .and_then(|b| Some(format_size(b, size_options)))
            .unwrap_or_else(|| "?".to_string()),
        summary
            .principals
            .iter_mut()
            .filter_map(|principal| {
                if principal.populated_total == 0 {
                    return None;
                }
                let (populated_private, populated_scaled, populated_total) = match (|| {
                    Some((
                        format_size(principal.populated_private, size_options),
                        format_size(principal.populated_scaled as u64, size_options),
                        format_size(principal.populated_total, size_options),
                    ))
                })(
                ) {
                    Some(ok) => ok,
                    None => return None,
                };
                let mut vmos = principal.vmos.iter().collect::<Vec<_>>();
                vmos.sort_by_key(|(_, vmo)| {
                    std::cmp::Reverse((vmo.committed_private, vmo.committed_scaled as u64))
                });
                let sizes = if populated_total == populated_private {
                    format_args!("{}", populated_total)
                } else {
                    format_args!("{} {} {}", populated_private, populated_scaled, populated_total)
                };
                Some(format!(
                    "{}: {}; {}",
                    principal.name,
                    sizes,
                    vmos.iter()
                        .filter_map(|(name, vmo)| {
                            if vmo.committed_total == 0 {
                                None
                            } else {
                                Some(format!(
                                    "{} {} {} {}",
                                    name,
                                    format_size(vmo.populated_private, size_options),
                                    format_size(vmo.populated_scaled as u64, size_options),
                                    format_size(vmo.populated_total, size_options)
                                ))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                ))
            })
            .collect::<Vec<_>>()
            .join("\n")
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use attribution_processing::{
        Attribution, AttributionData, GlobalPrincipalIdentifier, Principal, PrincipalDescription,
        PrincipalType, Resource, ResourceReference, ZXName,
    };
    use diagnostics_assertions::{NonZeroIntProperty, assert_data_tree};
    use std::num::NonZero;
    use std::time::Duration;

    use fidl_fuchsia_memory_attribution_plugin as fplugin;

    fn get_kernel_stats() -> (fkernel::MemoryStats, fkernel::MemoryStatsCompression) {
        (
            fkernel::MemoryStats {
                total_bytes: Some(1),
                free_bytes: Some(2),
                wired_bytes: Some(3),
                total_heap_bytes: Some(4),
                free_heap_bytes: Some(5),
                vmo_bytes: Some(6),
                mmu_overhead_bytes: Some(7),
                ipc_bytes: Some(8),
                other_bytes: Some(9),
                free_loaned_bytes: Some(10),
                cache_bytes: Some(11),
                slab_bytes: Some(12),
                zram_bytes: Some(13),
                vmo_reclaim_total_bytes: Some(14),
                vmo_reclaim_newest_bytes: Some(15),
                vmo_reclaim_oldest_bytes: Some(16),
                vmo_reclaim_disabled_bytes: Some(17),
                vmo_discardable_locked_bytes: Some(18),
                vmo_discardable_unlocked_bytes: Some(19),
                ..Default::default()
            },
            fkernel::MemoryStatsCompression {
                uncompressed_storage_bytes: Some(20),
                compressed_storage_bytes: Some(21),
                compressed_fragmentation_bytes: Some(22),
                compression_time: Some(23),
                decompression_time: Some(24),
                total_page_compression_attempts: Some(25),
                failed_page_compression_attempts: Some(26),
                total_page_decompressions: Some(27),
                compressed_page_evictions: Some(28),
                eager_page_compressions: Some(29),
                memory_pressure_page_compressions: Some(30),
                critical_memory_page_compressions: Some(31),
                pages_decompressed_unit_ns: Some(32),
                pages_decompressed_within_log_time: Some([40, 41, 42, 43, 44, 45, 46, 47]),

                ..Default::default()
            },
        )
    }

    fn get_raw_attribution_data() -> AttributionData {
        AttributionData {
            principals_vec: vec![Principal {
                identifier: GlobalPrincipalIdentifier(NonZero::new(1).unwrap()),
                description: Some(PrincipalDescription::Component("principal".to_owned())),
                principal_type: PrincipalType::Runnable,
                parent: None,
            }],
            resources_vec: vec![Resource {
                koid: 10,
                name_index: 0,
                resource_type: fplugin::ResourceType::Vmo(fplugin::Vmo {
                    parent: None,
                    private_committed_bytes: Some(1024),
                    private_populated_bytes: Some(2048),
                    scaled_committed_bytes: Some(1024),
                    scaled_populated_bytes: Some(2048),
                    total_committed_bytes: Some(1024),
                    total_populated_bytes: Some(2048),
                    ..Default::default()
                }),
            }],
            resource_names: vec![ZXName::from_string_lossy("resource")],
            attributions: vec![Attribution {
                source: GlobalPrincipalIdentifier(NonZero::new(1).unwrap()),
                subject: GlobalPrincipalIdentifier(NonZero::new(1).unwrap()),
                resources: vec![ResourceReference::KernelObject(10)],
            }],
        }
    }

    fn get_attribution_data() -> ProcessedAttributionData {
        attribute_vmos(get_raw_attribution_data())
    }

    #[derive(Clone)]
    struct FakeStallProvider {}
    impl StallProvider for FakeStallProvider {
        fn get_stall_info(&self) -> Result<stalls::MemoryStallMetrics, anyhow::Error> {
            Ok(stalls::MemoryStallMetrics {
                some: Duration::from_millis(10),
                full: Duration::from_millis(20),
            })
        }
    }

    #[derive(Clone)]
    struct FakePageFaultProvider {
        refault_count: u64,
        fault_count: u64,
    }

    impl PageFaultProvider for FakePageFaultProvider {
        fn get_refault_count(&self) -> u64 {
            self.refault_count
        }

        fn get_fault_count(&self) -> u64 {
            self.fault_count
        }
    }

    #[fuchsia::test]
    async fn test_update_inspect() -> Result<()> {
        let inspector = fuchsia_inspect::Inspector::default();
        let digest_node = inspector.root().create_child("logger");
        let timestamp = zx::BootInstant::get();
        let attribution_data = get_attribution_data();
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = Digest::compute(
            &attribution_data,
            &kernel_stats,
            &kernel_stats_compression,
            &vec![],
            false,
        )?;
        let mut bucket_list_node = std::cell::OnceCell::new();
        // Update inspect history twice, and ensure both instances are recorded.
        let _summary =
            update_inspect_summary(attribution_data, timestamp, &kernel_stats, &digest_node);
        update_inspect_history(
            timestamp,
            &digest,
            &FakeStallProvider {},
            &FakePageFaultProvider { refault_count: 300, fault_count: 400 },
            &mut bucket_list_node,
            &digest_node,
        )?;

        update_inspect_history(
            timestamp,
            &digest,
            &FakeStallProvider {},
            &FakePageFaultProvider { refault_count: 700, fault_count: 800 },
            &mut bucket_list_node,
            &digest_node,
        )?;
        assert_data_tree!(inspector, root: {
            logger: {
                measurements: {
                    // First update.
                    "0": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            // Orphaned: vmo_bytes reported by the kernel but not covered by any
                            // bucket => 6 - 1024 => 0 (saturating, cannot be negative)
                            0u64,
                            54u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other + 12 slab + 11 cache = 54
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                            6u64,    // [Addl]PopulatedAnonymousBytes
                        ],
                        bucket_sizes_populated: vec![
                            2048u64, // Undigested: matches the single unmatched VMO
                            // Orphaned: vmo_bytes reported by the kernel but not covered by any
                            // bucket => 6 - 1024 => 0 (saturating, cannot be negative)
                            0u64,
                            54u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other + 12 slab + 11 cache = 54
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                            6u64,    // [Addl]PopulatedAnonymousBytes
                        ],

                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                        faults: {
                            count: 400u64,
                        },
                        refaults: {
                            count: 300u64,
                        },
                    },
                    // Second update.
                    "1": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            // Orphaned: vmo_bytes reported by the kernel but not covered by any
                            // bucket => 6 - 1024 => 0 (saturating, cannot be negative)
                            0u64,
                            54u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other + 12 slab + 11 cache = 54
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                            6u64,    // [Addl]PopulatedAnonymousBytes
                        ],
                        bucket_sizes_populated: vec![
                            2048u64, // Undigested: matches the single unmatched VMO
                            // Orphaned: vmo_bytes reported by the kernel but not covered by any
                            // bucket => 6 - 1024 => 0 (saturating, cannot be negative)
                            0u64,
                            54u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other + 12 slab + 11 cache = 54
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                            6u64,    // [Addl]PopulatedAnonymousBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                        faults: {
                            count: 800u64,
                        },
                        refaults: {
                            count: 700u64,
                        },
                    },
                },
                current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB")?,
            },
        });
        Ok(())
    }

    #[test]
    fn test_wait_while_periodic_suppressed_defers_while_category_enabled() {
        use std::cell::{Cell, RefCell};

        let mut exec = fuchsia_async::TestExecutor::new();
        let is_suppressed = Cell::new(true);
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<(), zx::Status>>();
        let rx = RefCell::new(rx);

        {
            let mut wait_fut = std::pin::pin!(wait_while_periodic_suppressed(
                || is_suppressed.get(),
                || async { rx.borrow_mut().next().await.unwrap_or(Err(zx::Status::PEER_CLOSED)) },
            ));

            // While `memory:suppress_periodic` is enabled, the future stays pending.
            assert!(exec.run_until_stalled(&mut wait_fut).is_pending());

            // A state change notification while `memory:suppress_periodic` is still enabled
            // keeps waiting.
            tx.unbounded_send(Ok(())).unwrap();
            assert!(exec.run_until_stalled(&mut wait_fut).is_pending());

            // Once the trace stops (or `memory:suppress_periodic` is disabled) and the observer
            // signals, the wait completes.
            is_suppressed.set(false);
            tx.unbounded_send(Ok(())).unwrap();
            assert!(exec.run_until_stalled(&mut wait_fut).is_ready());
        }

        // If `on_state_changed` returns an error while suppressed, `wait_while_periodic_suppressed`
        // breaks out of the loop and completes instead of busy-waiting.
        is_suppressed.set(true);
        let mut wait_fut = std::pin::pin!(wait_while_periodic_suppressed(
            || is_suppressed.get(),
            || async { rx.borrow_mut().next().await.unwrap_or(Err(zx::Status::PEER_CLOSED)) },
        ));
        assert!(exec.run_until_stalled(&mut wait_fut).is_pending());
        tx.unbounded_send(Err(zx::Status::BAD_STATE)).unwrap();
        assert!(exec.run_until_stalled(&mut wait_fut).is_ready());
    }

    struct CountingAttributionDataProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl AttributionDataProvider for CountingAttributionDataProvider {
        fn get_attribution_data(&self) -> Result<AttributionData, anyhow::Error> {
            self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(get_raw_attribution_data())
        }
    }

    #[test]
    fn test_periodic_monitoring_reuses_manual_and_resets_timer() -> Result<()> {
        let mut exec = fuchsia_async::TestExecutor::new_with_fake_time();
        let start = exec.now();

        let (kernel_stats_proxy, mut kernel_stats_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();
        let _kernel_stats_task = fuchsia_async::Task::local(async move {
            while let Some(Ok(request)) = kernel_stats_stream.next().await {
                let (mem_stats, comp_stats) = get_kernel_stats();
                match request {
                    fkernel::StatsRequest::GetMemoryStats { responder } => {
                        let _ = responder.send(&mem_stats);
                    }
                    fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                        let _ = responder.send(&comp_stats);
                    }
                    _ => unreachable!(),
                }
            }
        });

        let (metric_event_logger, mut metric_event_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmetrics::MetricEventLoggerMarker>();
        let _metrics_task = fuchsia_async::Task::local(async move {
            while let Some(Ok(request)) = metric_event_stream.next().await {
                if let fmetrics::MetricEventLoggerRequest::LogMetricEvents { responder, .. } =
                    request
                {
                    let _ = responder.send(Ok(()));
                }
            }
        });

        let inspector = fuchsia_inspect::Inspector::default();
        let attribution_provider =
            CountingAttributionDataProvider { call_count: std::sync::atomic::AtomicUsize::new(0) };
        let stall_provider = FakeStallProvider {};
        let page_fault_provider = FakePageFaultProvider { refault_count: 300, fault_count: 400 };
        let (tx, rx) = futures::channel::mpsc::unbounded::<ManualMonitoringEvent>();

        let mut monitoring_fut = std::pin::pin!(periodic_monitoring(
            kernel_stats_proxy,
            &attribution_provider,
            &stall_provider,
            &page_fault_provider,
            &metric_event_logger,
            &[],
            inspector.root().create_child("logger"),
            rx,
        ));

        // Initial periodic collection runs immediately at t = 0.
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Advance 4 minutes (< 5 minutes): no new periodic collection yet.
        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(4));
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Signal an on-demand snapshot start at t = 4m, resetting the 5-minute countdown.
        tx.unbounded_send(ManualMonitoringEvent::Started).unwrap();
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());

        // At t = 6m (past the initial 5m deadline, but only 2m after the reset), no collection.
        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(6));
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // At t = 9m (5m after the reset at t = 4m), the periodic timer fires.
        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(9));
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 2);

        // At t = 10m, send a stale manual collection (timestamp = 1ns) followed by two new
        // manual collections back-to-back; only the newest valid one should be recorded, and
        // `get_attribution_data()` must NOT be called again.
        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(10));
        let (kmem_stats, kmem_stats_compression) = get_kernel_stats();
        let (stale_done_tx, mut stale_done_rx) = futures::channel::oneshot::channel();
        tx.unbounded_send(ManualMonitoringEvent::Completed(Box::new(ManualMonitoringData {
            timestamp: zx::BootInstant::from_nanos(1),
            attribution_data: get_raw_attribution_data(),
            kmem_stats: kmem_stats.clone(),
            kmem_stats_compression: kmem_stats_compression.clone(),
            done_tx: Some(stale_done_tx),
        })))
        .unwrap();

        let base_ts = zx::BootInstant::get() + zx::BootDuration::from_seconds(10);
        let newer_ts = base_ts + zx::BootDuration::from_seconds(1);
        let (superseded_done_tx, mut superseded_done_rx) = futures::channel::oneshot::channel();
        tx.unbounded_send(ManualMonitoringEvent::Completed(Box::new(ManualMonitoringData {
            timestamp: base_ts,
            attribution_data: get_raw_attribution_data(),
            kmem_stats: kmem_stats.clone(),
            kmem_stats_compression: kmem_stats_compression.clone(),
            done_tx: Some(superseded_done_tx),
        })))
        .unwrap();

        let (done_tx, mut done_rx) = futures::channel::oneshot::channel();
        tx.unbounded_send(ManualMonitoringEvent::Completed(Box::new(ManualMonitoringData {
            timestamp: newer_ts,
            attribution_data: get_raw_attribution_data(),
            kmem_stats,
            kmem_stats_compression,
            done_tx: Some(done_tx),
        })))
        .unwrap();

        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        // Manual reuse must not invoke `get_attribution_data()`.
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Completion acks for the processed and dropped snapshots have all resolved.
        assert!(matches!(done_rx.try_recv(), Ok(Some(()))));
        assert!(stale_done_rx.try_recv().is_err());
        assert!(superseded_done_rx.try_recv().is_err());

        // Next periodic collection is scheduled 5m after the manual completion at t = 10m:
        // at t = 14m it has not fired yet, and at t = 15m it fires.
        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(14));
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 2);

        exec.set_fake_time(start + fuchsia_async::MonotonicDuration::from_minutes(15));
        assert!(exec.run_until_stalled(&mut monitoring_fut).is_pending());
        assert_eq!(attribution_provider.call_count.load(std::sync::atomic::Ordering::SeqCst), 3);

        drop(monitoring_fut);

        // Verify Inspect tree has `buckets`, `current`, and 4 measurements ("0" from t=0,
        // "1" from t=9m, "2" from the coalesced manual snapshot at `newer_ts`, and "3" from t=15m).
        let mut assert_fut = std::pin::pin!(async {
            assert_data_tree!(inspector, root: {
                logger: contains {
                    buckets: vec![
                        "Undigested",
                        "Orphaned",
                        "Kernel",
                        "Free",
                        "[Addl]PagerTotal",
                        "[Addl]PagerNewest",
                        "[Addl]PagerOldest",
                        "[Addl]DiscardableLocked",
                        "[Addl]DiscardableUnlocked",
                        "[Addl]ZramCompressedBytes",
                        "[Addl]PopulatedAnonymousBytes",
                    ],
                    current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB").unwrap(),
                    measurements: {
                        "0": contains {
                            timestamp: NonZeroIntProperty,
                        },
                        "1": contains {
                            timestamp: NonZeroIntProperty,
                        },
                        "2": contains {
                            timestamp: newer_ts.into_nanos(),
                        },
                        "3": contains {
                            timestamp: NonZeroIntProperty,
                        },
                    },
                },
            });
        });
        assert!(exec.run_until_stalled(&mut assert_fut).is_ready());
        Ok(())
    }
}
