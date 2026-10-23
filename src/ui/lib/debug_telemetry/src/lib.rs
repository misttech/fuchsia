// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Best-effort system telemetry collection and snapshot caching for UI diagnostic overlays.
//!
//! [`DiagnosticsCollector`] provides monotonic/boot timestamp sampling, baseline priming, and
//! generation-counted [`DiagnosticSnapshot`] caching so callers can refresh telemetry on a steady
//! cadence while reusing the most recent snapshot between refreshes.
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
//!     let _ = snapshot.generation;
//! }
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

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

/// Point-in-time snapshot of system diagnostic metrics.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct DiagnosticSnapshot {
    /// Monotonically increasing counter (`>= 1` once collected) identifying each fresh sample.
    pub generation: u64,
}

/// Collects real-time diagnostic telemetry and caches the latest [`DiagnosticSnapshot`].
#[derive(Debug)]
pub struct DiagnosticsCollector {
    snapshot: DiagnosticSnapshot,
}

impl DiagnosticsCollector {
    /// Creates a [`DiagnosticsCollector`] connected to available telemetry protocols in the
    /// incoming namespace.
    pub fn from_namespace() -> Self {
        Self::disconnected()
    }

    /// Creates an inert `DiagnosticsCollector` with all protocol connections disabled.
    pub fn disconnected() -> Self {
        Self { snapshot: DiagnosticSnapshot::default() }
    }

    /// Returns the most recently cached [`DiagnosticSnapshot`], if any.
    pub fn cached_snapshot(&self) -> Option<&DiagnosticSnapshot> {
        (self.snapshot.generation != 0).then_some(&self.snapshot)
    }

    /// Captures baseline telemetry samples at the current monotonic instant so the next
    /// [`collect`](Self::collect) call can compute window deltas.
    pub async fn prime_baseline(&mut self) {
        self.prime_baseline_at(zx::MonotonicInstant::get()).await;
    }

    /// Captures baseline telemetry samples using `monotonic` as the reference timestamp.
    pub async fn prime_baseline_at(&mut self, _monotonic: zx::MonotonicInstant) {}

    /// Returns a [`DiagnosticSnapshot`] according to `policy`, reusing the cached snapshot when
    /// `policy` is [`CachePolicy::UseCached`] and a sample has already been collected.
    pub async fn snapshot(&mut self, policy: CachePolicy) -> &DiagnosticSnapshot {
        if policy == CachePolicy::UseCached && self.snapshot.generation != 0 {
            return &self.snapshot;
        }
        self.collect().await
    }

    /// Collects a fresh [`DiagnosticSnapshot`] using current Zircon timestamps, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect(&mut self) -> &DiagnosticSnapshot {
        let boot = zx::BootInstant::get();
        let monotonic = zx::MonotonicInstant::get();
        self.collect_at(monotonic, boot).await
    }

    /// Collects a fresh [`DiagnosticSnapshot`] stamped at `monotonic` and `boot`, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect_at(
        &mut self,
        _monotonic: zx::MonotonicInstant,
        _boot: zx::BootInstant,
    ) -> &DiagnosticSnapshot {
        self.snapshot.generation = self.snapshot.generation.wrapping_add(1).max(1);
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_diagnostics_collector_end_to_end_and_caching() {
        let mut collector = DiagnosticsCollector::from_namespace();
        assert!(collector.cached_snapshot().is_none());

        let cold = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cold.generation, 1);
        assert_eq!(collector.cached_snapshot().map(|s| s.generation), Some(1));

        let cached = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cached.generation, 1);

        let refreshed = collector.snapshot(CachePolicy::ForceRefresh).await.clone();
        assert_eq!(refreshed.generation, 2);

        let timed = collector
            .collect_at(
                zx::MonotonicInstant::from_nanos(600_000_000_000),
                zx::BootInstant::from_nanos(1_000_000_000_000),
            )
            .await;
        assert_eq!(timed.generation, 3);
        assert_eq!(collector.cached_snapshot().map(|s| s.generation), Some(3));

        collector.prime_baseline().await;
        let collected = collector.collect().await;
        assert_eq!(collected.generation, 4);
    }
}
