// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::terminal::{Terminal, TerminalSide};
use crate::ptrace::StopState;
use crate::signals::SignalInfo;
use crate::task::idr::{Idr, IdrGuard};
use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{Task, ThreadGroup, ThreadGroupLinkAdapter, ThreadGroupLinkNode};
use fuchsia_rcu::subtle::{RcuPtr, RcuPtrRef};
use fuchsia_rcu::{RcuDroppable, RcuOptionBox, RcuReadScope, RcuWeak, rcu_drop};
use fuchsia_rcu_collections::rcu_intrusive_list::RcuIntrusiveList;
use starnix_sync::PidTableLock;
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::Signal;
use starnix_uapi::{errno, error, pid_t};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Weak};

#[derive(Debug, RcuDroppable)]
enum ProcessEntry {
    ThreadGroup(Weak<ThreadGroup>),
    Zombie(Weak<Task>),
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
#[derive(Debug, RcuDroppable)]
pub struct PidEntry {
    pub id: pid_t,
    task: RcuWeak<Task>,
    process: RcuOptionBox<ProcessEntry>,
    pgid_thread_groups: RcuIntrusiveList<ThreadGroupLinkNode, ThreadGroupLinkAdapter>,
    sid_thread_groups: RcuIntrusiveList<ThreadGroupLinkNode, ThreadGroupLinkAdapter>,
}

impl PidEntry {
    pub fn get_task(&self) -> Result<Arc<Task>, Errno> {
        self.task.upgrade().ok_or_else(|| errno!(ESRCH))
    }

    /// Returns a representative [`Task`] for this process, including when the thread-group
    /// leader has exited or the process is an unreaped zombie.
    pub fn get_process_task(&self) -> Option<Arc<Task>> {
        if let Ok(task) = self.get_task() {
            return Some(task);
        }
        let process = self.process.read()?;
        match &*process {
            ProcessEntry::ThreadGroup(thread_group) => {
                if let Some(task) = thread_group.upgrade().and_then(|tg| tg.read().first_task()) {
                    return Some(task);
                }
                match &*self.process.read()? {
                    ProcessEntry::Zombie(task) => task.upgrade(),
                    ProcessEntry::ThreadGroup(_) => None,
                }
            }
            ProcessEntry::Zombie(task) => task.upgrade(),
        }
    }

    pub fn get_process(&self) -> Option<ProcessEntryRef> {
        let process = self.process.read()?;
        match &*process {
            ProcessEntry::ThreadGroup(thread_group) => {
                // Because `self.process` is read lock-free under RCU, the process may concurrently
                // transition to a zombie in `ThreadGroup::remove()` and drop its last strong
                // `Arc<ThreadGroup>` reference before `upgrade()` is called here.
                Some(match thread_group.upgrade() {
                    Some(thread_group) => ProcessEntryRef::Process(thread_group),
                    None => ProcessEntryRef::Zombie,
                })
            }
            ProcessEntry::Zombie(_) => Some(ProcessEntryRef::Zombie),
        }
    }

    pub fn get_thread_group(&self) -> Result<Arc<ThreadGroup>, Errno> {
        match self.get_process() {
            Some(ProcessEntryRef::Process(tg)) => Ok(tg),
            _ => error!(ESRCH),
        }
    }

    /// Returns whether any member thread groups belong to this process group (PGID).
    pub fn is_process_group(&self, scope: &RcuReadScope) -> bool {
        !self.pgid_thread_groups.is_empty(scope)
    }

    /// Returns whether any member thread groups belong to this session (SID).
    pub fn is_session(&self, scope: &RcuReadScope) -> bool {
        !self.sid_thread_groups.is_empty(scope)
    }

    /// Returns an iterator over the member thread groups in this process group (PGID).
    pub fn pgid_thread_groups<'a>(
        &'a self,
        scope: &'a RcuReadScope,
    ) -> impl Iterator<Item = Arc<ThreadGroup>> + 'a {
        self.pgid_thread_groups.iter(scope).filter_map(|n| n.thread_group.upgrade())
    }

    /// Returns an iterator over the member thread groups in this session (SID).
    pub fn sid_thread_groups<'a>(
        &'a self,
        scope: &'a RcuReadScope,
    ) -> impl Iterator<Item = Arc<ThreadGroup>> + 'a {
        self.sid_thread_groups.iter(scope).filter_map(|n| n.thread_group.upgrade())
    }

    /// Returns whether this process group (PGID) is non-empty and belongs to `session` (SID).
    pub fn is_process_group_in_session(&self, session: &PidEntry, scope: &RcuReadScope) -> bool {
        self.pgid_thread_groups(scope).next().is_some_and(|member_tg| {
            session.sid_thread_groups(scope).any(|tg| Arc::ptr_eq(&tg, &member_tg))
        })
    }

    /// Clears the controlling terminal on all member thread groups in this session (SID).
    ///
    /// If `matching` is `Some((terminal, maybe_side))`, only clears `controlling_terminal`
    /// if it refers to `terminal` (and matches `side` if specified).
    pub fn clear_controlling_terminal(&self, matching: Option<(&Terminal, Option<TerminalSide>)>) {
        let scope = RcuReadScope::new();
        for tg in self.sid_thread_groups(&scope) {
            let mut state = tg.write();
            if *state.session != *self {
                continue;
            }
            let should_clear = state.controlling_terminal.as_ref().is_some_and(|ct| {
                let matches = match matching {
                    Some((terminal, Some(side))) => ct.matches(terminal, side),
                    Some((terminal, None)) => std::ptr::eq(terminal, Arc::as_ptr(&ct.terminal)),
                    None => true,
                };
                matches && ct.terminal.read().controlling_session().map(Arc::as_ref) != Some(self)
            });
            if should_clear {
                state.controlling_terminal = None;
            }
        }
    }

    /// Returns whether the process group represented by this PidEntry is orphaned.
    ///
    /// An orphaned process group is one where every member's parent is either in the
    /// same process group or outside the group's session.
    ///
    /// If `ignored_tg` is supplied, that thread group is ignored during traversal.
    pub fn will_become_orphaned_pgrp(
        &self,
        scope: &RcuReadScope,
        ignored_tg: Option<&ThreadGroup>,
    ) -> bool {
        for tg in self.pgid_thread_groups(scope) {
            if ignored_tg.is_some_and(|ignored| std::ptr::eq(tg.as_ref(), ignored)) {
                continue;
            }

            let (parent_tg, my_sid) = {
                let tg_state = tg.read();
                if !tg_state.is_running() {
                    continue;
                }
                (tg_state.parent.as_ref().map(|p| p.upgrade()), tg_state.session.id)
            };

            let Some(parent_tg) = parent_tg else {
                continue;
            };

            // Skip the parent if it matches ignored_tg.
            if ignored_tg.is_some_and(|ignored| std::ptr::eq(parent_tg.as_ref(), ignored)) {
                continue;
            }

            // Init (PID 1) does not act as a job-controlling parent for reparented tasks.
            if parent_tg.leader.id == 1 {
                continue;
            }

            let (parent_pgid, parent_sid) = {
                let parent_state = parent_tg.read();
                if !parent_state.is_running() {
                    continue;
                }
                (parent_tg.pgid().id, parent_state.session.id)
            };

            // If a member has a parent in the same session but outside this process group,
            // that parent handles job control. The group is NOT orphaned.
            if parent_pgid != self.id && parent_sid == my_sid {
                return false;
            }
        }

        true
    }

    /// Returns whether any member in this process group is stopped by job control.
    pub fn has_stopped_jobs(&self, scope: &RcuReadScope) -> bool {
        for tg in self.pgid_thread_groups(scope) {
            let stop_state = tg.load_stopped();
            if matches!(stop_state, StopState::GroupStopping | StopState::GroupStopped)
                && tg.read().is_running()
            {
                return true;
            }
        }
        false
    }

    /// Dispatches signals to all member thread groups in this process group.
    ///
    /// Signals are delivered in order: each signal in `signals` is delivered to all
    /// group members before the subsequent signal is dispatched.
    pub fn send_signals_to_pgid(&self, signals: &[Signal]) {
        let scope = RcuReadScope::new();
        for &signal in signals {
            for tg in self.pgid_thread_groups(&scope) {
                let state = tg.write();
                if !state.is_exited() {
                    state.send_signal(SignalInfo::kernel(signal));
                }
            }
        }
    }

    /// Attaches `thread_group` to `list` using `node_slot` to store the allocated link node.
    ///
    /// Allocates a fresh [`ThreadGroupLinkNode`] for this list attachment so moving a process
    /// across process groups or sessions never mutates a node still visible to in-flight RCU
    /// readers.
    fn attach_thread_group_node(
        _guard: &PidTableGuard<'_>,
        list: &RcuIntrusiveList<ThreadGroupLinkNode, ThreadGroupLinkAdapter>,
        node_slot: &RcuPtr<ThreadGroupLinkNode>,
        thread_group: &Arc<ThreadGroup>,
    ) {
        let scope = RcuReadScope::new();
        debug_assert!(node_slot.read(&scope).is_null());
        let node_ptr =
            Box::into_raw(Box::new(ThreadGroupLinkNode::new(Arc::downgrade(thread_group))));
        node_slot.assign(node_ptr);
        // SAFETY: Single-writer exclusion is guaranteed by `_guard`, and `node_ptr` is newly
        // allocated for this list attachment and retired via `rcu_drop` upon detach.
        unsafe {
            list.push_back(&scope, RcuPtrRef::new(&scope, node_ptr));
        }
    }

    /// Detaches the node stored in `node_slot` from `list`, returning whether `list` is now empty.
    ///
    /// Unlinks the node (leaving `link.next` intact for concurrent RCU readers) and schedules
    /// deferred reclamation via [`rcu_drop`].
    fn detach_thread_group_node(
        _guard: &PidTableGuard<'_>,
        list: &RcuIntrusiveList<ThreadGroupLinkNode, ThreadGroupLinkAdapter>,
        node_slot: &RcuPtr<ThreadGroupLinkNode>,
    ) -> bool {
        let scope = RcuReadScope::new();
        let node_ptr = node_slot.read(&scope);
        if !node_ptr.is_null() {
            node_slot.assign(std::ptr::null_mut());
            // SAFETY: Single-writer exclusion is guaranteed by `_guard`. `remove` leaves
            // `node.link.next` intact for concurrent RCU readers, and `node_ptr` is never reused.
            unsafe {
                list.remove(&scope, node_ptr);
            }
            // SAFETY: `node_ptr` was allocated via `Box::into_raw` in `attach_thread_group_node`
            // and is detached at most once under `PidTableLock`.
            rcu_drop(unsafe { Box::from_raw(node_ptr.as_mut_ptr()) });
        }
        list.is_empty(&scope)
    }
    /// Attaches a member thread group to this process group (PGID).
    fn attach_pgid(&self, guard: &PidTableGuard<'_>, thread_group: &Arc<ThreadGroup>) {
        Self::attach_thread_group_node(
            guard,
            &self.pgid_thread_groups,
            &thread_group.pgrp_node,
            thread_group,
        );
    }

    /// Detaches a member thread group from this process group (PGID).
    fn detach_pgid(&self, guard: &PidTableGuard<'_>, thread_group: &ThreadGroup) -> bool {
        Self::detach_thread_group_node(guard, &self.pgid_thread_groups, &thread_group.pgrp_node)
    }

    /// Attaches a member thread group to this session (SID).
    fn attach_sid(&self, guard: &PidTableGuard<'_>, thread_group: &Arc<ThreadGroup>) {
        Self::attach_thread_group_node(
            guard,
            &self.sid_thread_groups,
            &thread_group.session_node,
            thread_group,
        );
    }

    /// Detaches a member thread group from this session (SID).
    fn detach_sid(&self, guard: &PidTableGuard<'_>, thread_group: &ThreadGroup) -> bool {
        Self::detach_thread_group_node(guard, &self.sid_thread_groups, &thread_group.session_node)
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
            pgid_thread_groups: Default::default(),
            sid_thread_groups: Default::default(),
        }
    }

    fn is_empty(&self, scope: &RcuReadScope) -> bool {
        self.task.strong_count(scope) == 0
            && self.process.is_none(scope)
            && self.pgid_thread_groups.is_empty(scope)
            && self.sid_thread_groups.is_empty(scope)
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

impl PartialOrd for PidEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PidEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self as *const Self).cmp(&(other as *const Self))
    }
}

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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskEntryScope {
    Task,
    ThreadGroup,
}

/// Identifies the target process or thread for a `/proc/<pid>` or `/proc/<pid>/task/<tid>` node.
#[derive(Clone, Debug)]
pub struct TaskContainer {
    pub pid: Pid,
    pub tid: Pid,
    pub scope: TaskEntryScope,
}

impl TaskContainer {
    pub fn from_task(task: &Task) -> Self {
        Self { pid: task.pid.clone(), tid: task.tid.clone(), scope: TaskEntryScope::Task }
    }

    pub fn from_pid(pid: Pid) -> Self {
        Self { tid: pid.clone(), pid, scope: TaskEntryScope::ThreadGroup }
    }

    pub fn from_thread_group(pid: Pid, tid: Pid) -> Self {
        Self { pid, tid, scope: TaskEntryScope::ThreadGroup }
    }

    pub fn get_task(&self) -> Result<Arc<Task>, Errno> {
        if let Ok(task) = self.tid.get_task() {
            return Ok(task);
        }
        if self.scope == TaskEntryScope::ThreadGroup && self.tid == self.pid {
            return self.pid.get_process_task().ok_or_else(|| errno!(ESRCH));
        }
        error!(ESRCH)
    }
}

/// The number of reserved PIDs in Linux. When wrapping around, PID allocation restarts at this value.
pub const RESERVED_PIDS: u32 = 300;

/// The default maximal PID considered in Linux.
pub const PID_MAX_DEFAULT: pid_t = 1 << 15;

/// The maximal PID considered in Linux.
pub const PID_MAX_LIMIT: pid_t = 1 << 22;

/// The default number of PIDs per CPU used to scale pid_max.
pub const PIDS_PER_CPU_DEFAULT: pid_t = 1024;

/// Returns the actual PID limit given a requested limit, scaled by the system's CPU count.
fn actual_pid_limit(limit: pid_t) -> pid_t {
    actual_pid_limit_with_cpus(limit, zx::system_get_num_cpus())
}

/// Returns the actual PID limit given a requested limit and the number of CPUs.
fn actual_pid_limit_with_cpus(limit: pid_t, num_cpus: u32) -> pid_t {
    let cpu_limit = (num_cpus as pid_t).saturating_mul(PIDS_PER_CPU_DEFAULT);
    limit.max(cpu_limit).min(PID_MAX_LIMIT)
}

pub struct PidTable {
    /// The most-recently allocated pid in this table.
    last_pid: AtomicI32,

    /// The tasks in this table, organized by pid_t using an IDR radix tree.
    idr: Idr<PidEntry, PidTableLock>,

    /// Used to notify thread group changes.
    thread_group_notifier: RcuOptionBox<std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>>,
}

impl Default for PidTable {
    fn default() -> Self {
        let idr = Idr::new_cyclic(Some(RESERVED_PIDS));
        idr.set_max(actual_pid_limit(PID_MAX_DEFAULT) as u32);
        idr.lock().reserve_id(0);
        Self { last_pid: AtomicI32::new(0), idr, thread_group_notifier: Default::default() }
    }
}

impl std::fmt::Debug for PidTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PidTable")
            .field("last_pid", &self.last_pid.load(Ordering::Relaxed))
            .finish()
    }
}

/// An RAII guard representing exclusive writer access to a [`PidTable`].
///
/// Holding this guard serializes mutations (allocations, reservations, removals)
/// to the PID table while allowing concurrent lock-free reads.
pub struct PidTableGuard<'a> {
    table: &'a PidTable,
    idr: IdrGuard<'a, PidEntry, PidTableLock>,
}

impl<'a> std::ops::Deref for PidTableGuard<'a> {
    type Target = PidTable;

    fn deref(&self) -> &Self::Target {
        self.table
    }
}

impl<'a> PidTableGuard<'a> {
    /// Allocates a new PID, returning an error if the PID table is full.
    pub fn allocate_pid(&mut self) -> Result<Pid, Errno> {
        self.idr
            .alloc(|id| Arc::new(PidEntry::new(id as pid_t)))
            .map(|(_, pid)| pid)
            .ok_or_else(|| errno!(EAGAIN))
    }

    pub fn add_task(&mut self, task: Arc<Task>) {
        let scope = RcuReadScope::new();
        let entry =
            self.idr.lookup(task.tid.id as u32, &scope).expect("task.tid should be in pid table");
        assert_eq!(entry.task.strong_count(&scope), 0);
        if task.is_leader() {
            assert!(entry.process.is_none(&scope));
            self.table.last_pid.store(task.tid.id, Ordering::Relaxed);
            // Publish process before task so lock-free RCU readers that look up a task
            // first (e.g. /proc/<pid> lookups) always observe its process entry populated
            // for a leader. Readers that check process first and fall back to task (such as
            // new_pidfd) rely on task.is_leader() to handle concurrent initialization/reaping.
            entry
                .process
                .update(Some(ProcessEntry::ThreadGroup(Arc::downgrade(task.thread_group()))));
        }
        entry.task.update(Arc::downgrade(&task));

        if task.is_leader() {
            // Notify thread group changes.
            if let Some(notifier) = self.table.thread_group_notifier.as_ref(&scope) {
                let mut tg_state = task.thread_group.write();
                let _ = notifier.send(MemoryAttributionLifecycleEvent::creation(task.tid.id));
                tg_state.notifier = Some(notifier.clone());
            }
        }
    }

    fn remove_item<F>(&mut self, pid: &Pid, do_remove: F)
    where
        F: FnOnce(&PidEntry),
    {
        let scope = RcuReadScope::new();
        debug_assert_eq!(self.idr.lookup(pid.id as u32, &scope).as_ref(), Some(pid));
        do_remove(pid);
        if pid.is_empty(&scope) {
            self.idr.remove(pid.id as u32);
        }
    }

    pub fn remove_task(&mut self, tid: &Pid) {
        self.remove_item(tid, |entry| {
            let scope = RcuReadScope::new();
            assert!(entry.task.strong_count(&scope) > 0);
            entry.task.update(Weak::new());
        });
    }

    /// Replace process with the specified `pid` with a zombie.
    pub fn kill_process(&mut self, pid: &Pid, zombie_task: Weak<Task>) {
        let scope = RcuReadScope::new();
        debug_assert_eq!(self.idr.lookup(pid.id as u32, &scope).as_ref(), Some(pid));
        assert!(matches!(pid.process.read().as_deref(), Some(ProcessEntry::ThreadGroup(_))));

        pid.process.update(Some(ProcessEntry::Zombie(zombie_task)));
    }

    pub fn remove_zombie(&mut self, pid: &Pid) {
        let scope = RcuReadScope::new();

        self.remove_item(pid, |entry| {
            assert!(matches!(entry.process.read().as_deref(), Some(ProcessEntry::Zombie(_))));
            entry.process.update(None);
        });

        // Notify thread group changes.
        if let Some(notifier) = self.table.thread_group_notifier.as_ref(&scope) {
            let _ = notifier.send(MemoryAttributionLifecycleEvent::destruction(pid.id));
        }
    }

    pub fn attach_pgid(&self, pid: &PidEntry, thread_group: &Arc<ThreadGroup>) {
        pid.attach_pgid(self, thread_group);
    }

    pub fn detach_pgid(&mut self, pid: &Pid, thread_group: &ThreadGroup) -> bool {
        let scope = RcuReadScope::new();
        let is_empty = pid.detach_pgid(self, thread_group);
        if pid.is_empty(&scope) && self.idr.lookup(pid.id as u32, &scope).as_ref() == Some(pid) {
            self.idr.remove(pid.id as u32);
        }
        is_empty
    }

    pub fn attach_sid(&self, pid: &PidEntry, thread_group: &Arc<ThreadGroup>) {
        pid.attach_sid(self, thread_group);
    }

    pub fn detach_sid(&mut self, pid: &Pid, thread_group: &ThreadGroup) -> bool {
        let scope = RcuReadScope::new();
        let is_empty = pid.detach_sid(self, thread_group);
        if pid.is_empty(&scope) && self.idr.lookup(pid.id as u32, &scope).as_ref() == Some(pid) {
            self.idr.remove(pid.id as u32);
        }
        is_empty
    }
}

impl PidTable {
    /// Acquires the lock for mutations, returning a [`PidTableGuard`].
    pub fn lock(&self) -> PidTableGuard<'_> {
        PidTableGuard { table: self, idr: self.idr.lock() }
    }

    pub fn get(&self, pid: pid_t) -> Result<Pid, Errno> {
        if pid <= 0 {
            return error!(ESRCH);
        }
        let scope = RcuReadScope::new();
        self.idr.lookup(pid as u32, &scope).ok_or_else(|| errno!(ESRCH))
    }

    pub fn set_thread_group_notifier(
        &self,
        notifier: std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>,
    ) {
        self.thread_group_notifier.update(Some(notifier));
    }

    pub fn get_thread_groups<'a>(
        &'a self,
        scope: &'a RcuReadScope,
    ) -> impl Iterator<Item = Arc<ThreadGroup>> + 'a {
        self.idr.iter(&scope).flat_map(move |(_, entry)| {
            entry
                .process
                .as_ref(&scope)
                .and_then(ProcessEntry::thread_group)
                .and_then(|g| g.upgrade())
        })
    }

    /// Returns the process ids for all processes, including zombies.
    pub fn process_ids(&self) -> Vec<pid_t> {
        let scope = RcuReadScope::new();
        self.idr
            .iter(&scope)
            .flat_map(|(_, entry)| entry.process.is_some(&scope).then_some(entry.id))
            .collect()
    }

    /// Returns an iterator over the [`Pid`]s for all the currently running tasks.
    pub fn running_task_ids<'a>(
        &'a self,
        scope: &'a RcuReadScope,
    ) -> impl Iterator<Item = &'a Pid> {
        self.idr
            .iter(scope)
            .map(|(_, entry)| entry)
            .filter(|entry| entry.task.strong_count(scope) > 0)
    }

    pub fn last_pid(&self) -> pid_t {
        self.last_pid.load(Ordering::Relaxed)
    }

    /// Returns the maximal PID value allowed for allocation.
    pub fn max(&self) -> pid_t {
        self.idr.max() as pid_t
    }

    /// Sets the maximal PID value allowed for allocation.
    pub fn set_max(&self, max: pid_t) {
        self.idr.set_max(actual_pid_limit(max) as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::ExitStatus;
    use crate::testing::spawn_kernel_and_run;
    use starnix_types::ownership::Releasable;
    use starnix_uapi::signals::{SIGCHLD, SIGCONT};
    use starnix_uapi::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_pid_table_allocation() {
        let table = PidTable::default();
        let pid1 = table.lock().allocate_pid().unwrap();
        assert_eq!(pid1.id, 1);

        let pid2 = table.lock().allocate_pid().unwrap();
        assert_eq!(pid2.id, 2);

        assert_eq!(table.get(1).unwrap().id, 1);
        assert_eq!(table.get(2).unwrap().id, 2);
        assert!(table.get(0).is_err());
        assert!(table.get(-1).is_err());
        assert!(table.get(3).is_err());
    }

    #[test]
    fn test_pid_table_lock_guard() {
        let table = PidTable::default();
        let pid1 = {
            let mut guard = table.lock();
            let pid = guard.allocate_pid().unwrap();
            assert_eq!(pid.id, 1);
            pid
        };
        assert_eq!(table.get(1).unwrap(), pid1);
    }

    #[test]
    fn test_pid_table_empty_state() {
        let table = PidTable::default();
        assert_eq!(table.last_pid(), 0);
        assert_eq!(table.process_ids().len(), 0);
        let scope = RcuReadScope::new();
        assert_eq!(table.running_task_ids(&scope).count(), 0);
    }

    #[test]
    fn test_pid_table_max_and_wrap() {
        let table = PidTable::default();
        assert_eq!(table.max(), actual_pid_limit(PID_MAX_DEFAULT));

        // Allocate up to the max value (1..=max).
        let max = table.max();
        for expected in 1..=max {
            let pid = table.lock().allocate_pid().unwrap();
            assert_eq!(pid.id, expected);
        }

        // The table is full up to max. Allocation fails.
        assert_eq!(table.lock().allocate_pid().unwrap_err(), errno!(EAGAIN));

        // Free PID 2 (below RESERVED_PIDS) and PID 302 (at or above RESERVED_PIDS).
        table.idr.lock().remove(2);
        table.idr.lock().remove(302);

        // Next allocation wraps to RESERVED_PIDS (300) and allocates slot 302.
        // Slot 2 is skipped because wrapping only allocates IDs >= RESERVED_PIDS.
        let pid = table.lock().allocate_pid().unwrap();
        assert_eq!(pid.id, 302);

        // Now all slots >= RESERVED_PIDS are occupied, so allocation fails even though slot 2 is free.
        assert_eq!(table.lock().allocate_pid().unwrap_err(), errno!(EAGAIN));

        // Free PID 300. Allocation should reuse it.
        table.idr.lock().remove(300);
        let pid = table.lock().allocate_pid().unwrap();
        assert_eq!(pid.id, 300);

        // Increase max and verify new PIDs can be allocated.
        let new_max = max + 10;
        table.set_max(new_max);
        assert_eq!(table.max(), new_max);
        let pid = table.lock().allocate_pid().unwrap();
        assert_eq!(pid.id, max + 1);
    }

    #[test]
    fn test_actual_pid_limit() {
        // With 1 CPU, limit scales to at least 1024, but PID_MAX_DEFAULT is 32768.
        assert_eq!(actual_pid_limit_with_cpus(PID_MAX_DEFAULT, 1), PID_MAX_DEFAULT);
        assert_eq!(actual_pid_limit_with_cpus(500, 1), 1024);

        // With 64 CPUs, cpu_limit is 65536 > PID_MAX_DEFAULT.
        assert_eq!(actual_pid_limit_with_cpus(PID_MAX_DEFAULT, 64), 65536);
        assert_eq!(actual_pid_limit_with_cpus(100_000, 64), 100_000);

        // Capped at PID_MAX_LIMIT.
        assert_eq!(actual_pid_limit_with_cpus(PID_MAX_LIMIT + 1000, 1), PID_MAX_LIMIT);
        assert_eq!(actual_pid_limit_with_cpus(PID_MAX_DEFAULT, 10_000), PID_MAX_LIMIT);
    }

    #[test]
    fn test_get_process_after_thread_group_release() {
        // `get_process()` reads `process` lock-free under RCU, so it can observe a
        // `ThreadGroup` entry that `kill_process()` is concurrently replacing with `Zombie`
        // and upgrade the `Weak` only after `CurrentTask::exit()` has dropped the last
        // strong reference. `Weak::new()` never upgrades, modelling that released state.
        let pid = PidEntry::new_for_test(1);
        pid.process.update(Some(ProcessEntry::ThreadGroup(Weak::new())));

        assert!(matches!(pid.get_process(), Some(ProcessEntryRef::Zombie)));
    }

    #[::fuchsia::test]
    async fn test_pid_table_last_pid_on_thread_group() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let initial_last_pid = kernel.pids.last_pid();

            // Allocating a PID directly must not update last_pid.
            let _allocated = kernel.pids.lock().allocate_pid().unwrap();
            assert_eq!(kernel.pids.last_pid(), initial_last_pid);

            // Cloning a thread in the same thread group must not update last_pid.
            let _thread = current_task.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );
            assert_eq!(kernel.pids.last_pid(), initial_last_pid);

            // Cloning a new process (thread group leader) must update last_pid.
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(kernel.pids.last_pid(), child.get_pid());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_intrusive_lists() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            let scope = RcuReadScope::new();

            assert!(!pid_entry.is_process_group(&scope));
            assert!(!pid_entry.is_session(&scope));

            // Detach from initial group/session and attach to pid_entry.
            let guard = kernel.pids.lock();
            current_pid.detach_pgid(&guard, current_task.thread_group());
            current_pid.detach_sid(&guard, current_task.thread_group());
            pid_entry.attach_pgid(&guard, current_task.thread_group());
            pid_entry.attach_sid(&guard, current_task.thread_group());

            // Verify PGID traversal.
            let pgid_members: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(pgid_members, vec![current_task.get_pid()]);

            // Verify SID traversal.
            let sid_members: Vec<_> =
                pid_entry.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(sid_members, vec![current_task.get_pid()]);

            // Detach and restore to current_pid.
            let pgid_empty = pid_entry.detach_pgid(&guard, current_task.thread_group());
            assert!(pgid_empty);
            let sid_empty = pid_entry.detach_sid(&guard, current_task.thread_group());
            assert!(sid_empty);
            current_pid.attach_pgid(&guard, current_task.thread_group());
            current_pid.attach_sid(&guard, current_task.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_empty_traversal() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            // Verify empty traversals report no membership and return empty iterators.
            assert!(!pid_entry.is_process_group(&scope));
            assert!(!pid_entry.is_session(&scope));
            assert!(pid_entry.pgid_thread_groups(&scope).next().is_none());
            assert!(pid_entry.sid_thread_groups(&scope).next().is_none());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_multiple_tasks_ordered_and_reverse_detach() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());
            current_pid.detach_pgid(&guard, child2.thread_group());
            current_pid.detach_pgid(&guard, child3.thread_group());

            pid_entry.attach_pgid(&guard, child1.thread_group());
            pid_entry.attach_pgid(&guard, child2.thread_group());
            pid_entry.attach_pgid(&guard, child3.thread_group());

            let initial: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(initial, vec![child1.get_pid(), child2.get_pid(), child3.get_pid()]);

            // Detach tail first (child3).
            let is_empty = pid_entry.detach_pgid(&guard, child3.thread_group());
            assert!(!is_empty);
            let state1: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state1, vec![child1.get_pid(), child2.get_pid()]);

            // Detach next tail (child2).
            let is_empty = pid_entry.detach_pgid(&guard, child2.thread_group());
            assert!(!is_empty);
            let state2: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state2, vec![child1.get_pid()]);

            // Detach final remaining member (child1).
            let is_empty = pid_entry.detach_pgid(&guard, child1.thread_group());
            assert!(is_empty);
            assert!(pid_entry.pgid_thread_groups(&scope).next().is_none());

            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_head_detach() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());
            current_pid.detach_pgid(&guard, child2.thread_group());
            current_pid.detach_pgid(&guard, child3.thread_group());

            pid_entry.attach_pgid(&guard, child1.thread_group());
            pid_entry.attach_pgid(&guard, child2.thread_group());
            pid_entry.attach_pgid(&guard, child3.thread_group());

            // Detach head (child1).
            let is_empty = pid_entry.detach_pgid(&guard, child1.thread_group());
            assert!(!is_empty);
            let state1: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state1, vec![child2.get_pid(), child3.get_pid()]);

            // Detach new head (child2).
            let is_empty = pid_entry.detach_pgid(&guard, child2.thread_group());
            assert!(!is_empty);
            let state2: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state2, vec![child3.get_pid()]);

            // Detach final head (child3).
            let is_empty = pid_entry.detach_pgid(&guard, child3.thread_group());
            assert!(is_empty);
            assert!(pid_entry.pgid_thread_groups(&scope).next().is_none());

            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_middle_detach() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());
            current_pid.detach_pgid(&guard, child2.thread_group());
            current_pid.detach_pgid(&guard, child3.thread_group());

            pid_entry.attach_pgid(&guard, child1.thread_group());
            pid_entry.attach_pgid(&guard, child2.thread_group());
            pid_entry.attach_pgid(&guard, child3.thread_group());

            // Detach middle element (child2).
            let is_empty = pid_entry.detach_pgid(&guard, child2.thread_group());
            assert!(!is_empty);
            let state1: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state1, vec![child1.get_pid(), child3.get_pid()]);

            // Detach child3.
            let is_empty = pid_entry.detach_pgid(&guard, child3.thread_group());
            assert!(!is_empty);
            let state2: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state2, vec![child1.get_pid()]);

            // Detach child1.
            let is_empty = pid_entry.detach_pgid(&guard, child1.thread_group());
            assert!(is_empty);
            assert!(pid_entry.pgid_thread_groups(&scope).next().is_none());

            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_table_reclaims_pid_when_last_pgid_and_sid_task_detaches() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Create a session/process-group leader.
            let leader = current_task.clone_task_for_test(0, Some(SIGCHLD));
            leader.thread_group().setsid().expect("setsid leader");
            let leader_pid_id = leader.get_pid();

            // Create a member process in the leader's session and process group.
            let member = leader.clone_task_for_test(0, Some(SIGCHLD));

            // Kill and drop the leader, then reap its zombie from current_task.
            leader.thread_group().kill(ExitStatus::Exit(0), None);
            drop(leader);
            {
                let mut pids = kernel.pids.lock();
                let zombie = current_task
                    .thread_group()
                    .write()
                    .zombie_children
                    .pop()
                    .expect("leader zombie");
                zombie.release(&mut pids);
            }

            // Leader's PID must still exist in PidTable while member references it via pgid/sid.
            assert!(kernel.pids.get(leader_pid_id).is_ok());

            // Move member into its own session and process group.
            member.thread_group().setsid().expect("setsid member");

            // Now leader's PidEntry is completely empty and must be reclaimed from PidTable.
            assert_eq!(kernel.pids.get(leader_pid_id), error!(ESRCH));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_reattach_after_detach() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());
            current_pid.detach_pgid(&guard, child2.thread_group());
            current_pid.detach_pgid(&guard, child3.thread_group());

            pid_entry.attach_pgid(&guard, child1.thread_group());
            pid_entry.attach_pgid(&guard, child2.thread_group());
            pid_entry.attach_pgid(&guard, child3.thread_group());

            // Detach middle member child2.
            let is_empty = pid_entry.detach_pgid(&guard, child2.thread_group());
            assert!(!is_empty);
            let after_detach: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(after_detach, vec![child1.get_pid(), child3.get_pid()]);

            // Re-attach child2 to the same pid_entry.
            pid_entry.attach_pgid(&guard, child2.thread_group());

            // Guard against cycle or infinite loop with take(10).
            let reattached: Vec<_> =
                pid_entry.pgid_thread_groups(&scope).take(10).map(|tg| tg.leader.id).collect();
            assert_eq!(reattached, vec![child1.get_pid(), child3.get_pid(), child2.get_pid()]);

            pid_entry.detach_pgid(&guard, child1.thread_group());
            pid_entry.detach_pgid(&guard, child3.thread_group());
            pid_entry.detach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_move_between_groups() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let (pid_entry1, pid_entry2) = {
                let mut guard = kernel.pids.lock();
                (guard.allocate_pid().unwrap(), guard.allocate_pid().unwrap())
            };

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let scope = RcuReadScope::new();

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());
            current_pid.detach_pgid(&guard, child2.thread_group());
            current_pid.detach_pgid(&guard, child3.thread_group());

            pid_entry1.attach_pgid(&guard, child1.thread_group());
            pid_entry1.attach_pgid(&guard, child2.thread_group());
            pid_entry1.attach_pgid(&guard, child3.thread_group());

            // Move child2 from group 1 to group 2.
            pid_entry1.detach_pgid(&guard, child2.thread_group());
            pid_entry2.attach_pgid(&guard, child2.thread_group());

            let group1_members: Vec<_> =
                pid_entry1.pgid_thread_groups(&scope).take(10).map(|tg| tg.leader.id).collect();
            assert_eq!(group1_members, vec![child1.get_pid(), child3.get_pid()]);

            let group2_members: Vec<_> =
                pid_entry2.pgid_thread_groups(&scope).take(10).map(|tg| tg.leader.id).collect();
            assert_eq!(group2_members, vec![child2.get_pid()]);

            pid_entry1.detach_pgid(&guard, child1.thread_group());
            pid_entry1.detach_pgid(&guard, child3.thread_group());
            pid_entry2.detach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_singleton_reattach_and_detach() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_pgid(&guard, child1.thread_group());

            // Attach singleton.
            pid_entry.attach_pgid(&guard, child1.thread_group());
            assert_eq!(
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect::<Vec<_>>(),
                vec![child1.get_pid()]
            );

            // Detach singleton.
            let is_empty = pid_entry.detach_pgid(&guard, child1.thread_group());
            assert!(is_empty);

            // Re-attach singleton.
            pid_entry.attach_pgid(&guard, child1.thread_group());
            assert_eq!(
                pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect::<Vec<_>>(),
                vec![child1.get_pid()]
            );

            // Detach singleton again and restore to current_pid.
            let is_empty = pid_entry.detach_pgid(&guard, child1.thread_group());
            assert!(is_empty);
            current_pid.attach_pgid(&guard, child1.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_sid_tasks_adversarial() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let (pid_entry1, pid_entry2) = {
                let mut guard = kernel.pids.lock();
                (guard.allocate_pid().unwrap(), guard.allocate_pid().unwrap())
            };
            let scope = RcuReadScope::new();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.detach_sid(&guard, child1.thread_group());
            current_pid.detach_sid(&guard, child2.thread_group());
            current_pid.detach_sid(&guard, child3.thread_group());

            pid_entry1.attach_sid(&guard, child1.thread_group());
            pid_entry1.attach_sid(&guard, child2.thread_group());
            pid_entry1.attach_sid(&guard, child3.thread_group());

            // Verify ordered traversal.
            let initial: Vec<_> =
                pid_entry1.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(initial, vec![child1.get_pid(), child2.get_pid(), child3.get_pid()]);

            // Detach middle element (child2).
            let is_empty = pid_entry1.detach_sid(&guard, child2.thread_group());
            assert!(!is_empty);
            let state1: Vec<_> =
                pid_entry1.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(state1, vec![child1.get_pid(), child3.get_pid()]);

            // Re-attach child2 to pid_entry2 (move between sessions).
            pid_entry2.attach_sid(&guard, child2.thread_group());
            let session1_members: Vec<_> =
                pid_entry1.sid_thread_groups(&scope).take(10).map(|tg| tg.leader.id).collect();
            let session2_members: Vec<_> =
                pid_entry2.sid_thread_groups(&scope).take(10).map(|tg| tg.leader.id).collect();
            assert_eq!(session1_members, vec![child1.get_pid(), child3.get_pid()]);
            assert_eq!(session2_members, vec![child2.get_pid()]);

            // Detach remaining elements and restore to current_pid.
            assert!(!pid_entry1.detach_sid(&guard, child3.thread_group()));
            assert!(pid_entry1.detach_sid(&guard, child1.thread_group()));
            assert!(pid_entry1.sid_thread_groups(&scope).next().is_none());
            assert!(pid_entry2.detach_sid(&guard, child2.thread_group()));

            current_pid.attach_sid(&guard, child1.thread_group());
            current_pid.attach_sid(&guard, child2.thread_group());
            current_pid.attach_sid(&guard, child3.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_signal_delivery_under_pid_table_lock() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            {
                let guard = kernel.pids.lock();
                let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
                current_pid.detach_pgid(&guard, child1.thread_group());
                current_pid.detach_pgid(&guard, child2.thread_group());

                pid_entry.attach_pgid(&guard, child1.thread_group());
                pid_entry.attach_pgid(&guard, child2.thread_group());

                // Send signals while holding PidTableLock to verify that lock acquisition
                // order PidTableLock => ThreadGroupMutableStateLock succeeds without deadlock.
                pid_entry.send_signals_to_pgid(&[SIGCONT]);
            }

            // Send signals without holding PidTableLock.
            pid_entry.send_signals_to_pgid(&[SIGCONT]);

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            pid_entry.detach_pgid(&guard, child1.thread_group());
            pid_entry.detach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pid_entry_concurrent_rcu_traversal_under_mutation() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let pid_entry = kernel.pids.lock().allocate_pid().unwrap();

            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child3 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            {
                let guard = kernel.pids.lock();
                let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
                current_pid.detach_pgid(&guard, child1.thread_group());
                current_pid.detach_pgid(&guard, child2.thread_group());
                current_pid.detach_pgid(&guard, child3.thread_group());
                current_pid.detach_sid(&guard, child1.thread_group());
                current_pid.detach_sid(&guard, child2.thread_group());
                current_pid.detach_sid(&guard, child3.thread_group());
            }

            let valid_pids: HashSet<i32> =
                [child1.get_pid(), child2.get_pid(), child3.get_pid()].into_iter().collect();

            let running = Arc::new(AtomicBool::new(true));

            // Spawn concurrent reader thread 1: iterates pgid_thread_groups.
            let r1_pid = pid_entry.clone();
            let r1_running = Arc::clone(&running);
            let r1_pids = valid_pids.clone();
            let reader1 = std::thread::spawn(move || {
                while r1_running.load(Ordering::Relaxed) {
                    let scope = RcuReadScope::new();
                    let mut count = 0;
                    for tg in r1_pid.pgid_thread_groups(&scope) {
                        count += 1;
                        assert!(r1_pids.contains(&tg.leader.id));
                        assert!(
                            count <= 3,
                            "Infinite loop or cycle detected in pgid_thread_groups"
                        );
                    }
                }
            });

            // Spawn concurrent reader thread 2: iterates sid_thread_groups.
            let r2_pid = pid_entry.clone();
            let r2_running = Arc::clone(&running);
            let r2_pids = valid_pids.clone();
            let reader2 = std::thread::spawn(move || {
                while r2_running.load(Ordering::Relaxed) {
                    let scope = RcuReadScope::new();
                    let tgs: Vec<_> = r2_pid.sid_thread_groups(&scope).take(10).collect();
                    assert!(tgs.len() <= 3, "Infinite loop or cycle detected in sid_thread_groups");
                    for tg in tgs {
                        assert!(r2_pids.contains(&tg.leader.id));
                    }
                }
            });

            // Spawn concurrent signal delivery thread: delivers signals to process group.
            let r3_pid = pid_entry.clone();
            let r3_running = Arc::clone(&running);
            let signal_thread = std::thread::spawn(move || {
                while r3_running.load(Ordering::Relaxed) {
                    r3_pid.send_signals_to_pgid(&[SIGCONT]);
                }
            });

            // Writer thread mutates pgid_thread_groups and sid_thread_groups under PidTableLock.
            for _ in 0..150 {
                {
                    let guard = kernel.pids.lock();
                    pid_entry.attach_pgid(&guard, child1.thread_group());
                    pid_entry.attach_pgid(&guard, child2.thread_group());
                    pid_entry.attach_sid(&guard, child2.thread_group());
                    pid_entry.attach_sid(&guard, child3.thread_group());
                }

                {
                    let guard = kernel.pids.lock();
                    pid_entry.detach_pgid(&guard, child2.thread_group());
                    pid_entry.attach_pgid(&guard, child3.thread_group());
                    pid_entry.detach_sid(&guard, child2.thread_group());
                }

                {
                    let guard = kernel.pids.lock();
                    pid_entry.detach_pgid(&guard, child1.thread_group());
                    pid_entry.detach_pgid(&guard, child3.thread_group());
                    pid_entry.detach_sid(&guard, child3.thread_group());
                }
            }

            running.store(false, Ordering::Relaxed);
            reader1.join().expect("Reader 1 join failed");
            reader2.join().expect("Reader 2 join failed");
            signal_thread.join().expect("Signal thread join failed");

            // Final cleanup check and restore to current_pid.
            let scope = RcuReadScope::new();
            assert!(pid_entry.pgid_thread_groups(&scope).next().is_none());
            assert!(pid_entry.sid_thread_groups(&scope).next().is_none());

            let guard = kernel.pids.lock();
            let current_pid = kernel.pids.get(current_task.get_pid()).unwrap();
            current_pid.attach_pgid(&guard, child1.thread_group());
            current_pid.attach_pgid(&guard, child2.thread_group());
            current_pid.attach_pgid(&guard, child3.thread_group());
            current_pid.attach_sid(&guard, child1.thread_group());
            current_pid.attach_sid(&guard, child2.thread_group());
            current_pid.attach_sid(&guard, child3.thread_group());
        })
        .await;
    }
}
