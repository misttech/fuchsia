// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! common functions to be used by Criterion or integration test for the
/// Topology Test Daemon.
use anyhow::Result;
use fidl::endpoints::create_sync_proxy;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_topology_test as fpt;
use fuchsia_component::client::connect_to_protocol_sync;

use rand::Rng;
use std::sync::Arc;

#[inline(always)]
fn black_box<T>(placeholder: T) -> T {
    criterion::black_box(placeholder)
}

fn work_func(
    topology_control: &fpt::TopologyControlSynchronousProxy,
    status_channel: &Arc<fbroker::StatusSynchronousProxy>,
) -> Result<()> {
    // Acquire lease for C @ 5.

    let _ = topology_control
        .acquire_lease("C", 5, fbroker::LeaseStatus::Unknown, zx::MonotonicInstant::INFINITE)
        .unwrap();
    let level = status_channel
        .watch_power_level(zx::MonotonicInstant::INFINITE)
        .expect("Fidl call should work")
        .expect("Result should be good");
    assert_eq!(level, 5);

    let _ = topology_control.drop_lease("C", zx::MonotonicInstant::INFINITE).unwrap();
    let level = status_channel
        .watch_power_level(zx::MonotonicInstant::INFINITE)
        .expect("Fidl call should work")
        .expect("Result should be good");
    assert_eq!(level, 0);

    Ok(())
}

pub(crate) fn prepare_work()
-> (Arc<fpt::TopologyControlSynchronousProxy>, Arc<fbroker::StatusSynchronousProxy>) {
    // Current Criterion library doesn't support async call yet.
    let topology_control = connect_to_protocol_sync::<fpt::TopologyControlMarker>().unwrap();

    let elements: [fpt::Element; 2] = [
        fpt::Element {
            element_name: "C".to_string(),
            initial_current_level: 0,
            valid_levels: vec![0, 5],
            dependencies: vec![fpt::LevelDependency {
                dependent_level: 5,
                requires_element: "P".to_string(),
                requires_level: 50,
            }],
        },
        fpt::Element {
            element_name: "P".to_string(),
            initial_current_level: 0,
            valid_levels: vec![0, 30, 50],
            dependencies: vec![],
        },
    ];
    let _ = topology_control.create(&elements, zx::MonotonicInstant::INFINITE).unwrap();
    let (status_channel, server_channel) = create_sync_proxy::<fbroker::StatusMarker>();
    let _ =
        topology_control.open_status_channel("C", server_channel, zx::MonotonicInstant::INFINITE);

    let level = status_channel
        .watch_power_level(zx::MonotonicInstant::INFINITE)
        .expect("Fidl call should work")
        .expect("Result should be good");
    assert_eq!(level, 0);

    (Arc::new(topology_control), Arc::new(status_channel))
}

pub(crate) fn execute(
    topology_control: &fpt::TopologyControlSynchronousProxy,
    status_channel: &Arc<fbroker::StatusSynchronousProxy>,
) {
    let _ = black_box(work_func(topology_control, status_channel));
}

fn acquire_and_drop_rand_lease_work_func(
    topology_control: &fpt::TopologyControlSynchronousProxy,
    num_elements: usize,
) -> Result<()> {
    let mut rng = rand::rng();
    let i = rng.random_range(0..num_elements);

    let _ = topology_control
        .acquire_lease(
            &format!("element_{}", i),
            1,
            fbroker::LeaseStatus::Satisfied,
            zx::MonotonicInstant::INFINITE,
        )
        .unwrap();

    let _ = topology_control
        .drop_lease(&format!("element_{}", i), zx::MonotonicInstant::INFINITE)
        .unwrap();
    Ok(())
}

pub(crate) fn prepare_large_topology(
    num_elements: usize,
) -> Arc<fpt::TopologyControlSynchronousProxy> {
    let topology_control = connect_to_protocol_sync::<fpt::TopologyControlMarker>().unwrap();

    let mut elements = Vec::new();

    for i in 0..num_elements {
        let name = format!("element_{}", i);
        let mut deps = Vec::new();
        if i > 0 {
            deps.push(fpt::LevelDependency {
                dependent_level: 1,
                requires_element: format!("element_{}", i - 1),
                requires_level: 1,
            });
            // Multi-dependencies to trigger exponential explosions natively
            if i > 1 {
                deps.push(fpt::LevelDependency {
                    dependent_level: 1,
                    requires_element: format!("element_{}", i - 2),
                    requires_level: 1,
                });
            }
        }
        elements.push(fpt::Element {
            element_name: name,
            initial_current_level: 0,
            valid_levels: vec![0, 1],
            dependencies: deps,
        });
    }

    let _ = topology_control.create(&elements, zx::MonotonicInstant::INFINITE).unwrap();

    Arc::new(topology_control)
}

pub(crate) fn execute_acquire_and_drop_rand_lease(
    topology_control: &fpt::TopologyControlSynchronousProxy,
    num_elements: usize,
) {
    let _ = black_box(acquire_and_drop_rand_lease_work_func(topology_control, num_elements));
}
