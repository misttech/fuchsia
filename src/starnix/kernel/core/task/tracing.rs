// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{Kernel, PidTable};
use starnix_logging::{log_debug, log_error, log_info};
use starnix_sync::LockDepRwLock;
use starnix_uapi::{pid_t, tid_t};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Weak};
use zx::Koid;

#[derive(Debug, Clone)]
pub struct KoidPair {
    pub process: Option<Koid>,
    pub thread: Option<Koid>,
}

/// The Linux pid/tid to Koid map is a thread safe hashmap.
pub type PidToKoidMap =
    Arc<LockDepRwLock<HashMap<tid_t, KoidPair>, starnix_sync::PidToKoidMapInnerLock>>;

pub struct TracePerformanceEventManager {
    // This is the map of pid/tid to koid where the tuple is the process koid, thread koid.
    // This grows unbounded (in the range of valid tid_t values). Users of this struct should
    // call |stop| and |clear| once the trace processing is completed to avoid holding memory.
    map: PidToKoidMap,

    // In order to reduce overhead when processing the trace events, make a local copy of the mappings
    // that is not thread safe and use that as the first level cache. Since this makes copies of
    // copies of mappings, |clear| should be called when the mappings are no longer needed.
    local_map: HashMap<tid_t, Option<KoidPair>>,

    // Hold a weak reference to the Kernel so we can make sure the pid to koid map is removed from
    // the kernel when this object is dropped.
    // This reference is also used to indicate this manager has been started.
    weak_kernel: Weak<Kernel>,
}

impl Drop for TracePerformanceEventManager {
    fn drop(&mut self) {
        // Stop is idempotent, and does not error if not started, or already stopped.
        self.stop();
    }
}

impl TracePerformanceEventManager {
    pub fn new() -> Self {
        Self { map: PidToKoidMap::default(), local_map: HashMap::new(), weak_kernel: Weak::new() }
    }

    /// Registers the map with the pid_table so the pid/tid to koid mappings can be recorded when
    /// new threads are created. Since processing the trace events could be done past a thread's
    /// lifetime, no mappings are removed when a thread or process exits.
    /// Additional work may be needed to handle pid reuse (https://fxbug.dev/322874557), currently
    /// new mapping information overwrites existing mappings.
    ///
    /// Calling |start| when this instance has already been started will panic.
    ///
    /// NOTE: This will record all thread and process mappings until |stop| is called. The mapping will
    /// continue to exist in memory until |clear| is called. It is expected that this is a relatively
    /// short period of time, such as the time during capturing a performance trace.
    pub fn start(&mut self, kernel: &Arc<Kernel>) {
        // Provide a reference to the mapping to the kernel so it can be updated as
        // new threads/processes are created.

        if self.weak_kernel.upgrade().is_some() {
            log_error!(
                "TracePerformanceEventManager has already been started. Re-initializing mapping"
            );
        }

        self.weak_kernel = Arc::downgrade(kernel);
        *kernel.pid_to_koid_mapping.write() = Some(self.map.clone());

        let kernel_pids = kernel.pids.read();
        let existing_pid_map = Self::read_existing_pid_map(&*kernel_pids);
        self.map.write().extend(existing_pid_map);
    }

    /// Clears the pid to koid map reference in the kernel passed in to |start|. Stop is a no-op
    /// if start has not been called, or if stop has already been called.
    pub fn stop(&mut self) {
        if let Some(kernel) = self.weak_kernel.upgrade() {
            log_info!("Stopping trace pid mapping. Notifier set to None.");
            *kernel.pid_to_koid_mapping.write() = None;
            self.weak_kernel = Weak::new();
        }
    }

    /// Clears the pid-koid map. After starting, call |load_pid_mappings| to
    /// initialize the table with existing task/process data.
    pub fn clear(&mut self) {
        self.map.write().clear();
        self.local_map.clear();
    }

    // Look up the pid/tid from a local copy of the pid-koid mapping table, and only
    // take a lock on the mapping table if there is a missing key from the local map.
    // Any new keys are added to the local map.
    fn get_mapping(&mut self, pid: pid_t) -> Option<&KoidPair> {
        if self.local_map.is_empty() {
            let shared_map = self.map.read().clone();
            self.local_map.extend(shared_map.into_iter().map(|(k, v)| (k, Some(v))));
        }
        match self.local_map.entry(pid) {
            Entry::Occupied(o) => {
                // If the entry was initialized while the starnix task was being started,
                // it is possible to have a None thread koid. Check the shared map and update
                // the local map in that case. Also check if we cached a miss (None) and see
                // if it has been added to the shared map since.
                let entry_val = o.into_mut();
                let needs_update = match entry_val {
                    Some(koid_pair) => koid_pair.process.is_none() || koid_pair.thread.is_none(),
                    None => true,
                };
                if needs_update {
                    let shared_map = self.map.read();
                    if let Some(updated_koid_pair) = shared_map.get(&pid) {
                        *entry_val = Some(updated_koid_pair.clone());
                    }
                }
                entry_val.as_ref()
            }
            Entry::Vacant(v) => {
                // If there is a miss, check the shared mapping table. This would only happen in
                // extreme cases where the tracing events are being mapped while new events are
                // being created by new threads.
                let shared_map = self.map.read();
                if let Some(koid_pair) = shared_map.get(&pid) {
                    v.insert(Some(koid_pair.clone())).as_ref()
                } else {
                    log_error!("shared map does not include an entry for pid/tid {pid}");
                    v.insert(None);
                    None
                }
            }
        }
    }

    /// Maps a "pid" to the koid. This is also referred to as the "Process Id" in Perfetto terms.
    pub fn map_pid_to_koid(&mut self, pid: pid_t) -> Option<Koid> {
        self.get_mapping(pid).and_then(|pair| pair.process)
    }

    /// Maps a "tid" to the koid. This is also referred to as the "Thread Id" in Perfetto terms.
    pub fn map_tid_to_koid(&mut self, tid: tid_t) -> Option<Koid> {
        self.get_mapping(tid).and_then(|pair| pair.thread)
    }

    /// Use the kernel pid table to make a mapping from linux pid to koid for existing entries.
    fn read_existing_pid_map(pid_table: &PidTable) -> HashMap<tid_t, KoidPair> {
        let mut pid_map = HashMap::new();

        let ids = pid_table.running_task_ids();
        for tid in &ids {
            // Running tasks may exit at any time. Record a task only if a snapshot of its running
            // state can be obtained.
            let Ok(task) = pid_table.get_task(*tid) else {
                continue;
            };
            let Ok(running_state) = task.running_state() else {
                continue;
            };
            let pair = KoidPair {
                process: task.thread_group().get_process_koid().ok(),
                thread: running_state.thread.get().map(|t| t.koid),
            };
            // ignore entries with no process or thread.
            if pair.process.is_some() || pair.thread.is_some() {
                pid_map.insert(*tid, pair);
            }
        }

        log_debug!("Initialized {} pid mappings. From {} ids", pid_map.len(), ids.len());
        pid_map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::ZirconThread;
    use crate::testing::{create_task, spawn_kernel_and_run};
    use futures::channel::oneshot;

    #[fuchsia::test]
    async fn test_initialize_pid_map() {
        let (sender, receiver) = oneshot::channel();
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let pid = current_task.task.tid;
            let tkoid = current_task.running_state().thread.get().map(|t| t.koid);
            let pkoid = current_task.thread_group().get_process_koid().ok();

            let _another_current = create_task(&kernel, "another-task");

            let pid_map = TracePerformanceEventManager::read_existing_pid_map(&*kernel.pids.read());

            assert!(tkoid.is_some());
            assert_eq!(pid_map.len(), 2, "Expected 2 entries in pid_map got {pid_map:?}");
            assert!(pid_map.contains_key(&pid));

            let pair = pid_map.get(&pid).unwrap();
            assert_eq!(pair.process, pkoid);
            assert_eq!(pair.thread, tkoid);
            sender.send(()).unwrap();
        })
        .await;
        receiver.await.unwrap();
    }

    #[fuchsia::test]
    fn test_mapping() {
        let mut manager = TracePerformanceEventManager::new();
        let mut map = HashMap::new();
        map.insert(
            1,
            KoidPair { process: Some(Koid::from_raw(101)), thread: Some(Koid::from_raw(201)) },
        );
        map.insert(2, KoidPair { process: Some(Koid::from_raw(102)), thread: None });
        manager.map.write().extend(map);

        assert_eq!(manager.map_tid_to_koid(0), None);

        assert_eq!(manager.map_pid_to_koid(1), Some(Koid::from_raw(101)));
        assert_eq!(manager.map_tid_to_koid(1), Some(Koid::from_raw(201)));
        assert_eq!(manager.map_pid_to_koid(2), Some(Koid::from_raw(102)));

        // The mapping added did not have a thread value, so it should map 2 -> None.
        assert_eq!(manager.map_tid_to_koid(2), None);

        // Update the thread mapping in the shared map, so now there is a thread id.
        manager.map.write().insert(
            2,
            KoidPair { process: Some(Koid::from_raw(102)), thread: Some(Koid::from_raw(202)) },
        );
        // The tid_to_koid lookup now succeeds since the mapping has been populated.
        assert_eq!(manager.map_tid_to_koid(2), Some(Koid::from_raw(202)));
    }

    #[fuchsia::test]
    fn test_unmapped_tid() {
        let mut manager = TracePerformanceEventManager::new();

        assert_eq!(manager.map_tid_to_koid(2), None);
    }

    #[fuchsia::test]
    async fn test_lifecycle() {
        let (sender, receiver) = oneshot::channel();
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let mut manager = TracePerformanceEventManager::new();

            manager.start(&kernel);

            let pid_map = manager.map.read().clone();
            assert_eq!(pid_map.len(), 1, "Expected 1 entry in pid_map got {pid_map:?}");

            // Associate a thread with a new task.
            let another_current = create_task(&kernel, "another-task");
            let test_thread = another_current
                .thread_group()
                .process
                .create_thread(b"my-new-test-thread")
                .expect("test thread");

            {
                another_current
                    .running_state()
                    .thread
                    .set(ZirconThread::new(Arc::new(test_thread)))
                    .expect("test thread set");
            }

            let pid_map = manager.map.read().clone();
            let pid_dump = format!("{pid_map:?}");
            assert_eq!(pid_map.len(), 1, "Expected 1 entry in pid_map got {pid_dump}");

            // This is called by the task when it is all ready to run.
            another_current.record_pid_koid_mapping();

            // Now expect 2 mappings.
            let pid_map = manager.map.read().clone();
            let pid_dump = format!("{pid_map:?}");
            assert_eq!(pid_map.len(), 2, "Expected 2 entries in pid_map got {pid_dump}");

            // Read the mappings, if it is not present, it will panic.
            let _ = manager.map_pid_to_koid(another_current.task.get_pid());
            let _ = manager.map_pid_to_koid(another_current.task.get_tid());

            manager.stop();

            manager.clear();
            assert!(manager.map.read().is_empty());
            sender.send(()).unwrap();
        })
        .await;
        receiver.await.unwrap();
    }
}
