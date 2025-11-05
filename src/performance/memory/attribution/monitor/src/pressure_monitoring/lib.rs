// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Result};
use attribution_processing::digest::{BucketDefinition, Digest};
use attribution_processing::summary::MemorySummary;
use attribution_processing::{AttributionDataProvider, attribute_vmos};
use fpressure::WatcherRequest;
use fuchsia_async::{MonotonicDuration, MonotonicInstant, WakeupTime};
use fuchsia_inspect::{ArrayProperty, Node};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use futures::{StreamExt, TryFutureExt, TryStreamExt, select, try_join};
use humansize::{BINARY, FormatSizeOptions, format_size};
use memory_monitor2_config::Config;
use stalls::StallProvider;
use {fidl_fuchsia_kernel as fkernel, fidl_fuchsia_memorypressure as fpressure};

/// Subscribe to memory pressure events, and produce memory reports when appropriate.
pub async fn serve_to_inspect(
    memory_monitor2_config: &Config,
    attribution_data_service: &impl AttributionDataProvider,
    stall_provider: impl StallProvider,
    kernel_stats_proxy: fkernel::StatsProxy,
    memorypressure_proxy: fpressure::ProviderProxy,
    bucket_definitions: &[BucketDefinition],
    inspect_root: Node,
) -> Result<()> {
    let (watcher, pressure_stream) =
        fidl::endpoints::create_request_stream::<fpressure::WatcherMarker>();
    memorypressure_proxy.register_watcher(watcher)?;
    let mut buckets_list_node =
        BoundedListNode::new(inspect_root.create_child("measurements"), 100);
    let buckets_names = std::cell::OnceCell::new();
    let pressure_stream = pressure_stream.map_err(anyhow::Error::from);
    // Get the initial, baseline pressure level.
    let (request, mut pressure_stream) = pressure_stream.into_future().await;
    let mut current_level = {
        let WatcherRequest::OnLevelChanged { level, responder } = request.ok_or_else(|| {
            anyhow::Error::msg(
                "Unexpectedly exhausted pressure stream before receiving baseline pressure level",
            )
        })??;
        responder.send()?;
        level
    };
    let mut deadline = pressure_to_deadline(current_level, &memory_monitor2_config);
    let mut timer = Box::pin(deadline.into_timer());
    let mut _current;
    loop {
        // Wait for either a pressure change or the timer corresponding to the current level. In
        // either case, reset the timer.
        let () = select! {
            // When we receive a pressure change, update the current level and the schedule.
            pressure = pressure_stream.next() =>
                match pressure.ok_or_else(
                    || anyhow::Error::msg("Unexpectedly exhausted pressure stream"))?
                .with_context(|| "Failed to read memory pressure stream")? {
                    WatcherRequest::OnLevelChanged{level, responder} => {
                        responder.send().with_context(|| "Failed to send pressure stream response")?;
                        // Don't do anything if the pressure has not changed.
                        if level == current_level { continue; }
                        current_level = level;
                        deadline = if memory_monitor2_config.capture_on_pressure_change {
                            MonotonicInstant::now()
                        } else {
                            std::cmp::min(pressure_to_deadline(level, memory_monitor2_config),
                                          deadline)
                        };
                        timer.as_mut().reset(deadline);
                        continue; // Resume waiting
                    },
                },
            // If we reached the deadline, schedule the next capture and do the current one.
            _ = timer => {
                deadline = pressure_to_deadline(current_level, memory_monitor2_config);
                timer.as_mut().reset(deadline);
            },
        };

        let timestamp = zx::BootInstant::get();
        // Retrieve (concurrently) the data necessary to perform the aggregation.
        let (kmem_stats, kmem_stats_compression) = try_join!(
            kernel_stats_proxy.get_memory_stats().map_err(anyhow::Error::from),
            kernel_stats_proxy.get_memory_stats_compression().map_err(anyhow::Error::from)
        )
        .with_context(|| "Failed to get kernel memory stats")?;
        let Digest { buckets } = {
            let attribution_data = attribution_data_service.get_attribution_data()?;
            // Compute the aggregation.
            let digest = Digest::compute(
                &attribution_data,
                &kmem_stats,
                &kmem_stats_compression,
                bucket_definitions,
            )?;
            let summary = attribute_vmos(attribution_data).summary();
            _current = inspect_root
                .create_string("current", record_summary(summary, timestamp, &kmem_stats));
            digest
        };
        // Initialize the inspect property containing the buckets names, if necessary.
        let _ = buckets_names.get_or_init(|| {
            // Create inspect node to store buckets related information.
            let buckets_names = inspect_root.create_string_array("buckets", buckets.len());
            for (i, attribution_processing::digest::Bucket { name, .. }) in
                buckets.iter().enumerate()
            {
                buckets_names.set(i, name);
            }
            buckets_names
        });

        let stall_values = stall_provider
            .get_stall_info()
            .with_context(|| "Unable to retrieve stall information")?;

        // Add an entry for the current aggregation.
        buckets_list_node.add_entry(|n| {
            n.record_int("timestamp", timestamp.into_nanos());
            let ia = n.create_uint_array("bucket_sizes", buckets.len());
            for (i, b) in buckets.iter().enumerate() {
                ia.set(i, b.size as u64);
            }
            n.record(ia);
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
        });
    }
}

fn pressure_to_deadline(level: fpressure::Level, config: &Config) -> MonotonicInstant {
    MonotonicInstant::now()
        + MonotonicDuration::from_seconds(match level {
            fpressure::Level::Normal => config.normal_capture_delay_s,
            fpressure::Level::Warning => config.warning_capture_delay_s,
            fpressure::Level::Critical => config.critical_capture_delay_s,
        } as i64)
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
        Attribution, AttributionData, Principal, PrincipalDescription, PrincipalIdentifier,
        PrincipalType, Resource, ResourceEnumerator, ResourceReference, ResourcesVisitor, ZXName,
    };

    use diagnostics_assertions::{NonZeroIntProperty, assert_data_tree};
    use futures::FutureExt;
    use futures::task::Poll;
    use std::time::Duration;

    use fidl_fuchsia_memory_attribution_plugin as fplugin;

    pub struct FakeAttributionDataProvider {
        pub attribution_data: AttributionData,
    }

    impl AttributionDataProvider for FakeAttributionDataProvider {
        fn get_attribution_data(&self) -> Result<AttributionData, anyhow::Error> {
            Ok(AttributionData {
                principals_vec: self.attribution_data.principals_vec.clone(),
                resources_vec: self.attribution_data.resources_vec.clone(),
                resource_names: self.attribution_data.resource_names.clone(),
                attributions: self.attribution_data.attributions.clone(),
            })
        }
    }

    impl ResourceEnumerator for FakeAttributionDataProvider {
        fn for_each_resource(
            &self,
            _visitor: &mut impl ResourcesVisitor,
        ) -> Result<(), anyhow::Error> {
            unimplemented!();
        }
    }

    async fn serve_kernel_stats(
        mut request_stream: fkernel::StatsRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = request_stream.try_next().await? {
            match request {
                fkernel::StatsRequest::GetMemoryStats { responder } => {
                    responder
                        .send(&fkernel::MemoryStats {
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
                        })
                        .unwrap();
                }
                fkernel::StatsRequest::GetMemoryStatsExtended { responder: _ } => {
                    unimplemented!("Deprecated call, should not be used")
                }
                fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                    responder
                        .send(&fkernel::MemoryStatsCompression {
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
                            pages_decompressed_within_log_time: Some([
                                40, 41, 42, 43, 44, 45, 46, 47,
                            ]),
                            ..Default::default()
                        })
                        .unwrap();
                }
                fkernel::StatsRequest::GetCpuStats { responder: _ } => unimplemented!(),
                fkernel::StatsRequest::GetCpuLoad { duration: _, responder: _ } => unimplemented!(),
            }
        }
        Ok(())
    }

    fn get_attribution_data_provider() -> impl AttributionDataProvider {
        let attribution_data = AttributionData {
            principals_vec: vec![Principal {
                identifier: PrincipalIdentifier(1),
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
                source: PrincipalIdentifier(1),
                subject: PrincipalIdentifier(1),
                resources: vec![ResourceReference::KernelObject(10)],
            }],
        };
        FakeAttributionDataProvider { attribution_data }
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

    #[test]
    fn test_digest_service_capture_on_pressure_change_and_wait() -> anyhow::Result<()> {
        let mut exec = fuchsia_async::TestExecutor::new_with_fake_time();
        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fuchsia_async::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();
        let inspector = fuchsia_inspect::Inspector::default();

        let (pressure_provider, pressure_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpressure::ProviderMarker>();
        let digest_node = inspector.root().create_child("logger");
        let mut digest_service = std::pin::pin!(fuchsia_async::Task::spawn(async {
            let attribution_data_provider = get_attribution_data_provider();
            serve_to_inspect(
                &Config {
                    capture_on_pressure_change: true,
                    imminent_oom_capture_delay_s: 10,
                    critical_capture_delay_s: 10,
                    warning_capture_delay_s: 10,
                    normal_capture_delay_s: 10,
                },
                &attribution_data_provider,
                FakeStallProvider {},
                stats_provider,
                pressure_provider,
                Default::default(),
                digest_node,
            )
            .await
        }));
        // Expects digest_service to register a watcher, answers with
        // an initial pressure level, then returns the watcher for
        // further signaling. Panics if this whole transaction is not
        // immediately ready.
        let Poll::Ready(watcher) = exec
            .run_until_stalled(
                &mut pressure_request_stream
                    .then(|request| async {
                        let fpressure::ProviderRequest::RegisterWatcher { watcher, .. } =
                            request.expect("digest_service failed to register a watcher");
                        let watcher = watcher.into_proxy();
                        watcher.on_level_changed(fpressure::Level::Normal).await.expect(
                            "digest_service failed to acknowledge the initial pressure level",
                        );
                        watcher
                    })
                    .boxed()
                    .into_future(),
            )
            .map(|(watcher, _)| {
                watcher.ok_or_else(|| anyhow::Error::msg("failed to register watcher"))
            })?
        else {
            panic!("digest_service failed to register a watcher");
        };
        // Send a pressure signal, to trigger a capture.
        assert!(
            exec.run_until_stalled(&mut watcher.on_level_changed(fpressure::Level::Warning))?
                .is_ready()
        );
        // Fake the passage of time, so that digest_service may do another capture.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fuchsia_async::TestExecutor::advance_to(
                exec.now() + Duration::from_secs(10).into()
            )))
            .is_ready()
        );
        // Ensure that digest_service has an opportunity to react to the passage of time.
        let _ = exec.run_until_stalled(&mut digest_service)?;

        // This should resolve immediately because the inspect hierarchy has been populated by now.
        let Poll::Ready(output) = exec
            .run_until_stalled(&mut fuchsia_inspect::reader::read(&inspector).boxed())
            .map(|r| r.expect("got hierarchy"))
        else {
            panic!("Couldn't retrieve inspect output");
        };

        assert_data_tree!(@executor exec, output, root: {
            logger: {
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
                ],
                measurements: {
                    // Corresponds to the capture on pressure change
                    "0": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            6u64,    // Orphaned: vmo_bytes reported by the kernel but not covered by any bucket
                            31u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other = 31
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                    },
                    // Corresponds to the capture after the passage of time
                    "1": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            6u64,    // Orphaned: vmo_bytes reported by the kernel but not covered by any bucket
                            31u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other = 31
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                    },
                },
                current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB")?,
            },
        });
        Ok(())
    }

    #[test]
    fn test_digest_service_wait() -> anyhow::Result<()> {
        let mut exec = fuchsia_async::TestExecutor::new_with_fake_time();
        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fuchsia_async::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();
        let (pressure_provider, pressure_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpressure::ProviderMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let digest_node = inspector.root().create_child("logger");
        let mut digest_service = std::pin::pin!(fuchsia_async::Task::spawn(async {
            let attribution_data_provider = get_attribution_data_provider();
            serve_to_inspect(
                &Config {
                    capture_on_pressure_change: false,
                    imminent_oom_capture_delay_s: 10,
                    critical_capture_delay_s: 10,
                    warning_capture_delay_s: 10,
                    normal_capture_delay_s: 10,
                },
                &attribution_data_provider,
                FakeStallProvider {},
                stats_provider,
                pressure_provider,
                Default::default(),
                digest_node,
            )
            .await
        }));
        // digest_service registers a watcher; make sure we answer.  Also, make sure not to drop the
        // proxy nor the pressure stream; early termination would get reported to digest_service,
        // which then prematurely interrupts it, before the timers have a chance to run.
        let Poll::Ready((_watcher, _pressure_stream)) = exec
            .run_until_stalled(
                &mut std::pin::pin!(pressure_request_stream.then(|request| async {
                    let fpressure::ProviderRequest::RegisterWatcher { watcher, .. } =
                        request.map_err(anyhow::Error::from)?;
                    let watcher_proxy = watcher.into_proxy();
                    let _ = watcher_proxy.on_level_changed(fpressure::Level::Normal).await?;
                    Ok::<fpressure::WatcherProxy, anyhow::Error>(watcher_proxy)
                }))
                .into_future(),
            )
            .map(|(watcher, pressure_stream)| {
                (
                    watcher.ok_or_else(|| {
                        anyhow::Error::msg("Pressure stream unexpectedly exhausted")
                    }),
                    pressure_stream,
                )
            })
        else {
            panic!("Failed to register the watcher");
        };

        // Fake the passage of time, so that digest_service may do another capture.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fuchsia_async::TestExecutor::advance_to(
                exec.now() + Duration::from_secs(15).into(),
            )))
            .is_ready()
        );
        // Ensure that digest_service has an opportunity to react to the passage of time.
        assert!(exec.run_until_stalled(&mut digest_service).is_pending());
        // This should resolve immediately because the inspect hierarchy has been populated by now.
        let Poll::Ready(output) = exec
            .run_until_stalled(&mut fuchsia_inspect::reader::read(&inspector).boxed())
            .map(|r| r.expect("got hierarchy"))
        else {
            panic!("Couldn't retrieve inspect output");
        };

        assert_data_tree!(@executor exec, output, root: {
            logger: {
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
                ],
                measurements: {
                    // Corresponds to the capture after the passage of time
                    "0": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            6u64,    // Orphaned: vmo_bytes reported by the kernel but not covered by any bucket
                            31u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other = 31
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                    },
                },
                current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB")?,
            },
        });
        Ok(())
    }

    #[test]
    fn test_digest_service_new_pressure_does_not_postpone() -> anyhow::Result<()> {
        // See https://fxbug.dev/417722087 for context.
        let mut exec = fuchsia_async::TestExecutor::new_with_fake_time();
        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fuchsia_async::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();
        let (pressure_provider, pressure_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpressure::ProviderMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let digest_node = inspector.root().create_child("logger");
        let mut digest_service = std::pin::pin!(fuchsia_async::Task::spawn(async {
            let attribution_data_provider = get_attribution_data_provider();
            serve_to_inspect(
                &Config {
                    capture_on_pressure_change: false,
                    imminent_oom_capture_delay_s: 10,
                    critical_capture_delay_s: 10,
                    warning_capture_delay_s: 100,
                    normal_capture_delay_s: 100,
                },
                &attribution_data_provider,
                FakeStallProvider {},
                stats_provider,
                pressure_provider,
                Default::default(),
                digest_node,
            )
            .await
        }));
        // digest_service registers a watcher; make sure we answer.  Also, make sure not to drop the
        // proxy nor the pressure stream; early termination would get reported to digest_service,
        // which then prematurely interrupts it, before the timers have a chance to run.
        let Poll::Ready((watcher, _pressure_stream)) = exec
            .run_until_stalled(
                &mut std::pin::pin!(pressure_request_stream.then(|request| async {
                    let fpressure::ProviderRequest::RegisterWatcher { watcher, .. } =
                        request.map_err(anyhow::Error::from)?;
                    let watcher_proxy = watcher.into_proxy();
                    watcher_proxy.on_level_changed(fpressure::Level::Critical).await?;
                    Ok::<fpressure::WatcherProxy, anyhow::Error>(watcher_proxy)
                }))
                .into_future(),
            )
            .map(|(watcher, pressure_stream)| {
                (
                    watcher.ok_or_else(|| {
                        anyhow::Error::msg("Pressure stream unexpectedly exhausted")
                    }),
                    pressure_stream,
                )
            })
        else {
            panic!("Failed to register the watcher");
        };

        // Keep the watcher alive; otherwise the pressure stream would get dropped, which would
        // cause digest_service to early error out.
        let watcher = watcher??;
        // Fake a pressure change, so that the frequency of captures drops down.
        assert!(
            exec.run_until_stalled(&mut watcher.on_level_changed(fpressure::Level::Normal))?
                .is_ready()
        );
        // Fake the passage of time, so that digest_service may do a capture; wait long enough that
        // the first two captures at Critical frequency would have already happened, but short
        // enough that no capture happens at the Normal frequency.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fuchsia_async::TestExecutor::advance_to(
                exec.now() + Duration::from_secs(25).into()
            )))
            .is_ready()
        );
        // Ensure that digest_service has an opportunity to react to the passage of time.
        assert!(exec.run_until_stalled(&mut digest_service)?.is_pending());
        // This should resolve immediately because the inspect hierarchy has been populated by now.
        let Poll::Ready(output) = exec
            .run_until_stalled(&mut fuchsia_inspect::reader::read(&inspector).boxed())
            .map(|r| r.expect("got hierarchy"))
        else {
            panic!("Couldn't retrieve inspect output");
        };

        assert_data_tree!(@executor exec, output, root: {
            logger: {
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
                ],
                measurements: {
                    // Corresponds to the capture after the passage of time
                    "0": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            6u64,    // Orphaned: vmo_bytes reported by the kernel but not covered by any bucket
                            31u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other = 31
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                    },
                },
                current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB")?,             },
        });
        Ok(())
    }

    #[test]
    fn test_digest_service_no_capture_on_pressure_change() -> anyhow::Result<()> {
        let mut exec = fuchsia_async::TestExecutor::new();
        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fuchsia_async::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();

        let inspector = fuchsia_inspect::Inspector::default();
        let (pressure_provider, pressure_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpressure::ProviderMarker>();
        let mut serve_pressure_stream = pressure_request_stream
            .then(|request| async {
                let fpressure::ProviderRequest::RegisterWatcher { watcher, .. } =
                    request.map_err(anyhow::Error::from)?;
                let watcher_proxy = watcher.into_proxy();
                let _ = watcher_proxy.on_level_changed(fpressure::Level::Normal).await?;
                Ok::<fpressure::WatcherProxy, anyhow::Error>(watcher_proxy)
            })
            .boxed();
        let digest_node = inspector.root().create_child("logger");
        let mut digest_service = std::pin::pin!(fuchsia_async::Task::spawn(async {
            let attribution_data_provider = get_attribution_data_provider();
            serve_to_inspect(
                &Config {
                    capture_on_pressure_change: false,
                    imminent_oom_capture_delay_s: 10,
                    critical_capture_delay_s: 10,
                    warning_capture_delay_s: 10,
                    normal_capture_delay_s: 10,
                },
                &attribution_data_provider,
                FakeStallProvider {},
                stats_provider,
                pressure_provider,
                Default::default(),
                digest_node,
            )
            .await
        }));
        let watcher =
            exec.run_singlethreaded(serve_pressure_stream.next()).transpose()?.expect("watcher");
        let _ = exec.run_singlethreaded(watcher.on_level_changed(fpressure::Level::Warning))?;
        let _ = exec.run_until_stalled(&mut digest_service);
        let output = exec
            .run_singlethreaded(fuchsia_inspect::reader::read(&inspector))
            .expect("got hierarchy");

        assert_data_tree!(@executor exec, output, root: {
            logger: {
                measurements: {},
            },
        });
        Ok(())
    }

    #[test]
    fn test_digest_service_capture_on_pressure_change() -> anyhow::Result<()> {
        let mut exec = fuchsia_async::TestExecutor::new();
        let (stats_provider, stats_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>();

        fuchsia_async::Task::spawn(async move {
            serve_kernel_stats(stats_request_stream).await.unwrap();
        })
        .detach();

        let inspector = fuchsia_inspect::Inspector::default();
        let (pressure_provider, pressure_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpressure::ProviderMarker>();
        let mut serve_pressure_stream = pressure_request_stream
            .then(|request| async {
                let fpressure::ProviderRequest::RegisterWatcher { watcher, .. } =
                    request.map_err(anyhow::Error::from)?;
                let watcher_proxy = watcher.into_proxy();
                let _ = watcher_proxy.on_level_changed(fpressure::Level::Normal).await?;
                Ok::<fpressure::WatcherProxy, anyhow::Error>(watcher_proxy)
            })
            .boxed();
        let digest_node = inspector.root().create_child("logger");
        let mut digest_service = std::pin::pin!(fuchsia_async::Task::spawn(async {
            let attribution_data_provider = get_attribution_data_provider();
            serve_to_inspect(
                &Config {
                    capture_on_pressure_change: true,
                    imminent_oom_capture_delay_s: 10,
                    critical_capture_delay_s: 10,
                    warning_capture_delay_s: 10,
                    normal_capture_delay_s: 10,
                },
                &attribution_data_provider,
                FakeStallProvider {},
                stats_provider,
                pressure_provider,
                Default::default(),
                digest_node,
            )
            .await
        }));
        let watcher =
            exec.run_singlethreaded(serve_pressure_stream.next()).transpose()?.expect("watcher");
        let _ = exec.run_singlethreaded(watcher.on_level_changed(fpressure::Level::Warning))?;
        let _ = exec.run_until_stalled(&mut digest_service);
        let output = exec
            .run_singlethreaded(fuchsia_inspect::reader::read(&inspector))
            .expect("got hierarchy");

        assert_data_tree!(@executor exec, output, root: {
            logger: {
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
                ],
                measurements: {
                    "0": {
                        timestamp: NonZeroIntProperty,
                        bucket_sizes: vec![
                            1024u64, // Undigested: matches the single unmatched VMO
                            6u64,    // Orphaned: vmo_bytes reported by the kernel but not covered by any bucket
                            31u64,   // Kernel: 3 wired + 4 heap + 7 mmu + 8 IPC + 9 other = 31
                            2u64,    // Free
                            14u64,   // [Addl]PagerTotal
                            15u64,   // [Addl]PagerNewest
                            16u64,   // [Addl]PagerOldest
                            18u64,   // [Addl]DiscardableLocked
                            19u64,   // [Addl]DiscardableUnlocked
                            21u64,   // [Addl]ZramCompressedBytes
                        ],
                        stalls: {
                            some_ms: 10u64,
                            full_ms: 20u64,
                        },
                    },
                },
                current: regex::Regex::new(r"^Time: \d+ VMO: 6B Free: 2B\nprincipal: 2KiB; resource 2KiB 2KiB 2KiB")?,
            },
        });
        Ok(())
    }
}
