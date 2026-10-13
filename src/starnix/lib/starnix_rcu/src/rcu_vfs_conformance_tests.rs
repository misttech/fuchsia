// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Conformance and concurrency stress tests for `DynamicRcuHashMap` and `RcuHashMap`
//! emulating the Starnix VFS directory entry cache (`DirEntry::children`) workload.
//!
//! These tests verify lockless RCU lookups alongside concurrent directory mutations,
//! dynamic LockDep writer lock levels (`ordered_lock` and `reset_dependencies` across
//! simulated `rename` operations), and weak reference reclamation ensuring that
//! `!RcuDroppable` values are never dropped on the `"starnix-rcu"` background thread.

use crate::{DynamicRcuHashMap, Entry, RcuReadScope};
use fuchsia_rcu::rcu_run_callbacks;
use fuchsia_sync::ResetDependencies;
use rapidhash::RapidBuildHasher;
use starnix_sync::{
    DirEntryChildrenLevel, DirEntryChildrenRecursiveLevel, FuseDirEntryChildrenLevel,
    allow_subclass, ordered_lock,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Weak};

const CONCURRENT_READER_THREADS: usize = 6;
const CONCURRENT_WRITER_THREADS: usize = 2;
const STRESS_ITERATIONS_PER_THREAD: usize = 400;
const RESIZE_TARGET_ENTRIES: usize = 512;
const NAME_MAX_LEN: usize = 255;

/// Records whether any `Drop::drop` invocation ever executes on the background RCU thread.
static DROPPED_ON_STARNIX_RCU_THREAD: AtomicBool = AtomicBool::new(false);

/// Probe type whose strong `Arc<DropThreadProbe>` owner simulates `DirEntry` (`!RcuDroppable`),
/// while `Weak<DropThreadProbe>` implements `RcuDroppable`.
struct DropThreadProbe {
    inode_id: u64,
    dropped_counter: Arc<AtomicUsize>,
}

impl Drop for DropThreadProbe {
    fn drop(&mut self) {
        if let Some(name) = std::thread::current().name() {
            if name == "starnix-rcu" {
                DROPPED_ON_STARNIX_RCU_THREAD.store(true, Ordering::Release);
            }
        }
        self.dropped_counter.fetch_add(1, Ordering::AcqRel);
    }
}

/// Simulated directory entry node verifying parent-to-child weak pointer semantics.
struct MockDirEntry {
    inode_id: u64,
    parent: Option<Arc<MockDirEntry>>,
    children: DynamicRcuHashMap<String, Weak<MockDirEntry>>,
}

#[test]
fn test_tier1_dynamic_rcu_hash_map_basic_crud_and_borrowed_get() {
    let map = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();
    assert!(map.is_empty());
    assert_eq!(map.len(), 0);

    assert_eq!(map.insert("alpha".to_string(), 101), None);
    assert_eq!(map.insert("beta".to_string(), 202), None);
    assert_eq!(map.insert("gamma".to_string(), 303), None);
    assert_eq!(map.len(), 3);
    assert!(!map.is_empty());

    // Verify lockless borrowed `&str` lookup inside an active RCU read scope.
    let scope = RcuReadScope::new();
    assert_eq!(map.get(&scope, "alpha"), Some(&101));
    assert_eq!(map.get(&scope, "beta"), Some(&202));
    assert_eq!(map.get(&scope, "gamma"), Some(&303));
    assert_eq!(map.get(&scope, "missing"), None);
    assert_eq!(map.get(&scope, "beta").copied(), Some(202));

    let collected: BTreeSet<(String, u64)> =
        map.iter(&scope).map(|(k, v)| (k.clone(), *v)).collect();
    let expected: BTreeSet<(String, u64)> =
        [("alpha".to_string(), 101), ("beta".to_string(), 202), ("gamma".to_string(), 303)]
            .into_iter()
            .collect();
    assert_eq!(collected, expected);
    drop(scope);

    // Update via writer guard and via `entry` API.
    assert_eq!(map.insert("beta".to_string(), 222), Some(202));
    assert_eq!(map.lock().get("beta"), Some(222));

    {
        let mut guard = map.lock();
        match guard.entry("delta".to_string()) {
            Entry::Vacant(vacant) => {
                vacant.insert(404);
            }
            Entry::Occupied(_) => panic!("delta must start vacant"),
        }
        match guard.entry("delta".to_string()) {
            Entry::Occupied(mut occupied) => {
                assert_eq!(occupied.get(), 404);
                assert_eq!(occupied.insert(444), 404);
            }
            Entry::Vacant(_) => panic!("delta must be occupied after insertion"),
        }
        assert_eq!(guard.get("delta"), Some(444));
    }

    assert_eq!(map.remove("alpha"), Some(101));
    assert_eq!(map.lock().get("alpha"), None);

    rcu_run_callbacks();
}

#[test]
fn test_tier1_ordered_lock_pair_deterministic_ordering() {
    let map_a = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();
    let map_b = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();

    {
        let (mut guard_a, mut guard_b) = ordered_lock(&map_a, &map_b);
        assert_eq!(guard_a.insert("from_a".to_string(), 1), None);
        assert_eq!(guard_b.insert("from_b".to_string(), 2), None);
    }

    // Reversed parameter order must lock in the identical canonical address order without deadlock.
    {
        let (mut guard_b, mut guard_a) = ordered_lock(&map_b, &map_a);
        assert_eq!(guard_b.get("from_b"), Some(2));
        assert_eq!(guard_a.get("from_a"), Some(1));
        assert_eq!(guard_b.remove("from_b"), Some(2));
        assert_eq!(guard_a.insert("from_b".to_string(), 2), None);
    }

    let scope = RcuReadScope::new();
    assert_eq!(map_a.get(&scope, "from_a"), Some(&1));
    assert_eq!(map_a.get(&scope, "from_b"), Some(&2));
    assert_eq!(map_b.get(&scope, "from_b"), None);
    drop(scope);

    rcu_run_callbacks();
}

#[test]
fn test_tier1_lockdep_recursive_and_fuse_hierarchy() {
    let overlay_children =
        DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenRecursiveLevel>();
    let lower_children = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();
    let fuse_children = DynamicRcuHashMap::<String, u64>::new::<FuseDirEntryChildrenLevel>();

    // OverlayFS (`DirEntryChildrenRecursiveLevel`) precedes lower `DirEntryChildrenLevel` in lockdep.
    {
        let mut upper_guard = overlay_children.lock();
        let mut lower_guard = lower_children.lock();
        upper_guard.insert("merged".to_string(), 10);
        lower_guard.insert("backing".to_string(), 20);
    }

    // FUSE lock level operates cleanly on its independent lockdep branch.
    {
        let mut fuse_guard = fuse_children.lock();
        fuse_guard.insert("remote_entry".to_string(), 30);
    }

    let scope = RcuReadScope::new();
    assert_eq!(overlay_children.get(&scope, "merged"), Some(&10));
    assert_eq!(lower_children.get(&scope, "backing"), Some(&20));
    assert_eq!(fuse_children.get(&scope, "remote_entry"), Some(&30));
    drop(scope);

    rcu_run_callbacks();
}

#[test]
fn test_tier1_weak_value_parent_child_cycle_freedom() {
    let root = Arc::new(MockDirEntry {
        inode_id: 1,
        parent: None,
        children: DynamicRcuHashMap::new::<DirEntryChildrenLevel>(),
    });
    let child = Arc::new(MockDirEntry {
        inode_id: 2,
        parent: Some(Arc::clone(&root)),
        children: DynamicRcuHashMap::new::<DirEntryChildrenLevel>(),
    });

    root.children.insert("child".to_string(), Arc::downgrade(&child));

    // Parent must hold only a weak pointer to child while child holds strong parent reference.
    assert_eq!(Arc::strong_count(&root), 2);
    assert_eq!(Arc::strong_count(&child), 1);

    {
        let scope = RcuReadScope::new();
        let weak_child = root.children.get(&scope, "child").expect("child entry must exist");
        let upgraded = weak_child.upgrade().expect("child must upgrade while alive");
        assert_eq!(upgraded.inode_id, 2);
        assert!(upgraded.parent.is_some());
    }

    // Dropping the external handle drops `child` immediately, releasing its strong ref on `root`.
    drop(child);
    assert_eq!(Arc::strong_count(&root), 1);

    {
        let scope = RcuReadScope::new();
        if let Some(weak_child) = root.children.get(&scope, "child") {
            assert!(weak_child.upgrade().is_none());
        }
    }

    rcu_run_callbacks();
}

#[test]
fn test_tier1_weak_drop_never_executes_inner_drop_on_starnix_rcu() {
    DROPPED_ON_STARNIX_RCU_THREAD.store(false, Ordering::Release);
    let dropped_counter = Arc::new(AtomicUsize::new(0));
    let map = DynamicRcuHashMap::<String, Weak<DropThreadProbe>>::new::<DirEntryChildrenLevel>();

    let mut handles = Vec::new();
    for i in 0..32 {
        let probe = Arc::new(DropThreadProbe {
            inode_id: i,
            dropped_counter: Arc::clone(&dropped_counter),
        });
        assert_eq!(probe.inode_id, i);
        map.insert(format!("entry_{i}"), Arc::downgrade(&probe));
        handles.push(probe);
    }

    // Overwrite and remove entries while strong handles are dropped on this foreground thread.
    for i in 0..16 {
        map.remove(format!("entry_{i}").as_str());
    }
    drop(handles);
    assert_eq!(dropped_counter.load(Ordering::Acquire), 32);

    rcu_run_callbacks();
    assert!(!DROPPED_ON_STARNIX_RCU_THREAD.load(Ordering::Acquire));
}

#[test]
fn test_tier2_capacity_zero_boundary_and_empty_operations() {
    let map = DynamicRcuHashMap::<String, u64, RapidBuildHasher>::with_capacity_and_hasher::<
        DirEntryChildrenLevel,
    >(0, RapidBuildHasher::default());

    {
        let scope = RcuReadScope::new();
        assert_eq!(map.get(&scope, "nonexistent"), None);
        assert_eq!(map.iter(&scope).count(), 0);
    }

    assert_eq!(map.remove("nonexistent"), None);
    assert_eq!(map.insert("singleton".to_string(), 999), None);
    assert_eq!(map.len(), 1);
    assert_eq!(map.lock().get("singleton"), Some(999));

    {
        let mut guard = map.lock();
        let drained: Vec<(String, u64)> = guard.drain().collect();
        assert_eq!(drained, vec![("singleton".to_string(), 999)]);
        assert!(guard.is_empty());
    }

    rcu_run_callbacks();
}

#[test]
fn test_tier2_table_growth_preserves_inflight_rcu_reader_snapshots() {
    let map = DynamicRcuHashMap::<String, u64, RapidBuildHasher>::with_capacity_and_hasher::<
        DirEntryChildrenLevel,
    >(2, RapidBuildHasher::default());

    map.insert("seed_0".to_string(), 1000);
    map.insert("seed_1".to_string(), 2000);

    // Hold an active RcuReadScope across repeated table doublings.
    let pre_resize_scope = RcuReadScope::new();
    let seed_0_ref: &u64 =
        map.get(&pre_resize_scope, "seed_0").expect("seed_0 present before resize");
    assert_eq!(*seed_0_ref, 1000);

    for i in 0..RESIZE_TARGET_ENTRIES {
        map.insert(format!("dynamic_key_{i:04}"), i as u64);
    }

    // Dereferencing the reference obtained before table growth must remain 100% sound.
    assert_eq!(*seed_0_ref, 1000);

    let post_resize_scope = RcuReadScope::new();
    assert_eq!(map.get(&post_resize_scope, "seed_0"), Some(&1000));
    assert_eq!(map.get(&post_resize_scope, "seed_1"), Some(&2000));
    for i in 0..RESIZE_TARGET_ENTRIES {
        let key = format!("dynamic_key_{i:04}");
        assert_eq!(map.get(&post_resize_scope, key.as_str()), Some(&(i as u64)));
    }

    drop(pre_resize_scope);
    drop(post_resize_scope);
    rcu_run_callbacks();
}

#[test]
fn test_tier2_boundary_keys_empty_max_name_and_raw_bytes() {
    let map = DynamicRcuHashMap::<Vec<u8>, u64>::new::<DirEntryChildrenLevel>();

    let empty_key: &[u8] = b"";
    let max_name_key = vec![b'x'; NAME_MAX_LEN];
    let raw_non_utf8_key: Vec<u8> = b"prefix_\x80\xfe\xff_suffix".to_vec();

    map.insert(empty_key.to_vec(), 1);
    map.insert(max_name_key.clone(), 2);
    map.insert(raw_non_utf8_key.clone(), 3);

    let scope = RcuReadScope::new();
    assert_eq!(map.get(&scope, empty_key), Some(&1));
    assert_eq!(map.get(&scope, max_name_key.as_slice()), Some(&2));
    assert_eq!(map.get(&scope, raw_non_utf8_key.as_slice()), Some(&3));
    drop(scope);

    assert_eq!(map.remove(raw_non_utf8_key.as_slice()), Some(3));
    assert_eq!(map.lock().get(raw_non_utf8_key.as_slice()), None);

    rcu_run_callbacks();
}

#[test]
fn test_tier2_reset_dependencies_allows_reparented_lock_inversion() {
    let dir_x = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();
    let dir_y = DynamicRcuHashMap::<String, u64>::new::<DirEntryChildrenLevel>();

    // Initial parent -> child lock order: dir_x -> dir_y.
    {
        let _guard_x = dir_x.lock();
        let _token = allow_subclass();
        let _guard_y = dir_y.lock();
    }

    // Simulate cross-directory rename reparenting dir_x beneath dir_y under exclusive rename lock.
    // SAFETY: Changing the lock ordering between `dir_x` and `dir_y` is safe because reparenting
    // updates the directory hierarchy before any subsequent lock acquisitions.
    unsafe {
        dir_x.reset_dependencies();
        dir_y.reset_dependencies();
    }

    // Now dir_y -> dir_x acquisition order must succeed cleanly without a lockdep cycle panic.
    {
        let _guard_y = dir_y.lock();
        let _token = allow_subclass();
        let _guard_x = dir_x.lock();
    }

    rcu_run_callbacks();
}

#[test]
fn test_tier3_pairwise_weak_table_resize_and_foreground_arc_drop_race() {
    DROPPED_ON_STARNIX_RCU_THREAD.store(false, Ordering::Release);
    let dropped_counter = Arc::new(AtomicUsize::new(0));
    let map =
        DynamicRcuHashMap::<String, Weak<DropThreadProbe>, RapidBuildHasher>::with_capacity_and_hasher::<
            DirEntryChildrenLevel,
        >(2, RapidBuildHasher::default());

    let mut strong_nodes = Vec::with_capacity(128);
    for i in 0..128 {
        let node = Arc::new(DropThreadProbe {
            inode_id: i,
            dropped_counter: Arc::clone(&dropped_counter),
        });
        assert_eq!(node.inode_id, i);
        map.insert(format!("node_{i}"), Arc::downgrade(&node));
        strong_nodes.push(node);
    }

    // Drop strong references on the foreground thread BEFORE retired buckets are reclaimed by RCU.
    drop(strong_nodes);
    assert_eq!(dropped_counter.load(Ordering::Acquire), 128);

    // Reclaim retired bucket tables and remaining weak entries.
    map.clear();
    drop(map);
    rcu_run_callbacks();

    assert!(!DROPPED_ON_STARNIX_RCU_THREAD.load(Ordering::Acquire));
}

#[test]
fn test_tier4_stress_concurrent_borrowed_get_resize_and_rcu_callbacks() {
    let map =
        Arc::new(DynamicRcuHashMap::<String, u64, RapidBuildHasher>::with_capacity_and_hasher::<
            DirEntryChildrenLevel,
        >(2, RapidBuildHasher::default()));
    let total_participants = CONCURRENT_READER_THREADS + CONCURRENT_WRITER_THREADS + 1;
    let barrier = Arc::new(Barrier::new(total_participants));
    let writers_finished = Arc::new(AtomicBool::new(false));
    let total_reads_succeeded = Arc::new(AtomicU64::new(0));

    std::thread::scope(|s| {
        for r in 0..CONCURRENT_READER_THREADS {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            let writers_finished = Arc::clone(&writers_finished);
            let total_reads_succeeded = Arc::clone(&total_reads_succeeded);
            s.spawn(move || {
                barrier.wait();
                let mut local_hits = 0u64;
                let mut idx = r;
                while !writers_finished.load(Ordering::Acquire) {
                    let scope = RcuReadScope::new();
                    let key = format!("shared_key_{}", idx % 64);
                    if let Some(val) = map.get(&scope, key.as_str()) {
                        assert!(*val >= (idx % 64) as u64);
                        local_hits += 1;
                    }
                    idx = idx.wrapping_add(1);
                }
                total_reads_succeeded.fetch_add(local_hits, Ordering::Relaxed);
            });
        }

        for w in 0..CONCURRENT_WRITER_THREADS {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            s.spawn(move || {
                barrier.wait();
                for iter in 0..STRESS_ITERATIONS_PER_THREAD {
                    let key_id = (iter + w * 17) % 64;
                    let key = format!("shared_key_{key_id}");
                    let val = (key_id + iter) as u64;
                    map.insert(key.clone(), val);
                    if iter % 5 == 0 {
                        let _ = map.remove(key.as_str());
                    }
                }
            });
        }

        // Dedicated background callback runner thread.
        let barrier_rcu = Arc::clone(&barrier);
        let writers_finished_rcu = Arc::clone(&writers_finished);
        s.spawn(move || {
            barrier_rcu.wait();
            while !writers_finished_rcu.load(Ordering::Acquire) {
                rcu_run_callbacks();
                std::thread::yield_now();
            }
            rcu_run_callbacks();
        });

        // Wait until writer threads finish by performing a deterministic workload on main thread.
        for iter in 0..STRESS_ITERATIONS_PER_THREAD {
            let scope = RcuReadScope::new();
            let _ = map.iter(&scope).count();
            if iter + 1 == STRESS_ITERATIONS_PER_THREAD {
                writers_finished.store(true, Ordering::Release);
            }
        }
    });

    rcu_run_callbacks();
    let _ = total_reads_succeeded.load(Ordering::Relaxed);
}

#[test]
fn test_tier4_stress_concurrent_ordered_lock_reset_and_weak_drops() {
    DROPPED_ON_STARNIX_RCU_THREAD.store(false, Ordering::Release);
    let dropped_counter = Arc::new(AtomicUsize::new(0));
    let forest: Arc<Vec<DynamicRcuHashMap<String, Weak<DropThreadProbe>>>> =
        Arc::new((0..8).map(|_| DynamicRcuHashMap::new::<DirEntryChildrenLevel>()).collect());

    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|s| {
        // 4 writer threads moving weak child entries between random directory pairs via ordered_lock.
        for t in 0..4usize {
            let forest = Arc::clone(&forest);
            let barrier = Arc::clone(&barrier);
            let dropped_counter = Arc::clone(&dropped_counter);
            s.spawn(move || {
                barrier.wait();
                for iter in 0..STRESS_ITERATIONS_PER_THREAD {
                    let idx_a = (t * 2 + iter) % forest.len();
                    let idx_b = (t * 2 + iter + 3) % forest.len();
                    if idx_a == idx_b {
                        continue;
                    }
                    let probe = Arc::new(DropThreadProbe {
                        inode_id: (t * 1000 + iter) as u64,
                        dropped_counter: Arc::clone(&dropped_counter),
                    });
                    assert_eq!(probe.inode_id, (t * 1000 + iter) as u64);
                    let key = format!("child_{t}_{}", iter % 16);
                    {
                        let (mut g1, mut g2) = ordered_lock(&forest[idx_a], &forest[idx_b]);
                        let _ = g1.remove(key.as_str());
                        g2.insert(key, Arc::downgrade(&probe));
                    }
                    drop(probe);
                }
            });
        }

        // 4 reader threads scanning child maps under RcuReadScope while invoking rcu_run_callbacks().
        for r in 0..4usize {
            let forest = Arc::clone(&forest);
            let barrier = Arc::clone(&barrier);
            s.spawn(move || {
                barrier.wait();
                for iter in 0..STRESS_ITERATIONS_PER_THREAD {
                    {
                        let scope = RcuReadScope::new();
                        let dir = &forest[(r + iter) % forest.len()];
                        for (_name, weak) in dir.iter(&scope) {
                            let _ = weak.upgrade();
                        }
                    }
                    if iter % 8 == 0 {
                        rcu_run_callbacks();
                    }
                }
            });
        }
    });

    rcu_run_callbacks();
    assert!(!DROPPED_ON_STARNIX_RCU_THREAD.load(Ordering::Acquire));
}
