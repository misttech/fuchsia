// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physical memory, VMO memory, and ZRAM compression telemetry via `fuchsia.kernel.Stats`.

use fidl_fuchsia_kernel as fkernel;
use fuchsia_component::client::connect_to_protocol;

const BYTES_PER_MEGABYTE: u64 = 1024 * 1024;

/// Snapshot of physical memory, VMO allocation, and ZRAM compression metrics in megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MemoryMetrics {
    /// Physical memory in use (`(total_bytes - free_bytes) / MiB`).
    pub used_mb: Option<u64>,
    /// Total physical memory in MiB.
    pub total_mb: Option<u64>,
    /// Free physical memory in MiB.
    pub free_mb: Option<u64>,
    /// Total VMO-backed memory in MiB.
    pub vmo_mb: Option<u64>,
    /// Compressed ZRAM storage size in MiB.
    pub zram_compressed_mb: Option<u64>,
    /// Uncompressed ZRAM payload size in MiB.
    pub zram_uncompressed_mb: Option<u64>,
}

/// Queries physical memory, VMO, and ZRAM compression statistics from `fuchsia.kernel.Stats`.
#[derive(Debug)]
pub struct MemoryCollector {
    proxy: Option<fkernel::StatsProxy>,
}

impl MemoryCollector {
    /// Connects to `fuchsia.kernel.Stats` in the incoming namespace if available.
    pub fn from_namespace() -> Self {
        Self::with_proxy(connect_to_protocol::<fkernel::StatsMarker>().ok())
    }

    /// Creates an inert collector with no `fuchsia.kernel.Stats` connection.
    pub fn disconnected() -> Self {
        Self::with_proxy(None)
    }

    /// Creates a collector backed by an explicit `StatsProxy` (or `None` when unavailable).
    pub fn with_proxy(proxy: Option<fkernel::StatsProxy>) -> Self {
        Self { proxy }
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.proxy.is_some()
    }

    pub(crate) fn disconnect(&mut self) {
        self.proxy = None;
    }

    /// Queries memory and ZRAM compression statistics concurrently.
    pub async fn collect(&mut self) -> MemoryMetrics {
        let mut metrics = MemoryMetrics::default();
        let Some(proxy) = self.proxy.as_ref() else {
            return metrics;
        };

        let (memory_res, compression_res) =
            futures::join!(proxy.get_memory_stats(), proxy.get_memory_stats_compression());

        let mut channel_closed = false;
        match memory_res {
            Ok(memory_stats) => {
                metrics.total_mb = memory_stats.total_bytes.map(|bytes| bytes / BYTES_PER_MEGABYTE);
                metrics.free_mb = memory_stats.free_bytes.map(|bytes| bytes / BYTES_PER_MEGABYTE);
                if let (Some(total_bytes), Some(free_bytes)) =
                    (memory_stats.total_bytes, memory_stats.free_bytes)
                {
                    metrics.used_mb =
                        Some(total_bytes.saturating_sub(free_bytes) / BYTES_PER_MEGABYTE);
                }
                metrics.vmo_mb = memory_stats.vmo_bytes.map(|bytes| bytes / BYTES_PER_MEGABYTE);
            }
            Err(err) => channel_closed |= err.is_closed(),
        }

        match compression_res {
            Ok(compression_stats) => {
                metrics.zram_compressed_mb = compression_stats
                    .compressed_storage_bytes
                    .map(|bytes| bytes / BYTES_PER_MEGABYTE);
                metrics.zram_uncompressed_mb = compression_stats
                    .uncompressed_storage_bytes
                    .map(|bytes| bytes / BYTES_PER_MEGABYTE);
            }
            Err(err) => channel_closed |= err.is_closed(),
        }

        if channel_closed {
            self.proxy = None;
        }

        metrics
    }
}
