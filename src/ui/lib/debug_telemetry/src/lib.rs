// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Best-effort system CPU and memory telemetry collection for UI diagnostic overlays.
//!
//! [`DiagnosticsCollector`] aggregates telemetry concurrently from:
//! - [`CpuCollector`] ([`CpuMetrics`]): average load, per-core load, and CPU power draw via
//!   `fuchsia.kernel.Stats::GetCpuStats`.
//! - [`MemoryCollector`] ([`MemoryMetrics`]): physical memory, VMO allocation, and ZRAM
//!   compression via `fuchsia.kernel.Stats`.
//!
//! # Examples
//!
//! ```no_run
//! use debug_telemetry::{CachePolicy, DiagnosticsCollector};
//!
//! async fn sample_once() {
//!     let mut collector = DiagnosticsCollector::from_namespace();
//!     collector.prime_baseline().await;
//!     let snapshot = collector.snapshot(CachePolicy::ForceRefresh).await;
//!     let _ = snapshot.cpu.avg_load_pct;
//! }
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod cpu;
pub mod memory;

pub use cpu::{CpuCollector, CpuMetrics};
pub use memory::{MemoryCollector, MemoryMetrics};

use fidl_fuchsia_kernel as fkernel;
use fuchsia_component::client::connect_to_protocol;
use std::future::Future;

/// Controls whether [`DiagnosticsCollector::snapshot`] reuses an existing cached sample or
/// performs a fresh collection pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Returns the most recently collected [`DiagnosticSnapshot`] if one exists, or collects a
    /// fresh snapshot when the cache is cold.
    UseCached,
    /// Collects a fresh [`DiagnosticSnapshot`] and updates the cache.
    ForceRefresh,
}

/// Point-in-time snapshot of system CPU and memory diagnostic metrics.
#[derive(Debug, PartialEq, Default)]
#[non_exhaustive]
pub struct DiagnosticSnapshot {
    /// Monotonically increasing counter (`>= 1` once collected) identifying each fresh sample.
    pub generation: u64,
    /// Average and per-core CPU load and package power consumption.
    pub cpu: CpuMetrics,
    /// Physical memory, VMO allocation, and ZRAM compression statistics.
    pub memory: MemoryMetrics,
}

impl Clone for DiagnosticSnapshot {
    fn clone(&self) -> Self {
        Self { generation: self.generation, cpu: self.cpu.clone(), memory: self.memory }
    }

    fn clone_from(&mut self, source: &Self) {
        self.generation = source.generation;
        self.cpu.clone_from(&source.cpu);
        self.memory = source.memory;
    }
}

/// Builder for selectively enabling [`DiagnosticsCollector`] metric sources from the incoming
/// component namespace.
///
/// Disabled collectors are initialized via `disconnected()`, avoiding unnecessary namespace
/// connections or background polling for capabilities a component does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticsCollectorBuilder {
    cpu: bool,
    memory: bool,
}

impl DiagnosticsCollectorBuilder {
    /// Creates a builder with all metric collectors initially disabled.
    pub const fn new() -> Self {
        Self { cpu: false, memory: false }
    }

    /// Enables [`CpuCollector`] (`fuchsia.kernel.Stats`).
    pub const fn with_cpu(mut self) -> Self {
        self.cpu = true;
        self
    }

    /// Enables [`MemoryCollector`] (`fuchsia.kernel.Stats`).
    pub const fn with_memory(mut self) -> Self {
        self.memory = true;
        self
    }

    /// Connects only the enabled metric collectors from the incoming component namespace, sharing
    /// a single `fuchsia.kernel.Stats` connection when both CPU and memory are enabled.
    pub fn build_from_namespace(self) -> DiagnosticsCollector {
        self.build_with_kernel_connect(|| connect_to_protocol::<fkernel::StatsMarker>().ok())
    }

    fn build_with_kernel_connect(
        self,
        connect_kernel: impl FnOnce() -> Option<fkernel::StatsProxy>,
    ) -> DiagnosticsCollector {
        let kernel_stats = if self.cpu || self.memory { connect_kernel() } else { None };
        let cpu = if self.cpu {
            CpuCollector::with_proxy(kernel_stats.clone())
        } else {
            CpuCollector::disconnected()
        };
        let memory = if self.memory {
            MemoryCollector::with_proxy(kernel_stats)
        } else {
            MemoryCollector::disconnected()
        };
        let mut collector = DiagnosticsCollector::with_collectors(cpu, memory);
        collector.shares_kernel_stats = self.cpu && self.memory;
        collector
    }
}

/// Collects real-time telemetry concurrently across [`CpuCollector`] and [`MemoryCollector`],
/// reusing internal snapshot storage across refreshes.
#[derive(Debug)]
pub struct DiagnosticsCollector {
    cpu: CpuCollector,
    memory: MemoryCollector,
    snapshot: DiagnosticSnapshot,
    shares_kernel_stats: bool,
}

impl DiagnosticsCollector {
    /// Returns a [`DiagnosticsCollectorBuilder`] with all collectors initially disabled so callers
    /// can opt into specific metric sources.
    pub const fn builder() -> DiagnosticsCollectorBuilder {
        DiagnosticsCollectorBuilder::new()
    }

    /// Connects to `fuchsia.kernel.Stats` in the incoming namespace if available, sharing a single
    /// connection between [`CpuCollector`] and [`MemoryCollector`].
    pub fn from_namespace() -> Self {
        Self::builder().with_cpu().with_memory().build_from_namespace()
    }

    /// Creates an inert `DiagnosticsCollector` with all protocol connections disabled.
    pub fn disconnected() -> Self {
        Self::with_collectors(CpuCollector::disconnected(), MemoryCollector::disconnected())
    }

    /// Creates a `DiagnosticsCollector` from explicit metric collectors.
    pub fn with_collectors(cpu: CpuCollector, memory: MemoryCollector) -> Self {
        Self { cpu, memory, snapshot: DiagnosticSnapshot::default(), shares_kernel_stats: false }
    }

    fn sync_shared_kernel_stats(&mut self) {
        if self.shares_kernel_stats && (!self.cpu.is_connected() || !self.memory.is_connected()) {
            self.cpu.disconnect();
            self.memory.disconnect();
        }
    }

    /// Returns the most recently cached [`DiagnosticSnapshot`], if any.
    pub fn cached_snapshot(&self) -> Option<&DiagnosticSnapshot> {
        (self.snapshot.generation != 0).then_some(&self.snapshot)
    }

    /// Captures baseline CPU idle-time and energy counters so the next [`collect`](Self::collect)
    /// call can immediately compute window deltas.
    pub async fn prime_baseline(&mut self) {
        self.cpu.prime_baseline().await;
        self.sync_shared_kernel_stats();
    }

    /// Captures baseline CPU counters stamped at `monotonic`.
    pub async fn prime_baseline_at(&mut self, monotonic: zx::MonotonicInstant) {
        self.cpu.prime_baseline_at(monotonic).await;
        self.sync_shared_kernel_stats();
    }

    /// Returns a [`DiagnosticSnapshot`] according to `policy`, reusing the cached snapshot when
    /// `policy` is [`CachePolicy::UseCached`] and a sample has already been collected.
    pub fn snapshot(&mut self, policy: CachePolicy) -> impl Future<Output = &DiagnosticSnapshot> {
        if policy == CachePolicy::UseCached && self.snapshot.generation != 0 {
            futures::future::Either::Left(futures::future::ready(&self.snapshot))
        } else {
            futures::future::Either::Right(self.collect())
        }
    }

    /// Collects a fresh [`DiagnosticSnapshot`] using current Zircon timestamps, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect(&mut self) -> &DiagnosticSnapshot {
        let (_, memory) =
            futures::join!(self.cpu.collect_into(&mut self.snapshot.cpu), self.memory.collect());
        self.sync_shared_kernel_stats();
        self.finish_snapshot(memory)
    }

    /// Collects a fresh [`DiagnosticSnapshot`] stamped at `monotonic` and `boot`, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect_at(
        &mut self,
        monotonic: zx::MonotonicInstant,
        _boot: zx::BootInstant,
    ) -> &DiagnosticSnapshot {
        let (_, memory) = futures::join!(
            self.cpu.collect_at_into(monotonic, &mut self.snapshot.cpu),
            self.memory.collect(),
        );
        self.sync_shared_kernel_stats();
        self.finish_snapshot(memory)
    }

    fn finish_snapshot(&mut self, memory: MemoryMetrics) -> &DiagnosticSnapshot {
        self.snapshot.generation = self.snapshot.generation.wrapping_add(1).max(1);
        self.snapshot.memory = memory;
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_kernel as fkernel;
    use fuchsia_async as fasync;
    use futures::StreamExt;

    const MB: u64 = 1024 * 1024;

    fn mono(ns: i64) -> zx::MonotonicInstant {
        zx::MonotonicInstant::from_nanos(ns)
    }

    fn boot(ns: i64) -> zx::BootInstant {
        zx::BootInstant::from_nanos(ns)
    }

    #[fuchsia::test]
    async fn test_cpu_and_memory_collectors_share_kernel_stats_proxy() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let mut cpu_call_index = 0_u32;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fkernel::StatsRequest::GetCpuStats { responder } => {
                        cpu_call_index += 1;
                        let stats = if cpu_call_index == 1 {
                            fkernel::CpuStats {
                                actual_num_cpus: 2,
                                per_cpu_stats: Some(vec![
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(1_000_000_000),
                                        active_energy_consumption_nj: Some(2_000_000_000),
                                        idle_energy_consumption_nj: Some(500_000_000),
                                        ..Default::default()
                                    },
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(2_000_000_000),
                                        active_energy_consumption_nj: Some(1_000_000_000),
                                        idle_energy_consumption_nj: Some(500_000_000),
                                        ..Default::default()
                                    },
                                ]),
                            }
                        } else {
                            fkernel::CpuStats {
                                actual_num_cpus: 2,
                                per_cpu_stats: Some(vec![
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(1_750_000_000),
                                        active_energy_consumption_nj: Some(2_300_000_000),
                                        idle_energy_consumption_nj: Some(600_000_000),
                                        ..Default::default()
                                    },
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(2_250_000_000),
                                        active_energy_consumption_nj: Some(1_500_000_000),
                                        idle_energy_consumption_nj: Some(600_000_000),
                                        ..Default::default()
                                    },
                                ]),
                            }
                        };
                        let _ = responder.send(&stats);
                    }
                    fkernel::StatsRequest::GetMemoryStats { responder } => {
                        let mem = fkernel::MemoryStats {
                            total_bytes: Some(4096 * MB),
                            free_bytes: Some(1296 * MB),
                            vmo_bytes: Some(2100 * MB),
                            ..Default::default()
                        };
                        let _ = responder.send(&mem);
                    }
                    fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                        let comp = fkernel::MemoryStatsCompression {
                            compressed_storage_bytes: Some(128 * MB),
                            uncompressed_storage_bytes: Some(384 * MB),
                            ..Default::default()
                        };
                        let _ = responder.send(&comp);
                    }
                    _ => {}
                }
            }
        });

        let mut cpu_collector = CpuCollector::with_proxy(Some(proxy.clone()));
        let mut memory_collector = MemoryCollector::with_proxy(Some(proxy));

        cpu_collector.prime_baseline_at(mono(10_000_000_000)).await;
        let (cpu, memory) = futures::join!(
            cpu_collector.collect_at(mono(11_000_000_000)),
            memory_collector.collect(),
        );

        assert_eq!(cpu.per_core_load_pct, vec![25, 75]);
        assert!((cpu.avg_load_pct.expect("avg cpu load") - 50.0).abs() < 1e-4);
        assert_eq!(cpu.power_mw, Some(1000));

        assert_eq!(memory.total_mb, Some(4096));
        assert_eq!(memory.free_mb, Some(1296));
        assert_eq!(memory.used_mb, Some(2800));
        assert_eq!(memory.vmo_mb, Some(2100));
        assert_eq!(memory.zram_compressed_mb, Some(128));
        assert_eq!(memory.zram_uncompressed_mb, Some(384));
    }

    #[fuchsia::test]
    async fn test_cpu_collector_offline_cores_zero_delta_and_energy_gating() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let samples = [
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_000_000_000),
                        active_energy_consumption_nj: Some(0),
                        idle_energy_consumption_nj: Some(0),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_500_000_000),
                        active_energy_consumption_nj: Some(0),
                        idle_energy_consumption_nj: Some(0),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(2_000_000_000),
                        active_energy_consumption_nj: Some(500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(3_000_000_000),
                        active_energy_consumption_nj: Some(500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
            ];
            let mut idx = 0_usize;
            while let Some(Ok(request)) = stream.next().await {
                if let fkernel::StatsRequest::GetCpuStats { responder } = request {
                    let per_cpu_stats = Some(samples[idx.min(samples.len() - 1)].clone());
                    idx += 1;
                    let _ =
                        responder.send(&fkernel::CpuStats { actual_num_cpus: 2, per_cpu_stats });
                }
            }
        });

        let mut collector = CpuCollector::with_proxy(Some(proxy));
        let unprimed = collector.collect_at(mono(1_000_000_000)).await;
        assert_eq!(unprimed.avg_load_pct, None);
        assert!(unprimed.per_core_load_pct.is_empty());
        assert_eq!(unprimed.power_mw, None);

        let sample = collector.collect_at(mono(2_000_000_000)).await;
        assert_eq!(sample.per_core_load_pct, vec![50, 0]);
        assert!((sample.avg_load_pct.expect("online avg") - 50.0).abs() < 1e-4);
        assert_eq!(sample.power_mw, None);

        let zero_delta = collector.collect_at(mono(2_000_000_000)).await;
        assert_eq!(zero_delta, sample);

        let with_energy = collector.collect_at(mono(3_000_000_000)).await;
        assert_eq!(with_energy.power_mw, Some(500));

        let quiet_energy = collector.collect_at(mono(4_000_000_000)).await;
        assert_eq!(quiet_energy.power_mw, Some(0));
    }

    #[fuchsia::test]
    async fn test_memory_collector_partial_stats_and_closed_proxy() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let mut call = 0_u32;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fkernel::StatsRequest::GetMemoryStats { responder } => {
                        call += 1;
                        let mem = if call == 1 {
                            fkernel::MemoryStats {
                                total_bytes: Some(2048 * MB),
                                free_bytes: None,
                                ..Default::default()
                            }
                        } else {
                            fkernel::MemoryStats {
                                total_bytes: Some(1024 * MB),
                                free_bytes: Some(2048 * MB),
                                ..Default::default()
                            }
                        };
                        let _ = responder.send(&mem);
                    }
                    fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                        let _ = responder.send(&fkernel::MemoryStatsCompression::default());
                        if call >= 2 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        let mut collector = MemoryCollector::with_proxy(Some(proxy));
        let partial = collector.collect().await;
        assert_eq!(partial.total_mb, Some(2048));
        assert_eq!(partial.free_mb, None);
        assert_eq!(partial.used_mb, None);

        let saturated = collector.collect().await;
        assert_eq!(saturated.used_mb, Some(0));

        let closed = collector.collect().await;
        assert_eq!(closed, MemoryMetrics::default());
    }

    #[fuchsia::test]
    async fn test_diagnostics_collector_end_to_end_caching_and_clone_from() {
        let mut collector = DiagnosticsCollector::disconnected();
        assert!(collector.cached_snapshot().is_none());

        let cold = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cold.generation, 1);
        assert_eq!(collector.cached_snapshot().map(|s| s.generation), Some(1));

        let cached = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cached.generation, 1);

        let refreshed = collector.snapshot(CachePolicy::ForceRefresh).await.clone();
        assert_eq!(refreshed.generation, 2);

        let mut reused = DiagnosticSnapshot::default();
        reused.cpu.per_core_load_pct = Vec::with_capacity(16);
        let initial_ptr = reused.cpu.per_core_load_pct.as_ptr();
        let mut source = refreshed.clone();
        source.cpu.per_core_load_pct = vec![10, 20, 30, 40];
        reused.clone_from(&source);
        assert_eq!(reused.cpu.per_core_load_pct.as_ptr(), initial_ptr);
        assert_eq!(reused, source);

        let timed = collector.collect_at(mono(600_000_000_000), boot(1_000_000_000_000)).await;
        assert_eq!(timed.generation, 3);
    }

    #[fuchsia::test]
    async fn test_diagnostics_collector_builder_selective_sources() {
        let empty = DiagnosticsCollector::builder().build_with_kernel_connect(|| {
            panic!("kernel stats should not connect when cpu and memory are disabled");
        });
        assert!(!empty.cpu.is_connected());
        assert!(!empty.memory.is_connected());
        assert!(!empty.shares_kernel_stats);

        let (proxy, _stream) = create_proxy_and_stream::<fkernel::StatsMarker>();
        let cpu_only =
            DiagnosticsCollector::builder().with_cpu().build_with_kernel_connect(|| Some(proxy));
        assert!(cpu_only.cpu.is_connected());
        assert!(!cpu_only.memory.is_connected());
        assert!(!cpu_only.shares_kernel_stats);
    }
}
