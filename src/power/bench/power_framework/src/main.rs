// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A benchmark runner for SAG, based on Criterion.
//!
//! main.rs contains benchmarks for AcquireWakeLease from SAG.

mod daemon_work;
mod sag_work;

use anyhow::Result;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_system as fsystem;
use fidl_fuchsia_power_topology_test as fpt;

use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::Criterion;
use std::sync::Arc;

fn bench_acquire_wake_lease(
    b: &mut criterion::Bencher,
    sag: Arc<fsystem::ActivityGovernorSynchronousProxy>,
) {
    b.iter(|| {
        sag_work::execute(&sag);
    });
}

fn bench_toggle_lease(
    b: &mut criterion::Bencher,
    topology_control: Arc<fpt::TopologyControlSynchronousProxy>,
    status_channel: Arc<fbroker::StatusSynchronousProxy>,
) {
    b.iter(|| {
        daemon_work::execute(&topology_control, &status_channel);
    });
}

fn get_sag_benches(name: &'static str) -> criterion::Benchmark {
    let sag_arc = sag_work::obtain_sag_proxy();
    criterion::Benchmark::new(name, move |b| bench_acquire_wake_lease(b, sag_arc.clone()))
}

fn get_daemon_benches() -> criterion::Benchmark {
    let (topology_control, status_channel) = daemon_work::prepare_work();
    criterion::Benchmark::new("ToggleLease", move |b| {
        bench_toggle_lease(b, topology_control.clone(), status_channel.clone())
    })
}

fn get_large_topology_lease_benches(name: &'static str) -> criterion::Benchmark {
    let num_elements = 20;
    let topology_control = daemon_work::prepare_large_topology(num_elements);
    criterion::Benchmark::new(name, move |b| {
        let randomize = true;
        b.iter(|| {
            daemon_work::execute_acquire_and_drop_lease(&topology_control, num_elements, randomize);
        });
    })
}

fn main() -> Result<()> {
    let mut c = FuchsiaCriterion::default();
    let internal_c: &mut Criterion = &mut c;
    *internal_c = std::mem::take(internal_c)
        .warm_up_time(std::time::Duration::from_millis(10))
        .measurement_time(std::time::Duration::from_millis(100))
        .sample_size(100);

    let _: &mut Criterion =
        c.bench("fuchsia.power.framework", get_sag_benches("TakeDropWakeLease"));

    // Hold a background wake lease to keep SAG's execution state lease active. This bypasses the
    // expensive Power Broker state transition and watch loop for subsequent benchmarks.
    let sag_arc = sag_work::obtain_sag_proxy();
    let background_wake_lease =
        sag_arc.acquire_wake_lease("benchmark", zx::MonotonicInstant::INFINITE).unwrap().unwrap();

    let _: &mut Criterion =
        c.bench("fuchsia.power.framework", get_sag_benches("TakeMonitoredWakeLease"));

    // Hold a background unmonitored lease to bypass both the Power Broker transition overhead
    // and SAG's long wake lease monitoring timer, enabling clean fast-path measurements.
    drop(background_wake_lease);
    let _unmonitored_lease_to_avoid_monitoring = sag_arc
        .acquire_unmonitored_wake_lease("benchmark_unmonitored", zx::MonotonicInstant::INFINITE)
        .unwrap()
        .unwrap();

    let _: &mut Criterion = c.bench("fuchsia.power.framework", get_sag_benches("TakeWakeLease"));

    let _: &mut Criterion = c.bench("fuchsia.power.framework", get_daemon_benches());
    let _: &mut Criterion =
        c.bench("fuchsia.power.framework", get_large_topology_lease_benches("LargeTopologyLease"));

    Ok(())
}
