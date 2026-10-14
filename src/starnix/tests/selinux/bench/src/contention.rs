// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Multi-threaded contention benchmarks for [`SecurityServer`] and [`ConcurrentAccessCache`].

use crate::{
    POPULATED_SID_TABLE_SIZE, context_with_category_id, new_populated_security_server,
    pin_thread_to_big_cores,
};
use criterion::BenchmarkGroup;
use criterion::measurement::WallTime;
use selinux::policy::AccessVector;
use selinux::{
    AccessQueryArgs, ClassId, ConcurrentAccessCache, FileSystemMountOptions, KernelAccessDecision,
    KernelClass, SecurityId, SecurityServer,
};
use std::hint::{black_box, spin_loop};
use std::num::NonZeroU32;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Number of background worker threads spawned alongside the measured benchmark thread.
///
/// Assumes the target system has 4-8 CPU cores (`NUM_BACKGROUND_THREADS + 1` active threads,
/// matching the 4 big cores on VIM3) so the measured thread and background workers run
/// concurrently without time-slicing preemption.
const NUM_BACKGROUND_THREADS: usize = 3;

/// Simulated non-SELinux CPU work (userspace execution and kernel syscall/VFS work) between
/// [`SecurityServer`] operations on a background worker thread.
///
/// Models a worst-case burst of uncached or metadata-creating syscalls (since steady-state
/// permission checks are mostly absorbed by [`PerThreadCache`](selinux::PerThreadCache) and
/// [`ConcurrentAccessCache`]), keeping lock-scalability regressions well above measurement noise.
const INTER_CHECK_WORK_DURATION: Duration = Duration::from_nanos(1500);

/// Number of distinct read-only [`SecurityServer`] operation kinds cycled through by background
/// workers in [`run_with_contention`].
const NUM_READ_OP_KINDS: usize = 4;

/// Number of distinct category-encoded contexts cycled through by contended benchmarks.
const WORKING_SET_SIZE: u32 = 16;

/// Offset between workers' starting positions in the [`SecurityServer`] working sets, so that
/// workers do not issue identical queries in lockstep.
const WORKER_STEP_STAGGER: usize = 7;

/// Number of loop iterations between write-locked operations by the first worker in
/// [`ContentionMode::ReadWrite`].
///
/// Write locks on [`SecurityServer`] only occur in production on `mount(2)` or when minting a
/// never-before-seen dynamic SID, so they are much less frequent than read-locked operations.
const WRITE_LOCK_PERIOD: usize = 256;

/// Number of pre-populated entries in [`concurrent_access_cache_get_contended_bench`].
const CACHE_WORKING_SET_SIZE: u32 = 1000;

/// Prime offset between workers' starting positions in
/// [`concurrent_access_cache_get_contended_bench`] to avoid lockstep bucket accesses.
const CACHE_WORKER_STEP_STAGGER: usize = 131;

/// Number of loop iterations between cache-miss insertions (and inter-burst pacing) per
/// background worker in [`concurrent_access_cache_get_contended_bench`].
const CACHE_MISS_PERIOD: usize = 16;

/// Base SID value (disjoint from `1..=CACHE_WORKING_SET_SIZE + 1`) used by background workers to
/// synthesize cache-miss keys in [`concurrent_access_cache_get_contended_bench`].
const CACHE_MISS_BASE_SID: u32 = 2000;

/// Number of distinct cache-miss keys cycled through by each background worker in
/// [`concurrent_access_cache_get_contended_bench`].
///
/// Must significantly exceed `DEFAULT_SHARED_SIZE.access_cache_capacity` (`2048`) so that miss
/// keys do not remain resident in spare bucket slots after warm-up, ensuring background workers
/// continuously trigger CLOCK eviction and seqlock writes during measurement.
const CACHE_MISS_SID_COUNT: u32 = 8192;

/// Contention mode for multi-threaded [`SecurityServer`] benchmarks.
#[derive(Clone, Copy)]
enum ContentionMode {
    /// Background worker threads perform paced read-locked `SidTable` lookups and policy
    /// computations against the shared [`SecurityServer`]. None of these consult the AVC: raw
    /// access decisions bypass it, and named create-SID queries are never cached.
    ReadOnly,
    /// As [`ContentionMode::ReadOnly`], plus the first worker takes the state write lock on every
    /// [`WRITE_LOCK_PERIOD`]th iteration via [`SecurityServer::resolve_fs_label`]. It re-resolves
    /// contexts that are already in the `SidTable`, so the table does not grow during measurement.
    ReadWrite,
}

/// RAII guard that signals background worker threads to exit when dropped, ensuring
/// [`thread::scope`] does not hang waiting to join workers if the measured closure panics.
struct StopGuard<'a> {
    stop_flag: &'a AtomicBool,
}

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Simulates non-SELinux CPU work between resource-accessing syscalls while remaining responsive
/// to `stop_flag`.
fn simulate_inter_check_work(stop_flag: &AtomicBool) {
    let start = Instant::now();
    while !stop_flag.load(Ordering::Relaxed) && start.elapsed() < INTER_CHECK_WORK_DURATION {
        spin_loop();
    }
}

/// Generates a working set of [`WORKING_SET_SIZE`] security contexts on `base_context` whose
/// category encodings are distinct from the filler contexts in
/// [`populate_sid_table`](crate::populate_sid_table).
fn make_working_set_contexts(base_context: &str) -> Vec<Vec<u8>> {
    (0..WORKING_SET_SIZE)
        .map(|index| {
            context_with_category_id(base_context, POPULATED_SID_TABLE_SIZE + index).into_bytes()
        })
        .collect()
}

/// Pre-computed working sets for [`SecurityServer`] background contention workers.
///
/// Built once per benchmark during setup so that all background SIDs are populated in the
/// `SidTable` before Criterion warm-up begins and setup work is not repeated on every sample.
struct ContentionWorkload {
    read_contexts: Vec<Vec<u8>>,
    read_sids: Vec<SecurityId>,
    access_queries: Vec<(SecurityId, SecurityId, ClassId)>,
    create_queries: Vec<(SecurityId, SecurityId, ClassId, &'static [u8])>,
    mount_options_list: Vec<FileSystemMountOptions>,
}

impl ContentionWorkload {
    fn new(server: &SecurityServer) -> Self {
        let read_contexts = make_working_set_contexts("u:r:kernel:s0");
        let read_sids: Vec<SecurityId> = read_contexts
            .iter()
            .map(|context| server.security_context_to_sid(context.as_slice().into()).unwrap())
            .collect();

        let source_contexts = make_working_set_contexts("u:r:untrusted_app:s0");
        let target_contexts = make_working_set_contexts("u:object_r:app_data_file:s0");
        let file_class_id = server.class_id_by_name("file").unwrap();
        let access_queries: Vec<(SecurityId, SecurityId, ClassId)> = source_contexts
            .iter()
            .zip(target_contexts.iter())
            .map(|(source, target)| {
                (
                    server.security_context_to_sid(source.as_slice().into()).unwrap(),
                    server.security_context_to_sid(target.as_slice().into()).unwrap(),
                    file_class_id,
                )
            })
            .collect();

        let create_sources = make_working_set_contexts("u:r:system_server:s0");
        let create_target_sid =
            server.security_context_to_sid(b"u:object_r:system_data_file:s0".into()).unwrap();
        let sock_class_id = server.class_id_by_name(KernelClass::SockFile.name()).unwrap();
        let create_queries: Vec<(SecurityId, SecurityId, ClassId, &'static [u8])> = create_sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let source_sid = server.security_context_to_sid(source.as_slice().into()).unwrap();
                let filename: &'static [u8] =
                    if index % 2 == 0 { b"ndebugsocket" } else { b"unlikely_filename_1234" };
                let _ = server
                    .compute_create_sid_raw(source_sid, create_target_sid, sock_class_id, filename)
                    .unwrap();
                (source_sid, create_target_sid, sock_class_id, filename)
            })
            .collect();

        let mount_options_list: Vec<FileSystemMountOptions> = read_contexts
            .iter()
            .map(|context| FileSystemMountOptions {
                context: Some(context.clone()),
                ..FileSystemMountOptions::default()
            })
            .collect();

        Self { read_contexts, read_sids, access_queries, create_queries, mount_options_list }
    }
}

/// Runs `bench_closure` while background worker threads concurrently exercise `server` under
/// `contention_mode` using pre-computed `workload`.
///
/// Spawns and joins [`NUM_BACKGROUND_THREADS`] scoped worker threads around `bench_closure` (once
/// per Criterion warm-up and measurement sample), so no background thread outlives the benchmark
/// and per-thread shard assignments or core placements average out across samples rather than
/// biasing an entire benchmark run. Each background worker simulates non-SELinux syscall/userspace
/// CPU work ([`INTER_CHECK_WORK_DURATION`]) between [`SecurityServer`] operations rather than 100%
/// duty-cycle lock spinning. Only the calling thread's `bench.iter` loop inside `bench_closure` is
/// timed. A `Relaxed` store to `stop_flag` suffices to stop the workers because [`thread::scope`]
/// joins every worker before returning.
fn run_with_contention(
    server: &SecurityServer,
    workload: &ContentionWorkload,
    contention_mode: ContentionMode,
    bench_closure: impl FnOnce(),
) {
    let start_barrier = Barrier::new(NUM_BACKGROUND_THREADS + 1);
    let stop_flag = AtomicBool::new(false);

    thread::scope(|scope| {
        for worker_index in 0..NUM_BACKGROUND_THREADS {
            let server_ref = server;
            let start_barrier_ref = &start_barrier;
            let stop_flag_ref = &stop_flag;

            scope.spawn(move || {
                pin_thread_to_big_cores();
                let mut step = worker_index * WORKER_STEP_STAGGER;

                start_barrier_ref.wait();
                while !stop_flag_ref.load(Ordering::Relaxed) {
                    let query_index = step / NUM_READ_OP_KINDS;
                    match step % NUM_READ_OP_KINDS {
                        0 => {
                            let context =
                                &workload.read_contexts[query_index % workload.read_contexts.len()];
                            let _ = black_box(
                                server_ref
                                    .security_context_to_sid(context.as_slice().into())
                                    .unwrap(),
                            );
                        }
                        1 => {
                            let sid = workload.read_sids[query_index % workload.read_sids.len()];
                            let _ = black_box(server_ref.sid_to_security_context(sid).unwrap());
                        }
                        2 => {
                            let &(source_sid, target_sid, class_id) = &workload.access_queries
                                [query_index % workload.access_queries.len()];
                            let _ = black_box(
                                server_ref
                                    .compute_access_decision_raw(source_sid, target_sid, class_id),
                            );
                        }
                        _ => {
                            let &(create_source, create_target, target_class_id, filename) =
                                &workload.create_queries
                                    [query_index % workload.create_queries.len()];
                            let _ = black_box(
                                server_ref
                                    .compute_create_sid_raw(
                                        create_source,
                                        create_target,
                                        target_class_id,
                                        filename,
                                    )
                                    .unwrap(),
                            );
                        }
                    }

                    if matches!(contention_mode, ContentionMode::ReadWrite)
                        && worker_index == 0
                        && (step + WRITE_LOCK_PERIOD / 2) % WRITE_LOCK_PERIOD == 0
                    {
                        let write_index = step / WRITE_LOCK_PERIOD;
                        let mount_options = &workload.mount_options_list
                            [write_index % workload.mount_options_list.len()];
                        let _ = black_box(
                            server_ref.resolve_fs_label(b"tmpfs".into(), mount_options).unwrap(),
                        );
                    }

                    step = step.wrapping_add(1);
                    simulate_inter_check_work(stop_flag_ref);
                }
            });
        }

        let _stop_guard = StopGuard { stop_flag: &stop_flag };
        start_barrier.wait();
        bench_closure();
    });
}

fn security_context_to_sid_contended_bench(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name_suffix: &'static str,
    contention_mode: ContentionMode,
) {
    let server = new_populated_security_server();
    let workload = ContentionWorkload::new(&server);
    let working_set = make_working_set_contexts("u:r:kernel:s0");
    for context in &working_set {
        let _ = server.security_context_to_sid(context.as_slice().into()).unwrap();
    }

    let _ = group.bench_function(format!("security_context_to_sid_{name_suffix}"), move |bench| {
        run_with_contention(&server, &workload, contention_mode, || {
            let mut cursor = 0usize;
            bench.iter(|| {
                let context_bytes = &working_set[cursor % working_set.len()];
                cursor = cursor.wrapping_add(1);
                let _ = black_box(
                    server.security_context_to_sid(context_bytes.as_slice().into()).unwrap(),
                );
            });
        });
    });
}

fn compute_access_decision_contended_bench(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name_suffix: &'static str,
    contention_mode: ContentionMode,
) {
    let server = new_populated_security_server();
    let workload = ContentionWorkload::new(&server);
    let source_contexts = make_working_set_contexts("u:r:untrusted_app:s0");
    let target_contexts = make_working_set_contexts("u:object_r:app_data_file:s0");
    let class_id = server.class_id_by_name("file").unwrap();
    let queries: Vec<(SecurityId, SecurityId, ClassId)> = source_contexts
        .iter()
        .zip(target_contexts.iter())
        .map(|(source, target)| {
            (
                server.security_context_to_sid(source.as_slice().into()).unwrap(),
                server.security_context_to_sid(target.as_slice().into()).unwrap(),
                class_id,
            )
        })
        .collect();

    let _ = group.bench_function(format!("compute_access_decision_{name_suffix}"), move |bench| {
        run_with_contention(&server, &workload, contention_mode, || {
            let mut cursor = 0usize;
            bench.iter(|| {
                let &(source_sid, target_sid, class_id) = &queries[cursor % queries.len()];
                cursor = cursor.wrapping_add(1);
                let _ =
                    black_box(server.compute_access_decision_raw(source_sid, target_sid, class_id));
            });
        });
    });
}

fn compute_create_sid_contended_bench(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name_suffix: &'static str,
    contention_mode: ContentionMode,
) {
    let server = new_populated_security_server();
    let workload = ContentionWorkload::new(&server);
    let source_contexts = make_working_set_contexts("u:r:kernel:s0");
    let target_sid = server.security_context_to_sid(b"u:object_r:init_exec:s0".into()).unwrap();
    let class_id = server.class_id_by_name("process").unwrap();
    let queries: Vec<(SecurityId, SecurityId, ClassId)> = source_contexts
        .iter()
        .map(|source| {
            let source_sid = server.security_context_to_sid(source.as_slice().into()).unwrap();
            let _ = server
                .compute_create_sid_raw(source_sid, target_sid, class_id, /* name = */ &[])
                .unwrap();
            (source_sid, target_sid, class_id)
        })
        .collect();

    let _ = group.bench_function(format!("compute_create_sid_{name_suffix}"), move |bench| {
        run_with_contention(&server, &workload, contention_mode, || {
            let mut cursor = 0usize;
            bench.iter(|| {
                let &(source_sid, target_sid, class_id) = &queries[cursor % queries.len()];
                cursor = cursor.wrapping_add(1);
                let _ = black_box(
                    server
                        .compute_create_sid_raw(
                            source_sid,
                            target_sid,
                            class_id,
                            /* name = */ &[],
                        )
                        .unwrap(),
                );
            });
        });
    });
}

fn compute_create_sid_filename_contended_bench(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name_suffix: &'static str,
    contention_mode: ContentionMode,
) {
    let server = new_populated_security_server();
    let workload = ContentionWorkload::new(&server);
    let source_contexts = make_working_set_contexts("u:r:system_server:s0");
    let target_sid =
        server.security_context_to_sid(b"u:object_r:system_data_file:s0".into()).unwrap();
    let class_id = server.class_id_by_name(KernelClass::SockFile.name()).unwrap();
    let queries: Vec<(SecurityId, SecurityId, ClassId, &'static [u8])> = source_contexts
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let source_sid = server.security_context_to_sid(source.as_slice().into()).unwrap();
            let filename: &'static [u8] =
                if index % 2 == 0 { b"ndebugsocket" } else { b"unlikely_filename_1234" };
            let _ =
                server.compute_create_sid_raw(source_sid, target_sid, class_id, filename).unwrap();
            (source_sid, target_sid, class_id, filename)
        })
        .collect();

    let _ =
        group.bench_function(format!("compute_create_sid_filename_{name_suffix}"), move |bench| {
            run_with_contention(&server, &workload, contention_mode, || {
                let mut cursor = 0usize;
                bench.iter(|| {
                    let &(source_sid, target_sid, class_id, filename) =
                        &queries[cursor % queries.len()];
                    cursor = cursor.wrapping_add(1);
                    let _ = black_box(
                        server
                            .compute_create_sid_raw(source_sid, target_sid, class_id, filename)
                            .unwrap(),
                    );
                });
            });
        });
}

fn concurrent_access_cache_get_contended_bench(group: &mut BenchmarkGroup<'_, WallTime>) {
    let cache = ConcurrentAccessCache::new(selinux::DEFAULT_SHARED_SIZE.access_cache_capacity);
    let value = KernelAccessDecision {
        allow: AccessVector::ALL,
        audit: AccessVector::NONE,
        flags: 0,
        todo_bug: None,
    };

    let keys: Vec<AccessQueryArgs> = (1..=CACHE_WORKING_SET_SIZE)
        .map(|index| AccessQueryArgs {
            source_sid: SecurityId(NonZeroU32::new(index).unwrap()),
            target_sid: SecurityId(NonZeroU32::new(index + 1).unwrap()),
            target_class: KernelClass::Process,
        })
        .collect();

    for key in &keys {
        let _ = cache.get_or_try_insert::<()>(key, || Ok(value));
    }

    let _ = group.bench_function("concurrent_access_cache_get_contended", move |bench| {
        let start_barrier = Barrier::new(NUM_BACKGROUND_THREADS + 1);
        let stop_flag = AtomicBool::new(false);

        thread::scope(|scope| {
            for worker_index in 0..NUM_BACKGROUND_THREADS {
                let cache_ref = &cache;
                let keys_ref = &keys;
                let start_barrier_ref = &start_barrier;
                let stop_flag_ref = &stop_flag;

                scope.spawn(move || {
                    pin_thread_to_big_cores();
                    let worker_miss_base =
                        CACHE_MISS_BASE_SID + (worker_index as u32) * CACHE_MISS_SID_COUNT;
                    let mut step = worker_index * CACHE_WORKER_STEP_STAGGER;
                    start_barrier_ref.wait();
                    while !stop_flag_ref.load(Ordering::Relaxed) {
                        // Interleave hits on existing keys with misses that insert/evict entries
                        // in the shared lock-free buckets, then simulate non-SELinux CPU work
                        // once per burst of `CACHE_MISS_PERIOD` lookups.
                        let is_miss_step = step % CACHE_MISS_PERIOD == 0;
                        let key = if is_miss_step {
                            let miss_index = step / CACHE_MISS_PERIOD;
                            let dynamic_sid =
                                worker_miss_base + ((miss_index as u32) % CACHE_MISS_SID_COUNT);
                            AccessQueryArgs {
                                source_sid: SecurityId(NonZeroU32::new(dynamic_sid).unwrap()),
                                target_sid: SecurityId(NonZeroU32::new(dynamic_sid + 1).unwrap()),
                                target_class: KernelClass::File,
                            }
                        } else {
                            keys_ref[step % keys_ref.len()].clone()
                        };
                        let _ = black_box(cache_ref.get_or_try_insert::<()>(&key, || Ok(value)));
                        step = step.wrapping_add(1);
                        if is_miss_step {
                            simulate_inter_check_work(stop_flag_ref);
                        } else {
                            spin_loop();
                        }
                    }
                });
            }

            let _stop_guard = StopGuard { stop_flag: &stop_flag };
            start_barrier.wait();
            bench.iter(|| {
                for key in &keys {
                    let _ = black_box(cache.get_or_try_insert::<()>(key, || Ok(value)));
                }
            });
        });
    });
}

/// Registers all multi-threaded contention benchmarks on `group`.
pub(super) fn register_benchmarks(group: &mut BenchmarkGroup<'_, WallTime>) {
    security_context_to_sid_contended_bench(group, "read_contended", ContentionMode::ReadOnly);
    security_context_to_sid_contended_bench(group, "write_contended", ContentionMode::ReadWrite);
    compute_access_decision_contended_bench(group, "read_contended", ContentionMode::ReadOnly);
    compute_access_decision_contended_bench(group, "write_contended", ContentionMode::ReadWrite);
    compute_create_sid_contended_bench(group, "read_contended", ContentionMode::ReadOnly);
    compute_create_sid_contended_bench(group, "write_contended", ContentionMode::ReadWrite);
    compute_create_sid_filename_contended_bench(group, "read_contended", ContentionMode::ReadOnly);
    concurrent_access_cache_get_contended_bench(group);
}
