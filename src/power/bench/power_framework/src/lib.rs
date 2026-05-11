// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Integration test that can help to check the connection to the server
//! and the fidl call.

mod daemon_work;
mod sag_work;

use anyhow::{Error, Result, format_err};
use argh::FromArgs;
use diagnostics_hierarchy::DiagnosticsHierarchy;
use diagnostics_reader::ArchiveReader;
use std::time::Instant;

#[derive(FromArgs, Debug)]
/// Command line argument for the tests
struct Options {
    /// the format for changing the argument on command line is `-- --repeat N`, e.g.
    /// fx test -o power-framework-bench-integration-tests --test-filter=*takewakelease -- --repeat 5000
    #[argh(option, default = "1000")]
    repeat: u32,

    /// switch used by rust test runner.
    #[argh(switch)]
    #[allow(unused)]
    nocapture: bool,

    /// timeout for the test in seconds
    #[argh(option, default = "10")]
    timeout_secs: u64,

    /// enables a quick and dirty mechanism for the host to synchronize with the test to perform
    /// memory profiling:
    ///   - The test will log "WAITING FOR MEMORY PROFILING" and sleeps indefinitely.
    ///   - The host takes a memory profile of the appropriate test component.
    ///   - The host kills the test.
    #[argh(switch)]
    wait_for_memory_profiling: bool,
}

async fn maybe_wait_for_memory_profiling(args: &Options) {
    // See flag docstring for usage.
    if args.wait_for_memory_profiling {
        println!("WAITING FOR MEMORY PROFILING");
        std::future::pending::<()>().await;
    }
}

/// Runs the given function `func` for `args.repeat` times or until `args.timeout_secs` is reached.
/// Prints the power broker inspect stats every 1000 iterations.
/// Returns the number of iterations performed.
async fn iterate_until_timeout<F>(args: &Options, mut func: F) -> u32
where
    F: FnMut(u32),
{
    let timeout = std::time::Duration::from_secs(args.timeout_secs);
    let start = Instant::now();
    let mut iterations = 0;
    while start.elapsed() < timeout && iterations < args.repeat {
        func(iterations);
        if iterations > 0 && iterations % 1000 == 0 {
            print_power_broker_inspect_stats(iterations).await;
        }
        iterations += 1;
    }
    iterations
}

#[fuchsia::test]
async fn test_sag_takewakelease() {
    let args: Options = argh::from_env::<Options>();

    let sag_arc = sag_work::obtain_sag_proxy();
    let start = Instant::now();
    let iterations = iterate_until_timeout(&args, |_| {
        sag_work::execute(&sag_arc);
    })
    .await;
    assert!(iterations > 0, "Test failed to complete at least 1 iteration");
    let duration = start.elapsed();
    println!("Total execution time: {:?}", duration);
    println!("Average time for each call is {:?}", duration / iterations);

    // Check how much PB Inspect VMO we used.
    print_power_broker_inspect_stats(iterations).await;

    maybe_wait_for_memory_profiling(&args).await;
    ()
}

#[fuchsia::test]
async fn test_topologytestdaemon_toggle() -> Result<()> {
    let args: Options = argh::from_env::<Options>();

    let (topology_control, status_channel) = daemon_work::prepare_work();
    let start = Instant::now();
    let iterations = iterate_until_timeout(&args, |_| {
        daemon_work::execute(&topology_control, &status_channel);
    })
    .await;
    assert!(iterations > 0, "Test failed to complete at least 1 iteration");
    let duration = start.elapsed();
    println!("Total execution time: {:?}", duration);
    println!("Average time for each call is {:?}", duration / iterations);

    maybe_wait_for_memory_profiling(&args).await;
    Ok(())
}

async fn get_power_broker_inspect() -> Result<DiagnosticsHierarchy, Error> {
    ArchiveReader::inspect()
        .select_all_for_component("test-power-broker")
        .snapshot()
        .await?
        .into_iter()
        .next()
        .and_then(|result| result.payload)
        .ok_or_else(|| format_err!("expected one inspect hierarchy"))
}

fn get_inspect_vmo_bytes(inspect: &DiagnosticsHierarchy) -> (u64, u64) {
    let curr = inspect
        .get_property_by_path(&vec!["fuchsia.inspect.Stats", "current_size"])
        .unwrap()
        .uint()
        .unwrap();
    let max = inspect
        .get_property_by_path(&vec!["fuchsia.inspect.Stats", "maximum_size"])
        .unwrap()
        .uint()
        .unwrap();
    return (curr, max);
}

async fn print_power_broker_inspect_stats(iteration: u32) {
    let pb_inspect = get_power_broker_inspect().await.expect("Inspect data");
    let (used, max) = get_inspect_vmo_bytes(&pb_inspect);
    println!(
        "{} - Power Broker inspect used {} / {} bytes, {:.0} % utilization",
        iteration,
        used,
        max,
        (used as f64 / max as f64) * 100.0
    );
    ()
}

#[fuchsia::test]
async fn test_large_topology_lease_benchmark() -> Result<()> {
    // TODO(b/491223927): I'd like to get this to at least 100, but starting
    // here, and we'll bump it up as we make improvements.
    let num_elements = 20;
    let args: Options = argh::from_env::<Options>();

    println!("Building large topology with {} elements...", num_elements);
    let topology_control = daemon_work::prepare_large_topology(num_elements);
    println!("Topology created.");

    let start = Instant::now();
    let randomize = false;
    let iterations = iterate_until_timeout(&args, |_| {
        daemon_work::execute_acquire_and_drop_lease(&topology_control, num_elements, randomize);
    })
    .await;
    assert!(iterations > 0, "Test failed to complete at least 1 iteration");
    let duration = start.elapsed();
    println!("Total execution time over {} iterations: {:?}", iterations, duration);
    println!(
        "Average time for each execution ({} leases acquire/drop) is {:?}",
        iterations,
        duration / iterations
    );

    print_power_broker_inspect_stats(iterations).await;

    maybe_wait_for_memory_profiling(&args).await;
    Ok(())
}
