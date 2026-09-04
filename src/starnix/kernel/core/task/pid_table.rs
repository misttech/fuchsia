// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{ProcessGroup, Task, ThreadGroup};
use fuchsia_rcu::{RcuDroppable, RcuOptionBox, RcuWeak};
use starnix_logging::track_stub;
use starnix_rcu::RcuReadScope;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, pid_t, tid_t};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

// The maximal pid considered.
const PID_MAX_LIMIT: pid_t = 1 << 15;

#[derive(Debug, RcuDroppable)]
enum ProcessEntry {
    ThreadGroup(Weak<ThreadGroup>),
    Zombie,
}

impl ProcessEntry {
    fn thread_group(&self) -> Option<&Weak<ThreadGroup>> {
        match self {
            Self::ThreadGroup(group) => Some(group),
            _ => None,
        }
    }
}

/// Entities identified by a pid.
#[derive(Debug)]
pub struct PidEntry {
    pub id: pid_t,
    task: RcuWeak<Task>,
    process: RcuOptionBox<ProcessEntry>,
    process_group: RcuWeak<ProcessGroup>,
}

impl PidEntry {
    pub fn get_task(&self) -> Result<Arc<Task>, Errno> {
        self.task.upgrade().ok_or_else(|| errno!(ESRCH))
    }

    pub fn get_process(&self) -> Option<ProcessEntryRef> {
        let process = self.process.read()?;
        match &*process {
            ProcessEntry::ThreadGroup(thread_group) => {
                let thread_group = thread_group
                    .upgrade()
                    .expect("ThreadGroup was released, but not removed from PidTable");
                Some(ProcessEntryRef::Process(thread_group))
            }
            ProcessEntry::Zombie => Some(ProcessEntryRef::Zombie),
        }
    }

    pub fn get_thread_group(&self) -> Option<Arc<ThreadGroup>> {
        match self.get_process() {
            Some(ProcessEntryRef::Process(tg)) => Some(tg),
            _ => None,
        }
    }

    pub fn get_process_group(&self) -> Option<Arc<ProcessGroup>> {
        self.process_group.upgrade()
    }

    #[cfg(test)]
    pub fn new_for_test(id: pid_t) -> Pid {
        Arc::new(Self::new(id))
    }

    fn new(id: pid_t) -> Self {
        Self {
            id,
            task: Default::default(),
            process: Default::default(),
            process_group: Default::default(),
        }
    }

    fn is_empty(&self, scope: &RcuReadScope) -> bool {
        self.task.strong_count(scope) == 0
            && self.process.is_none(scope)
            && self.process_group.strong_count(scope) == 0
    }
}

impl std::fmt::Display for PidEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id)
    }
}

impl PartialEq for PidEntry {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for PidEntry {}

impl std::hash::Hash for PidEntry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self as *const Self).hash(state);
    }
}

pub enum ProcessEntryRef {
    Process(Arc<ThreadGroup>),
    Zombie,
}

pub type Pid = Arc<PidEntry>;

#[derive(Default, Debug)]
pub struct PidTable {
    /// The most-recently allocated pid in this table.
    last_pid: pid_t,

    /// The tasks in this table, organized by pid_t.
    table: HashMap<pid_t, Pid>,

    /// Used to notify thread group changes.
    thread_group_notifier: RcuOptionBox<std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>>,
}

impl PidTable {
    pub fn get(&self, pid: pid_t) -> Result<&Pid, Errno> {
        self.table.get(&pid).ok_or_else(|| errno!(ESRCH))
    }

    fn get_or_create_entry(&mut self, pid: pid_t) -> &PidEntry {
        let entry = self.table.entry(pid).or_insert_with(|| Arc::new(PidEntry::new(pid)));
        &**entry
    }

    fn remove_item<F>(&mut self, pid: pid_t, do_remove: F)
    where
        F: FnOnce(&PidEntry),
    {
        let scope = RcuReadScope::new();
        if let Some(entry) = self.table.get(&pid) {
            do_remove(entry);
            if entry.is_empty(&scope) {
                self.table.remove(&pid);
            }
        }
    }

    pub fn set_thread_group_notifier(
        &self,
        notifier: std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>,
    ) {
        self.thread_group_notifier.update(Some(notifier));
    }

    pub fn allocate_pid(&mut self) -> Pid {
        let scope = RcuReadScope::new();
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
            if let Some(entry) = self.table.get(&self.last_pid) {
                if entry.is_empty(&scope) {
                    self.table.remove(&self.last_pid);
                } else {
                    continue;
                }
            }
            break;
        }
        let pid = Arc::new(PidEntry::new(self.last_pid));
        self.table.insert(self.last_pid, pid.clone());
        pid
    }

    pub fn get_task(&self, tid: tid_t) -> Result<Arc<Task>, Errno> {
        self.get(tid)?.get_task()
    }

    pub fn add_task(&mut self, task: Arc<Task>) {
        let entry = self.get_or_create_entry(task.tid.id);
        let scope = RcuReadScope::new();
        assert_eq!(entry.task.strong_count(&scope), 0);
        entry.task.update(Arc::downgrade(&task));

        // If we're not cloning a thread, add its thread group
        if task.is_leader() {
            assert!(entry.process.is_none(&scope));
            entry
                .process
                .update(Some(ProcessEntry::ThreadGroup(Arc::downgrade(task.thread_group()))));

            // Notify thread group changes.
            if let Some(notifier) = self.thread_group_notifier.cloned() {
                let mut tg_state = task.thread_group.write();
                let _ = notifier.send(MemoryAttributionLifecycleEvent::creation(task.tid.id));
                tg_state.notifier = Some(notifier);
            }
        }
    }

    pub fn remove_task(&mut self, tid: tid_t) {
        self.remove_item(tid, |entry| {
            let scope = RcuReadScope::new();
            assert!(entry.task.strong_count(&scope) > 0);
            entry.task.update(Weak::new());
        });
    }

    pub fn get_process(&self, pid: pid_t) -> Option<ProcessEntryRef> {
        self.get(pid).ok()?.get_process()
    }

    pub fn get_thread_group(&self, pid: pid_t) -> Option<Arc<ThreadGroup>> {
        self.get(pid).ok()?.get_thread_group()
    }

    pub fn get_thread_groups(&self) -> Vec<Arc<ThreadGroup>> {
        let scope = RcuReadScope::new();
        self.table
            .values()
            .flat_map(|entry| {
                entry
                    .process
                    .as_ref(&scope)
                    .and_then(ProcessEntry::thread_group)
                    .and_then(|g| g.upgrade())
            })
            .collect()
    }

    /// Replace process with the specified `pid` with a zombie.
    pub fn kill_process(&mut self, pid: pid_t) {
        let entry = self.get_or_create_entry(pid);
        assert!(matches!(entry.process.read().as_deref(), Some(ProcessEntry::ThreadGroup(_))));

        // All tasks from the process are expected to be cleared from the table before the process
        // becomes a zombie. Cannot verify this for all tasks here, check it just for the leader.
        let scope = RcuReadScope::new();
        assert_eq!(entry.task.strong_count(&scope), 0);

        entry.process.update(Some(ProcessEntry::Zombie));
    }

    pub fn remove_zombie(&mut self, pid: pid_t) {
        self.remove_item(pid, |entry| {
            assert!(matches!(entry.process.read().as_deref(), Some(ProcessEntry::Zombie)));
            entry.process.update(None);
        });

        let scope = RcuReadScope::new();
        // Notify thread group changes.
        if let Some(notifier) = self.thread_group_notifier.as_ref(&scope) {
            let _ = notifier.send(MemoryAttributionLifecycleEvent::destruction(pid));
        }
    }

    pub fn get_process_group(&self, pid: pid_t) -> Option<Arc<ProcessGroup>> {
        self.get(pid).ok()?.get_process_group()
    }

    pub fn add_process_group(&self, process_group: &Arc<ProcessGroup>) {
        let scope = RcuReadScope::new();
        assert_eq!(process_group.leader.process_group.strong_count(&scope), 0);
        process_group.leader.process_group.update(Arc::downgrade(process_group));
    }

    pub fn remove_process_group(&self, leader: &Pid) {
        let scope = RcuReadScope::new();
        assert!(leader.process_group.strong_count(&scope) > 0);
        leader.process_group.update(Weak::new());
    }

    /// Returns the process ids for all processes, including zombies.
    pub fn process_ids(&self) -> Vec<pid_t> {
        let scope = RcuReadScope::new();
        self.table
            .iter()
            .flat_map(|(_, entry)| entry.process.is_some(&scope).then_some(entry.id))
            .collect()
    }

    /// Returns the task ids for all the currently running tasks.
    pub fn running_task_ids(&self) -> Vec<pid_t> {
        let scope = RcuReadScope::new();
        self.table
            .iter()
            .flat_map(|(_, entry)| (entry.task.strong_count(&scope) > 0).then_some(entry.id))
            .collect()
    }

    pub fn last_pid(&self) -> pid_t {
        self.last_pid
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }
}
