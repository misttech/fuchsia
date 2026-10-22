// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of (e)BPF.
//!
//! BPF stands for Berkeley Packet Filter and is an API introduced in BSD that allows filtering
//! network packets by running little programs in the kernel. eBPF stands for extended BFP and
//! is a Linux extension of BPF that allows hooking BPF programs into many different
//! non-networking-related contexts.

pub mod attachments;
pub mod context;
pub mod fs;
pub mod map;
pub mod program;
pub mod syscalls;

use crate::bpf::attachments::EbpfAttachments;
use crate::bpf::map::{BpfMapHandle, BpfMapId, WeakBpfMapHandle};
use crate::bpf::program::{ProgramHandle, ProgramId, WeakProgramHandle};
use crate::security;
use crate::task::CurrentTask;
use ebpf::MapFlags;
use fuchsia_rcu::RcuReadScope;
use starnix_rcu::RcuHashMap;
use starnix_uapi::auth::{CAP_SYS_ADMIN, Capabilities};
use starnix_uapi::bpf_map_type_BPF_MAP_TYPE_SK_STORAGE;
use starnix_uapi::errors::Errno;
use std::sync::Arc;
use zerocopy::IntoBytes as _;

/// Checks if `current_task` has either `capability` or [`CAP_SYS_ADMIN`], matching Linux
/// capability auditing behavior.
///
/// In Linux, `capability` is checked first with auditing. If `capability` is absent, an audit
/// denial is generated for `capability`, and [`CAP_SYS_ADMIN`] is checked as a fallback with auditing.
/// If [`CAP_SYS_ADMIN`] is also absent, an audit denial is generated for [`CAP_SYS_ADMIN`].
pub(crate) fn check_cap_or_sys_admin(
    current_task: &CurrentTask,
    capability: Capabilities,
) -> Result<(), Errno> {
    if security::check_task_capable(current_task, capability).is_ok() {
        return Ok(());
    }
    security::check_task_capable(current_task, CAP_SYS_ADMIN)
}

/// Stores global eBPF state.
#[derive(Default)]
pub struct EbpfState {
    pub attachments: EbpfAttachments,

    programs: RcuHashMap<ProgramId, WeakProgramHandle>,
    maps: RcuHashMap<BpfMapId, WeakBpfMapHandle>,
}

impl EbpfState {
    fn register_program(&self, program: &ProgramHandle) {
        self.programs.insert(program.id(), Arc::downgrade(program));
    }

    fn unregister_program(&self, id: ProgramId) {
        self.programs.remove(&id).expect("Missing eBPF program");
    }

    fn get_next_program_id(&self, start_id: ProgramId) -> Option<ProgramId> {
        let scope = RcuReadScope::new();
        self.programs.keys(&scope).copied().filter(|&id| id > start_id).min()
    }

    fn get_program_by_id(&self, id: ProgramId) -> Option<ProgramHandle> {
        let scope = RcuReadScope::new();
        self.programs.get(&scope, &id).and_then(|p| p.upgrade())
    }

    fn register_map(&self, map: &BpfMapHandle) {
        self.maps.insert(map.id(), Arc::downgrade(map));
    }

    fn unregister_map(&self, id: BpfMapId) {
        self.maps.remove(&id).expect("Missing eBPF map");
    }

    fn get_next_map_id(&self, start_id: BpfMapId) -> Option<BpfMapId> {
        let scope = RcuReadScope::new();
        self.maps.keys(&scope).copied().filter(|&id| id > start_id).min()
    }

    fn get_map_by_id(&self, id: BpfMapId) -> Option<BpfMapHandle> {
        let scope = RcuReadScope::new();
        self.maps.get(&scope, &id).and_then(|m| m.upgrade())
    }

    /// Removed socket with the specified `cookie` from all `sk_storage` maps.
    // TODO(https://fxbug.dev/496639039): Move sk_storage cleanup to Netstack.
    pub fn remove_sk_storage_entries(&self, cookie: u64) {
        let scope = RcuReadScope::new();
        for weak_map in self.maps.values(&scope) {
            if let Some(map) = weak_map.upgrade()
                && map.schema.map_type == bpf_map_type_BPF_MAP_TYPE_SK_STORAGE
            {
                let _ = map.delete(cookie.as_bytes());
            }
        }
    }

    /// Clones `sk_storage` map entries with `BPF_F_CLONE` from `parent_cookie` socket to
    /// `child_cookie` socket.
    pub fn clone_sk_storage_entries(&self, parent_cookie: u64, child_cookie: u64) {
        let scope = RcuReadScope::new();
        for weak_map in self.maps.values(&scope) {
            if let Some(map) = weak_map.upgrade()
                && map.schema.map_type == bpf_map_type_BPF_MAP_TYPE_SK_STORAGE
                && map.schema.flags.contains(MapFlags::Clone)
            {
                if let Some(mut val) = map.load(parent_cookie.as_bytes()) {
                    let _ = map.update(child_cookie.as_bytes(), (&mut val[..]).into(), 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpf::map::BpfMap;
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::testing::spawn_kernel_and_run;
    use ebpf::MapSchema;
    use starnix_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY;
    use std::sync::atomic::{AtomicBool, Ordering};

    const PARENT_COOKIE_BASE: u64 = 10_000;
    const CHILD_COOKIE_BASE: u64 = 20_000;
    const STRESS_WRITER_ITERATIONS: usize = 50;
    const STRESS_READER_THREADS: u64 = 2;
    const RELEASER_DRAIN_INTERVAL: u32 = 16;

    fn create_sk_storage_map(current_task: &CurrentTask, clone_flag: bool) -> BpfMapHandle {
        let mut flags = MapFlags::NoPrealloc;
        if clone_flag {
            flags |= MapFlags::Clone;
        }
        let schema = MapSchema {
            map_type: bpf_map_type_BPF_MAP_TYPE_SK_STORAGE,
            key_size: 4,
            value_size: 8,
            max_entries: 0,
            flags,
        };
        BpfMap::new(current_task, schema, "sk_map", security::bpf_map_alloc(current_task))
            .expect("create sk_storage map")
    }

    #[::fuchsia::test]
    async fn test_sk_storage_clone_and_remove() {
        spawn_kernel_and_run(async |current_task| {
            let ebpf_state = &current_task.kernel().ebpf_state;
            let cloneable_map = create_sk_storage_map(current_task, true);
            let non_cloneable_map = create_sk_storage_map(current_task, false);
            let array_map = BpfMap::new(
                current_task,
                MapSchema {
                    map_type: bpf_map_type_BPF_MAP_TYPE_ARRAY,
                    key_size: 4,
                    value_size: 8,
                    max_entries: 1,
                    flags: MapFlags::empty(),
                },
                "arr_map",
                security::bpf_map_alloc(current_task),
            )
            .expect("create array map");

            assert_eq!(ebpf_state.maps.len(), 3);
            assert!(ebpf_state.get_map_by_id(cloneable_map.id()).is_some());
            assert_eq!(
                ebpf_state.get_next_map_id(cloneable_map.id()),
                Some(non_cloneable_map.id())
            );
            assert_eq!(ebpf_state.get_next_map_id(non_cloneable_map.id()), Some(array_map.id()));
            assert_eq!(ebpf_state.get_next_map_id(array_map.id()), None);

            let parent_cookie: u64 = 1001;
            let child_cookie: u64 = 1002;
            let mut val1 = 0xdeadbeef_u64.to_ne_bytes();
            let mut val2 = 0xcafebabe_u64.to_ne_bytes();

            cloneable_map
                .update(parent_cookie.as_bytes(), (&mut val1[..]).into(), 0)
                .expect("insert parent in cloneable_map");
            non_cloneable_map
                .update(parent_cookie.as_bytes(), (&mut val2[..]).into(), 0)
                .expect("insert parent in non_cloneable_map");

            // Hold `ebpf_state.maps.lock()` while invoking `clone_sk_storage_entries` and
            // `remove_sk_storage_entries` to verify that neither method acquires
            // `self.maps.lock()`.
            {
                let _maps_guard = ebpf_state.maps.lock();
                ebpf_state.clone_sk_storage_entries(parent_cookie, child_cookie);
            }

            assert_eq!(cloneable_map.load(child_cookie.as_bytes()).as_deref(), Some(&val1[..]));
            assert!(non_cloneable_map.load(child_cookie.as_bytes()).is_none());

            {
                let _maps_guard = ebpf_state.maps.lock();
                ebpf_state.remove_sk_storage_entries(parent_cookie);
            }
            assert!(cloneable_map.load(parent_cookie.as_bytes()).is_none());
            assert!(non_cloneable_map.load(parent_cookie.as_bytes()).is_none());
            assert_eq!(cloneable_map.load(child_cookie.as_bytes()).as_deref(), Some(&val1[..]));

            drop(array_map);
            drop(cloneable_map);
            drop(non_cloneable_map);
            current_task.trigger_delayed_releaser();
            fuchsia_rcu::rcu_run_callbacks();

            assert!(ebpf_state.maps.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_concurrent_sk_storage_register_unregister_and_lookup() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let done = Arc::new(AtomicBool::new(false));

            let mut reader_results = Vec::new();
            for reader_idx in 0..STRESS_READER_THREADS {
                let done_clone = done.clone();
                let (res_fn, req) = SpawnRequestBuilder::new()
                    .with_sync_closure(move |task: &CurrentTask| {
                        let ebpf_state = &task.kernel().ebpf_state;
                        let parent_cookie = PARENT_COOKIE_BASE + reader_idx;
                        let child_cookie = CHILD_COOKIE_BASE + reader_idx;
                        let mut iteration = 0u32;
                        while !done_clone.load(Ordering::Relaxed) {
                            ebpf_state.clone_sk_storage_entries(parent_cookie, child_cookie);
                            ebpf_state.remove_sk_storage_entries(child_cookie);
                            iteration = iteration.wrapping_add(1);
                            if iteration % RELEASER_DRAIN_INTERVAL == 0 {
                                task.trigger_delayed_releaser();
                            }
                        }
                        task.trigger_delayed_releaser();
                    })
                    .build_with_sync_result();
                kernel.kthreads.spawner().spawn_from_request(req);
                reader_results.push(res_fn);
            }

            for _ in 0..STRESS_WRITER_ITERATIONS {
                let sk_map = create_sk_storage_map(current_task, true);
                let array_map = BpfMap::new(
                    current_task,
                    MapSchema {
                        map_type: bpf_map_type_BPF_MAP_TYPE_ARRAY,
                        key_size: 4,
                        value_size: 8,
                        max_entries: 1,
                        flags: MapFlags::empty(),
                    },
                    "arr_map",
                    security::bpf_map_alloc(current_task),
                )
                .expect("create array map");

                let mut value = 1234u64.to_ne_bytes();
                for reader_idx in 0..STRESS_READER_THREADS {
                    let parent_cookie = PARENT_COOKIE_BASE + reader_idx;
                    let _ = sk_map.update(parent_cookie.as_bytes(), (&mut value[..]).into(), 0);
                }

                drop(array_map);
                drop(sk_map);
                current_task.trigger_delayed_releaser();
                fuchsia_rcu::rcu_run_callbacks();
            }

            done.store(true, Ordering::Relaxed);
            for res_fn in reader_results {
                res_fn().expect("reader thread completed");
            }
            current_task.trigger_delayed_releaser();
            fuchsia_rcu::rcu_run_callbacks();
        })
        .await;
    }
}
