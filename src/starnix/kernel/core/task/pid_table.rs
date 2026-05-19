// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{ProcessGroup, Task, ThreadGroup};
use fuchsia_rcu::RcuOptionBox;
use starnix_logging::track_stub;
use starnix_rcu::{RcuHashMap, RcuReadScope};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, pid_t, tid_t};
use std::sync::Arc;

// The maximal pid considered.
const PID_MAX_LIMIT: pid_t = 1 << 15;

#[derive(Default, Debug)]
pub struct PidTable {
    /// The most-recently allocated pid in this table.
    last_pid: pid_t,

    /// The tasks in this table, organized by pid_t.
    table: RcuHashMap<pid_t, Arc<Task>>,

    /// The process groups in this table, organized by pid_t.
    process_groups: RcuHashMap<pid_t, Arc<ProcessGroup>>,

    /// Used to notify thread group changes.
    thread_group_notifier: RcuOptionBox<std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>>,
}

impl PidTable {
    pub fn set_thread_group_notifier(
        &self,
        notifier: std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>,
    ) {
        self.thread_group_notifier.update(Some(notifier));
    }

    pub fn allocate_pid(&mut self) -> pid_t {
        loop {
            self.last_pid = {
                let r = self.last_pid + 1;
                if r > PID_MAX_LIMIT {
                    track_stub!(TODO("https://fxbug.dev/322874557"), "pid wraparound");
                    2
                } else {
                    r
                }
            };
            if self.table.get(&RcuReadScope::new(), &self.last_pid).is_none() {
                break;
            }
        }
        self.last_pid
    }

    pub fn get_task(&self, tid: tid_t) -> Result<Arc<Task>, Errno> {
        self.table.get(&RcuReadScope::new(), &tid).cloned().ok_or_else(|| errno!(ESRCH))
    }

    pub fn add_task(&mut self, task: Arc<Task>) {
        assert!(self.table.insert(task.tid, Arc::clone(&task)).is_none());
        if task.is_leader() {
            let scope = RcuReadScope::new();
            // Notify thread group changes.
            if let Some(notifier) = self.thread_group_notifier.as_ref(&scope) {
                task.thread_group.write().notifier = Some(notifier.clone());
                let _ = notifier.send(MemoryAttributionLifecycleEvent::creation(task.tid));
            }
        }
    }

    pub fn remove_task(&self, tid: tid_t) -> Option<Arc<Task>> {
        let task = self.table.remove(&tid)?;
        if task.is_leader() {
            let scope = RcuReadScope::new();
            // Notify thread group changes.
            if let Some(notifier) = self.thread_group_notifier.as_ref(&scope) {
                let _ = notifier.send(MemoryAttributionLifecycleEvent::destruction(tid));
            }
        }
        Some(task)
    }

    pub fn get_process(&self, pid: pid_t) -> Result<Arc<Task>, Errno> {
        let scope = RcuReadScope::new();
        self.table
            .get(&scope, &pid)
            .filter(|task| task.is_leader())
            .cloned()
            .ok_or_else(|| errno!(ESRCH))
    }

    pub fn get_thread_group(&self, pid: pid_t) -> Option<Arc<ThreadGroup>> {
        let scope = RcuReadScope::new();
        self.table
            .get(&scope, &pid)
            .filter(|task| task.is_leader())
            .map(|task| task.thread_group.clone())
    }

    pub fn get_thread_groups(&self) -> Vec<Arc<ThreadGroup>> {
        // Get the thread group of every leader task for which the thread group is not empty. The
        // leader itself may not be live, but the thread group still exists.
        // TODO(https://fxbug.dev/507835515): Clean this up. ThreadGroup::is_live would be helpful.
        let scope = RcuReadScope::new();
        self.table
            .iter(&scope)
            .filter(|(_pid, task)| task.is_leader())
            .map(|(_pid, task)| task.thread_group.clone())
            .collect()
    }

    pub fn get_process_group(&self, pid: pid_t) -> Option<Arc<ProcessGroup>> {
        let scope = RcuReadScope::new();
        self.process_groups.get(&scope, &pid).cloned()
    }

    pub fn add_process_group(&self, process_group: Arc<ProcessGroup>) {
        let removed = self.process_groups.insert(process_group.leader, process_group);
        assert!(removed.is_none());
    }

    pub fn remove_process_group(&self, pid: pid_t) {
        let removed = self.process_groups.remove(&pid);
        assert!(removed.is_some());
    }

    /// Returns the process ids for all processes, including zombies.
    pub fn process_ids(&self) -> Vec<pid_t> {
        let scope = RcuReadScope::new();
        self.table
            .iter(&scope)
            .filter(|(_pid, task)| task.is_leader())
            .map(|(pid, _task)| *pid)
            .collect()
    }

    /// Returns the task ids for all the currently running tasks.
    pub fn live_task_ids(&self) -> Vec<tid_t> {
        let scope = RcuReadScope::new();
        self.table
            .iter(&scope)
            .filter(|(_pid, task)| task.is_live())
            .map(|(tid, _task)| *tid)
            .collect()
    }

    pub fn last_pid(&self) -> pid_t {
        self.last_pid
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }
}
