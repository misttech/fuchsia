// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-core CPU load and CPU energy/power telemetry via `fuchsia.kernel.Stats`.

use fidl_fuchsia_kernel as fkernel;
use fuchsia_component::client::connect_to_protocol;

const MILLIWATTS_PER_WATT: f64 = 1000.0;
const FULL_PERCENT: f64 = 100.0;
/// Matches Zircon's `ZX_INFO_CPU_STATS_FLAG_ONLINE` (`1 << 0`).
const CPU_STATS_FLAG_ONLINE: u32 = 1 << 0;

/// Snapshot of average CPU load, per-core CPU load, and CPU power consumption over the latest
/// sample window.
#[derive(Debug, PartialEq, Default)]
#[non_exhaustive]
pub struct CpuMetrics {
    /// Mean active CPU utilization (`0.0..=100.0`) across online cores over the sample window.
    pub avg_load_pct: Option<f32>,
    /// Rounded active CPU utilization (`0..=100`) for each core reported by the kernel (`0` for
    /// offline cores).
    pub per_core_load_pct: Vec<u8>,
    /// Total CPU package power consumption in milliwatts (`>= 0`) over the sample window when the
    /// platform exposes non-zero CPU energy counters.
    pub power_mw: Option<u32>,
}

impl Clone for CpuMetrics {
    fn clone(&self) -> Self {
        Self {
            avg_load_pct: self.avg_load_pct,
            per_core_load_pct: self.per_core_load_pct.clone(),
            power_mw: self.power_mw,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.avg_load_pct = source.avg_load_pct;
        self.per_core_load_pct.clone_from(&source.per_core_load_pct);
        self.power_mw = source.power_mw;
    }
}

impl CpuMetrics {
    fn clear(&mut self) {
        self.avg_load_pct = None;
        self.per_core_load_pct.clear();
        self.power_mw = None;
    }
}

#[derive(Debug, Clone, Copy)]
struct CoreSample {
    idle_time_ns: i64,
    energy_nj: u64,
}

impl CoreSample {
    fn from_fidl(stats: &fkernel::PerCpuStats) -> Self {
        let energy_nj = stats
            .active_energy_consumption_nj
            .unwrap_or(0)
            .saturating_add(stats.idle_energy_consumption_nj.unwrap_or(0));
        Self { idle_time_ns: stats.idle_time.unwrap_or(0).max(0), energy_nj }
    }
}

/// Collects CPU load and CPU energy/power metrics from `fuchsia.kernel.Stats::GetCpuStats`,
/// retaining compact per-CPU counters across samples to compute window deltas without per-tick
/// heap reallocations.
#[derive(Debug)]
pub struct CpuCollector {
    proxy: Option<fkernel::StatsProxy>,
    previous_monotonic_ns: Option<i64>,
    previous_cores: Vec<CoreSample>,
    last_metrics: CpuMetrics,
}

impl CpuCollector {
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
        Self {
            proxy,
            previous_monotonic_ns: None,
            previous_cores: Vec::new(),
            last_metrics: CpuMetrics::default(),
        }
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.proxy.is_some()
    }

    pub(crate) fn disconnect(&mut self) {
        self.proxy = None;
    }

    async fn fetch_per_cpu_stats(&mut self) -> Option<Vec<fkernel::PerCpuStats>> {
        let proxy = self.proxy.as_ref()?;
        match proxy.get_cpu_stats().await {
            Ok(stats) => stats.per_cpu_stats,
            Err(err) => {
                if err.is_closed() {
                    self.proxy = None;
                }
                None
            }
        }
    }

    fn save_baseline(&mut self, monotonic_ns: i64, cores: &[fkernel::PerCpuStats]) {
        self.previous_monotonic_ns = Some(monotonic_ns.max(0));
        self.previous_cores.clear();
        self.previous_cores.extend(cores.iter().map(CoreSample::from_fidl));
        self.last_metrics.clear();
    }

    /// Captures a baseline per-CPU idle-time and energy sample, reading `zx::MonotonicInstant`
    /// immediately after `GetCpuStats` returns to match Zircon's forward-projected `idle_time`.
    pub async fn prime_baseline(&mut self) {
        if let Some(cores) = self.fetch_per_cpu_stats().await {
            let monotonic_ns = zx::MonotonicInstant::get().into_nanos().max(0);
            self.save_baseline(monotonic_ns, &cores);
        }
    }

    /// Captures a baseline per-CPU idle-time and energy sample stamped at `monotonic`.
    pub async fn prime_baseline_at(&mut self, monotonic: zx::MonotonicInstant) {
        if let Some(cores) = self.fetch_per_cpu_stats().await {
            self.save_baseline(monotonic.into_nanos().max(0), &cores);
        }
    }

    /// Collects [`CpuMetrics`], sampling `zx::MonotonicInstant` immediately after `GetCpuStats`
    /// returns.
    pub async fn collect(&mut self) -> CpuMetrics {
        let mut out = CpuMetrics::default();
        self.collect_into(&mut out).await;
        out
    }

    /// Collects [`CpuMetrics`] using `monotonic` as the sample timestamp.
    pub async fn collect_at(&mut self, monotonic: zx::MonotonicInstant) -> CpuMetrics {
        let mut out = CpuMetrics::default();
        self.collect_at_into(monotonic, &mut out).await;
        out
    }

    pub(crate) async fn collect_into(&mut self, out: &mut CpuMetrics) {
        let Some(current_cores) = self.fetch_per_cpu_stats().await else {
            out.clear();
            return;
        };
        let monotonic_ns = zx::MonotonicInstant::get().into_nanos().max(0);
        self.record_sample_into(monotonic_ns, &current_cores, out);
    }

    pub(crate) async fn collect_at_into(
        &mut self,
        monotonic: zx::MonotonicInstant,
        out: &mut CpuMetrics,
    ) {
        let Some(current_cores) = self.fetch_per_cpu_stats().await else {
            out.clear();
            return;
        };
        self.record_sample_into(monotonic.into_nanos().max(0), &current_cores, out);
    }

    fn record_sample_into(
        &mut self,
        monotonic_ns: i64,
        current_cores: &[fkernel::PerCpuStats],
        out: &mut CpuMetrics,
    ) {
        out.clear();

        if current_cores.is_empty() {
            return;
        }

        if let Some(previous_monotonic_ns) = self.previous_monotonic_ns
            && self.previous_cores.len() == current_cores.len()
        {
            let delta_ns = monotonic_ns - previous_monotonic_ns;
            if delta_ns <= 0 {
                out.clone_from(&self.last_metrics);
                return;
            }

            let inv_delta_ns = 1.0_f64 / delta_ns as f64;
            let mut online_load_sum_pct = 0.0_f64;
            let mut online_core_count = 0_usize;
            let mut total_energy_delta_nj = 0_u64;
            let mut has_energy_telemetry = false;
            out.per_core_load_pct.reserve(current_cores.len());

            for (previous, current_fidl) in self.previous_cores.iter_mut().zip(current_cores.iter())
            {
                let current = CoreSample::from_fidl(current_fidl);
                let is_online =
                    current_fidl.flags.is_none_or(|flags| (flags & CPU_STATS_FLAG_ONLINE) != 0);
                if is_online {
                    let idle_delta_ns =
                        current.idle_time_ns.saturating_sub(previous.idle_time_ns).max(0);
                    let core_load_pct = ((1.0 - (idle_delta_ns as f64 * inv_delta_ns))
                        * FULL_PERCENT)
                        .clamp(0.0, FULL_PERCENT);
                    online_load_sum_pct += core_load_pct;
                    online_core_count += 1;
                    out.per_core_load_pct.push(core_load_pct.round() as u8);
                } else {
                    out.per_core_load_pct.push(0);
                }

                has_energy_telemetry |= current.energy_nj > 0;
                total_energy_delta_nj = total_energy_delta_nj
                    .saturating_add(current.energy_nj.saturating_sub(previous.energy_nj));

                *previous = current;
            }

            self.previous_monotonic_ns = Some(monotonic_ns);
            if online_core_count > 0 {
                out.avg_load_pct = Some((online_load_sum_pct / online_core_count as f64) as f32);
            }
            if has_energy_telemetry {
                out.power_mw = Some(
                    ((total_energy_delta_nj as f64 * inv_delta_ns) * MILLIWATTS_PER_WATT).round()
                        as u32,
                );
            }

            self.last_metrics.clone_from(out);
            return;
        }

        self.save_baseline(monotonic_ns, current_cores);
    }
}
