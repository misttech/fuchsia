// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::terminal::{Terminal, TerminalController};
use crate::mutable_state::{state_accessor, state_implementation};
use crate::ptrace::{
    AtomicStopState, PtraceAllowedPtracers, PtraceEvent, PtraceOptions, PtraceStatus, StopState,
    ZombiePtracees, ptrace_detach,
};
use crate::security;
use crate::signals::syscalls::WaitingOptions;
use crate::signals::{
    DeliveryAction, IntoSignalInfoOptions, QueuedSignals, SignalActions, SignalDetail, SignalInfo,
    UncheckedSignalInfo, action_for_signal, send_standard_signal,
};
use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{
    ControllingTerminal, CurrentTask, ExitStatus, Kernel, PidTable, ProcessGroup, Session, Task,
    TaskExitState, TaskLifecycleState, TaskMutableState, TaskPersistentInfo, TypedWaitQueue,
};
use crate::time::{IntervalTimerHandle, TimerTable};
use itertools::Itertools;
use macro_rules_attribute::apply;
use starnix_lifecycle::{AtomicCounter, DropNotifier};
use starnix_logging::{log_debug, log_info, log_warn, track_stub};
use starnix_sync::{
    LockBefore, Locked, Mutex, OrderedMutex, ProcessGroupState, RwLock, ThreadGroupLimits, Unlocked,
};
use starnix_task_command::TaskCommand;
use starnix_types::stats::TaskTimeStats;
use starnix_types::time::{itimerspec_from_itimerval, timeval_from_duration};
use starnix_uapi::arc_key::WeakKey;
use starnix_uapi::auth::{CAP_SYS_ADMIN, CAP_SYS_RESOURCE};
use starnix_uapi::errors::Errno;
use starnix_uapi::personality::PersonalityFlags;
use starnix_uapi::resource_limits::{Resource, ResourceLimits};
use starnix_uapi::signals::{
    SIGCHLD, SIGCONT, SIGHUP, SIGKILL, SIGTERM, SIGTTOU, SigSet, Signal, UncheckedSignal,
};
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    ITIMER_PROF, ITIMER_REAL, ITIMER_VIRTUAL, SA_NOCLDWAIT, SI_TKILL, SI_USER, SIG_IGN, errno,
    error, itimerval, pid_t, rlimit, tid_t,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use zx::{Koid, Status};

/// A weak reference to a thread group that can be used in set and maps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadGroupKey {
    pid: pid_t,
    thread_group: WeakKey<ThreadGroup>,
}

impl ThreadGroupKey {
    /// The pid of the thread group keyed by this object.
    ///
    /// As the key is weak (and pid are not unique due to pid namespaces), this should not be used
    /// as an unique identifier of the thread group.
    pub fn pid(&self) -> pid_t {
        self.pid
    }
}

impl std::ops::Deref for ThreadGroupKey {
    type Target = Weak<ThreadGroup>;
    fn deref(&self) -> &Self::Target {
        &self.thread_group.0
    }
}

impl From<&ThreadGroup> for ThreadGroupKey {
    fn from(tg: &ThreadGroup) -> Self {
        Self { pid: tg.leader, thread_group: WeakKey::from(&tg.weak_self.upgrade().unwrap()) }
    }
}

impl<T: AsRef<ThreadGroup>> From<T> for ThreadGroupKey {
    fn from(tg: T) -> Self {
        tg.as_ref().into()
    }
}

/// Values used for waiting on the [ThreadGroup] lifecycle wait queue.
#[repr(u64)]
pub enum ThreadGroupLifecycleWaitValue {
    /// Wait for updates to the exit status of tasks in the group.
    ChildStatus,
    /// Wait for updates to `stopped`.
    Stopped,
    /// Wait for the thread group to fully exit.
    Exited,
}

impl Into<u64> for ThreadGroupLifecycleWaitValue {
    fn into(self) -> u64 {
        self as u64
    }
}

/// Child process that have exited, but the zombie ptrace needs to be consumed
/// before they can be waited for.
#[derive(Clone, Debug)]
pub struct DeferredZombiePTracer {
    /// Original tracer
    pub tracer_thread_group_key: ThreadGroupKey,
    /// Tracee tid
    pub tracee_tid: tid_t,
    /// Tracee pgid
    pub tracee_pgid: pid_t,
    /// Tracee thread group
    pub tracee_thread_group_key: ThreadGroupKey,
}

impl DeferredZombiePTracer {
    fn new(tracer: &ThreadGroup, tracee: &Task) -> Self {
        Self {
            tracer_thread_group_key: tracer.into(),
            tracee_tid: tracee.tid,
            tracee_pgid: tracee.thread_group().read().process_group.leader,
            tracee_thread_group_key: tracee.thread_group_key.clone(),
        }
    }
}

/// The mutable state of the ThreadGroup.
pub struct ThreadGroupMutableState {
    /// The parent thread group.
    ///
    /// The value needs to be writable so that it can be re-parent to the correct subreaper if the
    /// parent ends before the child.
    pub parent: Option<ThreadGroupParent>,

    /// The signal this process generates on exit.
    pub exit_signal: Option<Signal>,

    /// The tasks in the thread group.
    ///
    /// The references to Task is weak to prevent cycles as Task have a Arc reference to their
    /// thread group.
    /// It is still expected that these weak references are always valid, as tasks must unregister
    /// themselves before they are deleted.
    tasks: BTreeMap<tid_t, TaskContainer>,

    /// The children of this thread group.
    ///
    /// The references to ThreadGroup is weak to prevent cycles as ThreadGroup have a Arc reference
    /// to their parent.
    /// It is still expected that these weak references are always valid, as thread groups must unregister
    /// themselves before they are deleted.
    pub children: BTreeMap<pid_t, Weak<ThreadGroup>>,

    /// Child tasks that have exited, but not yet been waited for.
    pub zombie_children: Vec<tid_t>,

    /// ptracees of this process that have exited, but not yet been waited for.
    pub zombie_ptracees: ZombiePtracees,

    /// Child processes that have exited, but the zombie ptrace needs to be consumed
    /// before they can be waited for.
    pub deferred_zombie_ptracers: Vec<DeferredZombiePTracer>,

    /// Unified [WaitQueue] for all waited ThreadGroup events.
    pub lifecycle_waiters: TypedWaitQueue<ThreadGroupLifecycleWaitValue>,

    /// Whether this thread group will inherit from children of dying processes in its descendant
    /// tree.
    pub is_child_subreaper: bool,

    /// The IDs used to perform shell job control.
    pub process_group: Arc<ProcessGroup>,

    pub did_exec: bool,

    /// A signal that indicates whether the process is going to become waitable
    /// via waitid and waitpid for either WSTOPPED or WCONTINUED, depending on
    /// the value of `stopped`. If not None, contains the SignalInfo to return.
    pub last_signal: Option<SignalInfo>,

    /// Whether the thread group is exiting or not, and if it is, the exit info of the thread group.
    run_state: ThreadGroupRunState,

    /// Time statistics accumulated from the children.
    pub children_time_stats: TaskTimeStats,

    /// Personality flags set with `sys_personality()`.
    pub personality: PersonalityFlags,

    /// Thread groups allowed to trace tasks in this this thread group.
    pub allowed_ptracers: PtraceAllowedPtracers,

    /// Channel to message when this thread group exits.
    exit_notifier: Option<futures::channel::oneshot::Sender<()>>,

    /// Notifier for name changes.
    pub notifier: Option<std::sync::mpsc::Sender<MemoryAttributionLifecycleEvent>>,
}

/// A collection of `Task` objects that roughly correspond to a "process".
///
/// Userspace programmers often think about "threads" and "process", but those concepts have no
/// clear analogs inside the kernel because tasks are typically created using `clone(2)`, which
/// takes a complex set of flags that describes how much state is shared between the original task
/// and the new task.
///
/// If a new task is created with the `CLONE_THREAD` flag, the new task will be placed in the same
/// `ThreadGroup` as the original task. Userspace typically uses this flag in conjunction with the
/// `CLONE_FILES`, `CLONE_VM`, and `CLONE_FS`, which corresponds to the userspace notion of a
/// "thread". For example, that's how `pthread_create` behaves. In that sense, a `ThreadGroup`
/// normally corresponds to the set of "threads" in a "process". However, this pattern is purely a
/// userspace convention, and nothing stops userspace from using `CLONE_THREAD` without
/// `CLONE_FILES`, for example.
///
/// In Starnix, a `ThreadGroup` corresponds to a Zircon process, which means we do not support the
/// `CLONE_THREAD` flag without the `CLONE_VM` flag. If we run into problems with this limitation,
/// we might need to revise this correspondence.
///
/// Each `Task` in a `ThreadGroup` has the same thread group ID (`tgid`). The task with the same
/// `pid` as the `tgid` is called the thread group leader.
///
/// Thread groups are destroyed when the last task in the group exits.
pub struct ThreadGroup {
    /// Weak reference to the `OwnedRef` of this `ThreadGroup`. This allows to retrieve the
    /// `TempRef` from a raw `ThreadGroup`.
    pub weak_self: Weak<ThreadGroup>,

    /// The kernel to which this thread group belongs.
    pub kernel: Arc<Kernel>,

    /// A handle to the underlying Zircon process object.
    ///
    /// Currently, we have a 1-to-1 mapping between thread groups and zx::process
    /// objects. This approach might break down if/when we implement CLONE_VM
    /// without CLONE_THREAD because that creates a situation where two thread
    /// groups share an address space. To implement that situation, we might
    /// need to break the 1-to-1 mapping between thread groups and zx::process
    /// or teach zx::process to share address spaces.
    pub process: zx::Process,

    /// A handle to the restricted address space for the Zircon process object.
    pub root_vmar: zx::Vmar,

    /// The lead task of this thread group.
    ///
    /// The lead task is typically the initial thread created in the thread group.
    pub leader: pid_t,

    // TODO(https://fxbug.dev/508746892): Remove this once the `PidTable` lock is removed.
    /// Cached weak reference to the leader task.
    ///
    /// This is used to break a deadlock in signal delivery, where a reference to the leader task
    /// must be obtained in order to do access checks in situations where the leader has exited and
    /// is no longer in the task list.
    pub leader_task: OnceLock<Weak<Task>>,

    /// The signal actions that are registered for this process.
    pub signal_actions: Arc<SignalActions>,

    /// The timers for this thread group (from timer_create(), etc.).
    pub timers: TimerTable,

    /// A mechanism to be notified when this `ThreadGroup` is destroyed.
    pub drop_notifier: DropNotifier,

    /// Whether the process is currently stopped.
    ///
    /// Must only be set when the `mutable_state` write lock is held.
    stop_state: AtomicStopState,

    /// The mutable state of the ThreadGroup.
    mutable_state: RwLock<ThreadGroupMutableState>,

    /// The resource limits for this thread group.  This is outside mutable_state
    /// to avoid deadlocks where the thread_group lock is held when acquiring
    /// the task lock, and vice versa.
    pub limits: OrderedMutex<ResourceLimits, ThreadGroupLimits>,

    /// The next unique identifier for a seccomp filter.  These are required to be
    /// able to distinguish identical seccomp filters, which are treated differently
    /// for the purposes of SECCOMP_FILTER_FLAG_TSYNC.  Inherited across clone because
    /// seccomp filters are also inherited across clone.
    pub next_seccomp_filter_id: AtomicCounter<u64>,

    /// Tasks ptraced by this process
    pub ptracees: Mutex<BTreeMap<tid_t, TaskContainer>>,

    /// The signals that are currently pending for this thread group.
    pub pending_signals: Mutex<QueuedSignals>,

    /// Whether or not there are any pending signals available for tasks in this thread group.
    /// Used to avoid having to acquire the signal state lock in hot paths.
    pub has_pending_signals: AtomicBool,

    /// The monotonic time at which the thread group started.
    pub start_time: zx::MonotonicInstant,

    /// Whether to log syscalls at INFO level for this thread group.
    log_syscalls_as_info: AtomicBool,
}

impl fmt::Debug for ThreadGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.process.get_name().unwrap_or(zx::Name::new_lossy("<unknown>")),
            self.leader
        )
    }
}

impl ThreadGroup {
    pub fn sync_syscall_log_level(&self) {
        let command = self.read().leader_command();
        let filters = self.kernel.syscall_log_filters.lock();
        let should_log = filters.iter().any(|f| f.matches(&command));
        let prev_should_log = self.log_syscalls_as_info.swap(should_log, Ordering::Relaxed);
        let change_str = match (should_log, prev_should_log) {
            (true, false) => Some("Enabled"),
            (false, true) => Some("Disabled"),
            _ => None,
        };
        if let Some(change_str) = change_str {
            log_info!(
                "{change_str} info syscall logs for thread group {} (command: {command})",
                self.leader
            );
        }
    }

    #[inline]
    pub fn syscall_log_level(&self) -> starnix_logging::Level {
        if self.log_syscalls_as_info.load(Ordering::Relaxed) {
            starnix_logging::Level::Info
        } else {
            starnix_logging::Level::Trace
        }
    }
}

impl PartialEq for ThreadGroup {
    fn eq(&self, other: &Self) -> bool {
        self.leader == other.leader
    }
}

impl Drop for ThreadGroup {
    fn drop(&mut self) {
        let state = self.mutable_state.get_mut();
        assert!(state.tasks.is_empty());
        assert!(state.children.is_empty());
        assert!(state.zombie_children.is_empty());
        assert!(state.zombie_ptracees.is_empty());
        #[cfg(any(test, debug_assertions))]
        assert!(
            state
                .parent
                .as_ref()
                .and_then(|p| p.0.upgrade().as_ref().map(|p| p
                    .read()
                    .children
                    .get(&self.leader)
                    .is_none()))
                .unwrap_or(true)
        );
    }
}

/// A wrapper around a `Weak<ThreadGroup>` that expects the underlying `Weak` to always be
/// valid. The wrapper will check this at runtime during creation and upgrade.
pub struct ThreadGroupParent(Weak<ThreadGroup>);

impl ThreadGroupParent {
    pub fn new(t: Weak<ThreadGroup>) -> Self {
        debug_assert!(t.upgrade().is_some());
        Self(t)
    }

    pub fn upgrade(&self) -> Arc<ThreadGroup> {
        self.0.upgrade().expect("ThreadGroupParent references must always be valid")
    }
}

impl Clone for ThreadGroupParent {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// A selector that can match a process. Works as a representation of the pid argument to syscalls
/// like wait and kill.
#[derive(Debug, Clone)]
pub enum ProcessSelector {
    /// Matches any process at all.
    Any,
    /// Matches only the process with the specified pid
    Pid(pid_t),
    /// Matches all the processes in the given process group
    Pgid(pid_t),
    /// Match the thread group with the given key
    Process(ThreadGroupKey),
}

impl ProcessSelector {
    pub fn match_tid(&self, tid: tid_t, pid_table: &PidTable) -> bool {
        match *self {
            ProcessSelector::Pid(p) => {
                if p == tid {
                    true
                } else {
                    if let Ok(task_ref) = pid_table.get_task(tid) {
                        task_ref.get_pid() == p
                    } else {
                        false
                    }
                }
            }
            ProcessSelector::Any => true,
            ProcessSelector::Pgid(pgid) => {
                if let Ok(task_ref) = pid_table.get_task(tid) {
                    pid_table.get_process_group(pgid).as_ref()
                        == Some(&task_ref.thread_group().read().process_group)
                } else {
                    false
                }
            }
            ProcessSelector::Process(ref key) => {
                if let Some(tg) = key.upgrade() {
                    tg.read().tasks.contains_key(&tid)
                } else {
                    false
                }
            }
        }
    }

    pub fn match_task(&self, task: &Task) -> bool {
        match *self {
            ProcessSelector::Any => true,
            ProcessSelector::Pid(pid) => task.thread_group_key.pid() == pid,
            ProcessSelector::Pgid(pgid) => task.thread_group.read().process_group.leader == pgid,
            ProcessSelector::Process(ref key) => task.thread_group_key == *key,
        }
    }

    pub fn match_task_and_waiting_options(&self, task: &Task, options: &WaitingOptions) -> bool {
        let Some(exit_state) = task.exit_state() else {
            return false;
        };

        if !self.match_task(task) {
            return false;
        }

        if options.wait_for_all {
            return true;
        }

        // A "clone" zombie is one which has delivered no signal, or a signal other than SIGCHLD, to
        // its parent upon exit.
        options.wait_for_clone == (exit_state.signal != Some(SIGCHLD))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ThreadGroupRunState {
    #[default]
    Running,
    Exiting(ExitStatus),
    Exited(ExitStatus),
}

impl ThreadGroup {
    /// Creates a ThreadGroup for a regular userspace process.
    pub fn new<L>(
        locked: &mut Locked<L>,
        kernel: Arc<Kernel>,
        process: zx::Process,
        root_vmar: zx::Vmar,
        parent: Option<ThreadGroupWriteGuard<'_>>,
        leader: pid_t,
        exit_signal: Option<Signal>,
        process_group: Arc<ProcessGroup>,
        signal_actions: Arc<SignalActions>,
    ) -> Arc<ThreadGroup>
    where
        L: LockBefore<ProcessGroupState>,
    {
        debug_assert!(!process.is_invalid());
        debug_assert!(!root_vmar.is_invalid());
        Self::new_internal(
            locked,
            kernel,
            process,
            root_vmar,
            parent,
            leader,
            exit_signal,
            process_group,
            signal_actions,
        )
    }

    /// Creates a ThreadGroup for a kernel system task (e.g., kthreadd).
    pub fn for_system<L>(
        locked: &mut Locked<L>,
        kernel: Arc<Kernel>,
        leader: pid_t,
        process_group: Arc<ProcessGroup>,
    ) -> Arc<ThreadGroup>
    where
        L: LockBefore<ProcessGroupState>,
    {
        Self::new_internal(
            locked,
            kernel,
            zx::Process::invalid(),
            zx::Vmar::invalid(),
            None,
            leader,
            Some(SIGCHLD),
            process_group,
            SignalActions::default(),
        )
    }

    /// Creates a ThreadGroup suitable for use in tests.
    ///
    /// This function performs the minimal setup necessary to produce a valid `ThreadGroup`
    /// instance. It uses an invalid handle for the root VMAR, sets no parent, and uses
    /// default signal actions with `SIGCHLD` as the exit signal.
    ///
    /// This should only be used in tests where a full process environment is not required.
    pub fn for_test<L>(
        locked: &mut Locked<L>,
        kernel: Arc<Kernel>,
        process: zx::Process,
        parent: ThreadGroupWriteGuard<'_>,
        leader: pid_t,
        process_group: Arc<ProcessGroup>,
    ) -> Arc<ThreadGroup>
    where
        L: LockBefore<ProcessGroupState>,
    {
        Self::new_internal(
            locked,
            kernel,
            process,
            zx::Vmar::invalid(),
            Some(parent),
            leader,
            Some(SIGCHLD),
            process_group,
            SignalActions::default(),
        )
    }

    fn new_internal<L>(
        locked: &mut Locked<L>,
        kernel: Arc<Kernel>,
        process: zx::Process,
        root_vmar: zx::Vmar,
        parent: Option<ThreadGroupWriteGuard<'_>>,
        leader: pid_t,
        exit_signal: Option<Signal>,
        process_group: Arc<ProcessGroup>,
        signal_actions: Arc<SignalActions>,
    ) -> Arc<ThreadGroup>
    where
        L: LockBefore<ProcessGroupState>,
    {
        Arc::new_cyclic(|weak_self| {
            let mut thread_group = ThreadGroup {
                weak_self: weak_self.clone(),
                kernel,
                process,
                root_vmar,
                leader,
                leader_task: OnceLock::new(),
                signal_actions,
                timers: Default::default(),
                drop_notifier: Default::default(),
                // A child process created via fork(2) inherits its parent's
                // resource limits.  Resource limits are preserved across execve(2).
                limits: OrderedMutex::new(
                    parent
                        .as_ref()
                        .map(|p| p.base.limits.lock(locked.cast_locked()).clone())
                        .unwrap_or(Default::default()),
                ),
                next_seccomp_filter_id: Default::default(),
                ptracees: Default::default(),
                stop_state: AtomicStopState::new(StopState::Awake),
                pending_signals: Default::default(),
                has_pending_signals: Default::default(),
                start_time: zx::MonotonicInstant::get(),
                mutable_state: RwLock::new(ThreadGroupMutableState {
                    parent: parent
                        .as_ref()
                        .map(|p| ThreadGroupParent::new(p.base.weak_self.clone())),
                    exit_signal,
                    tasks: BTreeMap::new(),
                    children: BTreeMap::new(),
                    zombie_children: vec![],
                    zombie_ptracees: ZombiePtracees::new(),
                    deferred_zombie_ptracers: vec![],
                    lifecycle_waiters: TypedWaitQueue::<ThreadGroupLifecycleWaitValue>::default(),
                    is_child_subreaper: false,
                    process_group: Arc::clone(&process_group),
                    did_exec: false,
                    last_signal: None,
                    run_state: Default::default(),
                    children_time_stats: Default::default(),
                    personality: parent
                        .as_ref()
                        .map(|p| p.personality)
                        .unwrap_or(Default::default()),
                    allowed_ptracers: PtraceAllowedPtracers::None,
                    exit_notifier: None,
                    notifier: None,
                }),
                log_syscalls_as_info: AtomicBool::new(false),
            };

            if let Some(mut parent) = parent {
                thread_group.next_seccomp_filter_id.reset(parent.base.next_seccomp_filter_id.get());
                parent.children.insert(leader, weak_self.clone());
                process_group.insert(locked, &thread_group);
            };
            thread_group
        })
    }

    state_accessor!(ThreadGroup, mutable_state);

    pub fn load_stopped(&self) -> StopState {
        self.stop_state.load(Ordering::Relaxed)
    }

    // Causes the thread group to exit.  If this is being called from a task
    // that is part of the current thread group, the caller should pass
    // `current_task`.  If ownership issues prevent passing `current_task`, then
    // callers should use CurrentTask::thread_group_exit instead.
    pub fn exit(
        &self,
        locked: &mut Locked<Unlocked>,
        exit_status: ExitStatus,
        mut current_task: Option<&mut CurrentTask>,
    ) {
        if let Some(ref mut current_task) = current_task {
            current_task.ptrace_event(
                locked,
                PtraceOptions::TRACEEXIT,
                exit_status.signal_info_status() as u64,
            );
        }
        let mut pids = self.kernel.pids.write();
        let mut state = self.write();
        if !state.is_running() {
            // The thread group is not running. All threads in the thread group have already been
            // interrupted.
            return;
        }

        state.run_state = ThreadGroupRunState::Exiting(exit_status.clone());

        // Drop ptrace zombies
        state.zombie_ptracees.release(&mut pids);

        // Interrupt each task. Unlock the group because send_signal will lock the group in order
        // to call set_stopped.
        let tasks = state.tasks();
        drop(state);

        // Detach from any ptraced tasks, killing the ones that set PTRACE_O_EXITKILL.
        let tracees = self.ptracees.lock().keys().cloned().collect::<Vec<_>>();
        for tracee in tracees {
            if let Ok(task_ref) = pids.get_task(tracee) {
                let mut should_send_sigkill = false;
                if let Some(ptrace) = &task_ref.read().ptrace {
                    should_send_sigkill = ptrace.has_option(PtraceOptions::EXITKILL);
                }
                if should_send_sigkill {
                    send_standard_signal(locked, task_ref.as_ref(), SignalInfo::kernel(SIGKILL));
                    continue;
                }

                let _ =
                    ptrace_detach(locked, &mut pids, self, task_ref.as_ref(), &UserAddress::NULL);
            }
        }

        for task in tasks {
            task.write().set_exit_status(exit_status.clone());
            send_standard_signal(locked, &task, SignalInfo::kernel(SIGKILL));
        }
    }

    pub fn add(&self, task: Arc<Task>) -> Result<(), Errno> {
        let mut state = self.write();
        if !state.is_running() {
            if state.tasks_count() == 0 {
                log_warn!(
                    "Task {} with leader {} exiting while adding its first task, \
                not sending creation notification",
                    task.tid,
                    self.leader
                );
            }
            return error!(EINVAL);
        }
        if task.tid == self.leader {
            let _ = self.leader_task.set(Arc::downgrade(&task));
        }
        state.tasks.insert(task.tid, (&task).into());

        Ok(())
    }

    /// Remove the task from the children of this ThreadGroup.
    ///
    /// It is important that the task is taken as an `Arc`. It ensures the tasks of the
    /// ThreadGroup are always valid as they are still valid when removed.
    pub fn remove<L>(&self, locked: &mut Locked<L>, task: &Arc<Task>)
    where
        L: LockBefore<ProcessGroupState>,
    {
        let mut pids = task.kernel().pids.write();

        // To prevent nested locking with tracers, which may be ancestors to this thread group,
        // only hold the thread group state lock long enough to remove the task.
        let was_last_task = {
            let mut state = self.write();
            if state.tasks.remove(&task.tid).is_none() {
                // The task has never been added. This should only happen when the thread group is
                // not running.
                debug_assert!(!state.is_running());
                return;
            }
            state.tasks.is_empty()
        };

        // Locate the running tracer for the removed task. Tasks that have a tracer that is not
        // running are processed as if they are not traced.
        let tracer = task
            .read()
            .ptrace
            .as_ref()
            .map(|ptrace| ptrace.get_pid())
            .and_then(|pid| pids.get_thread_group(pid))
            .and_then(|tracer| {
                let mut state = tracer.write();
                if state.is_running() {
                    // Mark this task as a zombie ptracee.
                    state.zombie_ptracees.add(Arc::clone(task));
                    tracer.ptracees.lock().remove(&task.tid);
                    drop(state);
                    Some(tracer)
                } else {
                    None
                }
            });

        // Determine whether the task should be immediately reaped. Only leader tasks and active
        // ptracees become zombies. All other tasks are transparently reaped upon removal from the
        // thread group.
        let zombify = task.is_leader() || tracer.is_some();
        if !zombify {
            pids.remove_task(task.tid);
        }

        // The thread group exits after its last task is removed.
        if was_last_task {
            // For the thread group exit itself, use the exit state from the last task to exit.
            let leader =
                pids.get_task(self.leader).expect("Thread group exits before reaping leader");
            let exit_status = task.exit_state().expect("Last task has exited").status.clone();
            let parent = self.do_exit(locked, &mut *pids, &leader, exit_status);

            // For zombie notification, use the exit state from either the removed task, if it
            // should zombify, or from the leader task.
            let zombie = if zombify { task } else { &leader };

            if let Some(parent) = parent {
                parent.upgrade().notify_zombie(&mut *pids, tracer.as_deref(), zombie);
            } else {
                // Do not leave orphaned zombies in the PID table. Reap immediately.
                pids.remove_task(zombie.tid);
            }
        }
    }

    /// Finalize the exit of the [`ThreadGroup`] after its last task has been removed.
    ///
    /// Returns the [`ThreadGroupParent`] to notify of the exit.
    fn do_exit<L>(
        &self,
        locked: &mut Locked<L>,
        mut pids: &mut PidTable,
        leader: &Task,
        exit_status: ExitStatus,
    ) -> Option<ThreadGroupParent>
    where
        L: LockBefore<ProcessGroupState>,
    {
        let mut state = self.write();

        // Synchronize the exit state of the thread group and its leader task. The leader task is
        // the representative of the thread group in the form of a zombie process. If not already
        // set by exit_group, the exit status is taken from the last task to exit.
        let exit_status = match &state.run_state {
            ThreadGroupRunState::Exiting(status) => status.clone(),
            _ => exit_status,
        };
        let mut exit_state = leader.exit_state().expect("Leader exits before thread group").clone();
        exit_state.status = exit_status.clone();
        // Include the time statistics of the thread group's reaped children in the leader
        exit_state.time_stats += state.children_time_stats;
        leader.lifecycle_state.update(TaskLifecycleState::Exited(exit_state));
        state.run_state = ThreadGroupRunState::Exiting(exit_status.clone());

        // Leave the process group.
        state.leave_process_group(locked, &pids);

        // Drop the lock on the thread group before removing the process from the cgroup2 PID table.
        // The CgroupState lock must be taken before the ThreadGroup state lock.
        // See lock_cgroup2_pid_table for details.
        drop(state);
        self.kernel.cgroups.lock_cgroup2_pid_table().remove_process(self.into());

        // Reparent thread group children.
        if let Some(reaper) = self.find_reaper() {
            let reaper = reaper.upgrade();
            {
                // Parent thread groups must be locked before their children.
                let mut reaper_state = reaper.write();
                let mut state = self.write();
                for (_pid, weak_child) in std::mem::take(&mut state.children) {
                    if let Some(child) = weak_child.upgrade() {
                        let mut child_state = child.write();
                        child_state.exit_signal = Some(SIGCHLD);
                        child_state.parent = Some(ThreadGroupParent::new(Arc::downgrade(&reaper)));
                        reaper_state.children.insert(child.leader, weak_child.clone());
                    }
                }
                reaper_state.zombie_children.append(&mut state.zombie_children);
            }
            ZombiePtracees::reparent(self, &reaper);
        } else {
            // This should only happen for the init process under specific conditions.
            assert_eq!(self.leader, 1, "Non-init ThreadGroup exiting without reaper");

            let mut state = self.write();

            // Check for children that will be orphaned by this group exiting.
            // TODO(https://fxbug.dev/507836238): Check what Linux does in this situation and match.
            // It's difficult to observe Linux's behavior directly here, as it occurs very late in
            // the system lifecycle, if at all. For now, we panic to catch a report of an actual
            // occurrence.
            assert_eq!(state.children.len(), 0);

            // Immediately reap any remaining zombie children.
            for tid in state.zombie_children.drain(..) {
                pids.remove_task(tid);
            }
            state.zombie_ptracees.release(&mut pids);
        }

        // Check that reparenting did not leave any children behind.
        #[cfg(any(test, debug_assertions))]
        {
            let state = self.read();
            assert!(state.zombie_children.is_empty());
            assert!(state.zombie_ptracees.is_empty());
        }

        // Clear the `parent` reference now that children have been reparented.
        let parent = self.write().parent.take();
        if let Some(ref parent) = parent {
            let parent = parent.upgrade();
            parent.check_orphans(locked, &pids);
        }

        // TODO: Set the error_code on the Zircon process object. Currently missing a way
        // to do this in Zircon. Might be easier in the new execution model.

        // Once the last zircon thread stops, the zircon process will also stop executing.

        // Mark the thread group as exited and notify waiters.
        let mut state = self.write();
        state.run_state = ThreadGroupRunState::Exited(exit_status);
        state.lifecycle_waiters.notify_value(ThreadGroupLifecycleWaitValue::Exited);
        if let Some(notifier) = state.exit_notifier.take() {
            let _ = notifier.send(());
        }

        parent
    }

    /// Notifies the tracer and/or the parent of a task's zombification, depending on whether the
    /// tracer is watching the tracee and the tracer is the parent.
    ///
    /// Must be called on the parent [`ThreadGroup`].
    fn notify_zombie(&self, pids: &mut PidTable, tracer: Option<&ThreadGroup>, task: &Task) {
        // To avoid potential lock inversion with further zombie notification, this state lock must
        // be dropped prior to calling `notify_zombie()` or `notify_child_zombie()`.
        let mut state = self.write();

        // When a parent process exits and reparents its children, it does so by taking its own
        // lock, clearing its `children` map, and updating each child's `parent` pointer to the new
        // `reaper` under the child's lock (respecting the Parent => Child lock ordering).
        //
        // If a child exits concurrently, it may read its parent pointer (obtaining this parent),
        // drop its child lock, and then block trying to acquire this parent's lock (since the
        // parent is currently exiting and holding the lock).
        //
        // While the child is blocked, the parent successfully reparents the child to the `reaper`
        // and inserts it into `reaper.children` (believing the child is still running).
        //
        // When the child resumes, it will call `notify_zombie` on this OLD parent (using the
        // captured parent pointer). If we were to process the notification here, it would be lost
        // (since this parent is exiting and its zombie lists have already been moved to the
        // reaper), and the `reaper` would never be notified that the child is a zombie, leaving a
        // dangling weak reference in `reaper.children` which eventually causes a kernel panic.
        //
        // To resolve this, we temporarily acquire the child's read lock (respecting the Parent =>
        // Child lock order since we hold the parent's write lock) to check if the child's parent
        // has changed. If it has, we drop our parent lock (to prevent holding multiple parent locks
        // concurrently and causing lock inversion) and forward the notification to the new parent.
        let task_tg = task.thread_group();
        let task_parent = task_tg.read().parent.clone();
        if let Some(task_parent) = task_parent {
            let task_parent = task_parent.upgrade();
            let this = self.weak_self.upgrade().unwrap();
            if !Arc::ptr_eq(&task_parent, &this) {
                drop(state);
                task_parent.notify_zombie(pids, tracer, task);
                return;
            }
        }

        let Some(tracer) = tracer else {
            // There is no tracer. Notify the parent.
            drop(state);
            self.notify_child_zombie(pids, task);
            return;
        };

        if self == tracer {
            // The parent and tracer are the same. Reuse the parent state lock as the tracer lock.
            if !state.zombie_ptracees.has_tracee(task.tid) {
                // The parent/tracer has consumed the notification. No further action required.
                return;
            }
            state.zombie_ptracees.remove(pids, task.tid);
            drop(state);
            self.notify_child_zombie(pids, task);
            return;
        }

        let mut tracer_state = tracer.write();
        if !tracer_state.is_running() {
            // The tracer exited between zombie tracee registration and this notification step. The
            // parent was notified during tracer exit. No further action required.
            return;
        }

        if !tracer_state.zombie_ptracees.has_tracee(task.tid) {
            // The tracer has consumed the notification. Notify the parent.
            drop(tracer_state);
            drop(state);
            self.notify_child_zombie(pids, task);
            return;
        }

        // Defer notification to the parent and notify the tracer.
        tracer_state.zombie_ptracees.set_parent_of(task.tid, self);
        drop(tracer_state);
        state.deferred_zombie_ptracers.push(DeferredZombiePTracer::new(tracer, task));
        state.children.remove(&task.get_pid());
        drop(state);
        task.write().notify_ptracers();
    }

    pub fn notify_child_zombie(&self, pids: &mut PidTable, task: &Task) {
        let exit_state = task.exit_state().expect("Task is exited");

        let mut state = self.write();
        state.children.remove(&task.get_pid());
        state
            .deferred_zombie_ptracers
            .retain(|tracer| tracer.tracee_thread_group_key != task.thread_group_key);

        // Tasks that exit with an ignored signal or `SIGCHLD` with `SA_NOCLDWAIT` set are reaped
        // immediately upon exit rather than being kept as zombies.
        let reap = exit_state
            .signal
            .map(|signal| {
                let sigaction = self.signal_actions.get(signal);
                sigaction.sa_handler == SIG_IGN
                    || (signal == SIGCHLD && (sigaction.sa_flags & SA_NOCLDWAIT as u64) != 0)
            })
            .unwrap_or(false);

        if reap {
            // Only count the zombie's time statistics upon reaping.
            state.children_time_stats += exit_state.time_stats;
            pids.remove_task(task.tid);
        } else {
            state.zombie_children.push(task.get_pid());
        }

        // Conditionally extract the signal information from the zombie only if the child specified
        // an exit signal. When a child is created without an exit signal, it must not signal its
        // parent upon exit.
        let signal_info = exit_state.signal.map(|_| exit_state.as_signal_info());

        state.lifecycle_waiters.notify_value(ThreadGroupLifecycleWaitValue::ChildStatus);
        if let Some(signal_info) = signal_info {
            state.send_signal(signal_info);
        }
    }

    /// Find the task which will adopt our children after we die.
    fn find_reaper(&self) -> Option<ThreadGroupParent> {
        let mut weak_parent = self.read().parent.clone()?;
        loop {
            weak_parent = {
                let parent = weak_parent.upgrade();
                let parent_state = parent.read();
                if parent_state.is_child_subreaper {
                    break;
                }
                match parent_state.parent {
                    Some(ref next_parent) => next_parent.clone(),
                    None => break,
                }
            };
        }
        Some(weak_parent)
    }

    pub fn setsid<L>(&self, locked: &mut Locked<L>) -> Result<(), Errno>
    where
        L: LockBefore<ProcessGroupState>,
    {
        let pids = self.kernel.pids.read();
        if pids.get_process_group(self.leader).is_some() {
            return error!(EPERM);
        }
        let process_group = ProcessGroup::new(self.leader, None);
        pids.add_process_group(process_group.clone());
        self.write().set_process_group(locked, process_group, &pids);
        self.check_orphans(locked, &pids);

        Ok(())
    }

    pub fn setpgid<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        target: &Task,
        pgid: pid_t,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ProcessGroupState>,
    {
        let pids = self.kernel.pids.read();

        {
            let current_process_group = Arc::clone(&self.read().process_group);

            // The target process must be either the current process of a child of the current process
            let mut target_thread_group = target.thread_group().write();
            let is_target_current_process_child =
                target_thread_group.parent.as_ref().map(|tg| tg.upgrade().leader)
                    == Some(self.leader);
            if target_thread_group.leader() != self.leader && !is_target_current_process_child {
                return error!(ESRCH);
            }

            // If the target process is a child of the current task, it must not have executed one of the exec
            // function.
            if is_target_current_process_child && target_thread_group.did_exec {
                return error!(EACCES);
            }

            let new_process_group;
            {
                let target_process_group = &target_thread_group.process_group;

                // The target process must not be a session leader and must be in the same session as the current process.
                if target_thread_group.leader() == target_process_group.session.leader
                    || current_process_group.session != target_process_group.session
                {
                    return error!(EPERM);
                }

                let target_pgid = if pgid == 0 { target_thread_group.leader() } else { pgid };
                if target_pgid < 0 {
                    return error!(EINVAL);
                }

                if target_pgid == target_process_group.leader {
                    return Ok(());
                }

                // If pgid is not equal to the target process id, the associated process group must exist
                // and be in the same session as the target process.
                if target_pgid != target_thread_group.leader() {
                    new_process_group =
                        pids.get_process_group(target_pgid).ok_or_else(|| errno!(EPERM))?;
                    if new_process_group.session != target_process_group.session {
                        return error!(EPERM);
                    }
                    security::check_setpgid_access(current_task, target)?;
                } else {
                    security::check_setpgid_access(current_task, target)?;
                    // Create a new process group
                    new_process_group =
                        ProcessGroup::new(target_pgid, Some(target_process_group.session.clone()));
                    pids.add_process_group(new_process_group.clone());
                }
            }

            target_thread_group.set_process_group(locked, new_process_group, &pids);
        }

        target.thread_group().check_orphans(locked, &pids);

        Ok(())
    }

    fn itimer_real(&self) -> IntervalTimerHandle {
        self.timers.itimer_real()
    }

    pub fn set_itimer(
        &self,
        current_task: &CurrentTask,
        which: u32,
        value: itimerval,
    ) -> Result<itimerval, Errno> {
        if which == ITIMER_PROF || which == ITIMER_VIRTUAL {
            // We don't support setting these timers.
            // The gvisor test suite clears ITIMER_PROF as part of its test setup logic, so we support
            // clearing these values.
            if value.it_value.tv_sec == 0 && value.it_value.tv_usec == 0 {
                return Ok(itimerval::default());
            }
            track_stub!(TODO("https://fxbug.dev/322874521"), "Unsupported itimer type", which);
            return error!(ENOTSUP);
        }

        if which != ITIMER_REAL {
            return error!(EINVAL);
        }
        let itimer_real = self.itimer_real();
        let prev_remaining = itimer_real.time_remaining();
        if value.it_value.tv_sec != 0 || value.it_value.tv_usec != 0 {
            itimer_real.arm(current_task, itimerspec_from_itimerval(value), false)?;
        } else {
            itimer_real.disarm(current_task)?;
        }
        Ok(itimerval {
            it_value: timeval_from_duration(prev_remaining.remainder),
            it_interval: timeval_from_duration(prev_remaining.interval),
        })
    }

    pub fn get_itimer(&self, which: u32) -> Result<itimerval, Errno> {
        if which == ITIMER_PROF || which == ITIMER_VIRTUAL {
            // We don't support setting these timers, so we can accurately report that these are not set.
            return Ok(itimerval::default());
        }
        if which != ITIMER_REAL {
            return error!(EINVAL);
        }
        let remaining = self.itimer_real().time_remaining();
        Ok(itimerval {
            it_value: timeval_from_duration(remaining.remainder),
            it_interval: timeval_from_duration(remaining.interval),
        })
    }

    /// Check whether the stop state is compatible with `new_stopped`. If it is return it,
    /// otherwise, return None.
    fn check_stopped_state(
        &self,
        new_stopped: StopState,
        finalize_only: bool,
    ) -> Option<StopState> {
        let stopped = self.load_stopped();
        if finalize_only && !stopped.is_stopping_or_stopped() {
            return Some(stopped);
        }

        if stopped.is_illegal_transition(new_stopped) {
            return Some(stopped);
        }

        return None;
    }

    /// Set the stop status of the process.  If you pass |siginfo| of |None|,
    /// does not update the signal.  If |finalize_only| is set, will check that
    /// the set will be a finalize (Stopping -> Stopped or Stopped -> Stopped)
    /// before executing it.
    ///
    /// Returns the latest stop state after any changes.
    pub fn set_stopped(
        &self,
        new_stopped: StopState,
        siginfo: Option<SignalInfo>,
        finalize_only: bool,
    ) -> StopState {
        // Perform an early return check to see if we can avoid taking the lock.
        if let Some(stopped) = self.check_stopped_state(new_stopped, finalize_only) {
            return stopped;
        }

        self.write().set_stopped(new_stopped, siginfo, finalize_only)
    }

    /// Ensures |session| is the controlling session inside of |terminal_controller|, and returns a
    /// reference to the |TerminalController|.
    fn check_terminal_controller(
        session: &Arc<Session>,
        terminal_controller: &Option<TerminalController>,
    ) -> Result<(), Errno> {
        if let Some(terminal_controller) = terminal_controller {
            if let Some(terminal_session) = terminal_controller.session.upgrade() {
                if Arc::ptr_eq(session, &terminal_session) {
                    return Ok(());
                }
            }
        }
        error!(ENOTTY)
    }

    pub fn get_foreground_process_group(&self, terminal: &Terminal) -> Result<pid_t, Errno> {
        let state = self.read();
        let process_group = &state.process_group;
        let terminal_state = terminal.read();

        // "When fd does not refer to the controlling terminal of the calling
        // process, -1 is returned" - tcgetpgrp(3)
        Self::check_terminal_controller(&process_group.session, &terminal_state.controller)?;
        let pid = process_group.session.read().get_foreground_process_group_leader();
        Ok(pid)
    }

    pub fn set_foreground_process_group<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        terminal: &Terminal,
        pgid: pid_t,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ProcessGroupState>,
    {
        let process_group;
        let send_ttou;
        {
            // Keep locks to ensure atomicity.
            let pids = self.kernel.pids.read();
            let state = self.read();
            process_group = Arc::clone(&state.process_group);
            let terminal_state = terminal.read();
            Self::check_terminal_controller(&process_group.session, &terminal_state.controller)?;

            // pgid must be positive.
            if pgid < 0 {
                return error!(EINVAL);
            }

            let new_process_group = pids.get_process_group(pgid).ok_or_else(|| errno!(ESRCH))?;
            if new_process_group.session != process_group.session {
                return error!(EPERM);
            }

            let mut session_state = process_group.session.write();
            // If the calling process is a member of a background group and not ignoring SIGTTOU, a
            // SIGTTOU signal is sent to all members of this background process group.
            send_ttou = process_group.leader != session_state.get_foreground_process_group_leader()
                && !current_task.read().signal_mask().has_signal(SIGTTOU)
                && self.signal_actions.get(SIGTTOU).sa_handler != SIG_IGN;

            if !send_ttou {
                session_state.set_foreground_process_group(&new_process_group);
            }
        }

        // Locks must not be held when sending signals.
        if send_ttou {
            process_group.send_signals(locked, &[SIGTTOU]);
            return error!(EINTR);
        }

        Ok(())
    }

    pub fn set_controlling_terminal(
        &self,
        current_task: &CurrentTask,
        terminal: &Terminal,
        is_main: bool,
        steal: bool,
        is_readable: bool,
    ) -> Result<(), Errno> {
        // Keep locks to ensure atomicity.
        let state = self.read();
        let process_group = &state.process_group;
        let mut terminal_state = terminal.write();
        let mut session_writer = process_group.session.write();

        // "The calling process must be a session leader and not have a
        // controlling terminal already." - tty_ioctl(4)
        if process_group.session.leader != self.leader
            || session_writer.controlling_terminal.is_some()
        {
            return error!(EINVAL);
        }

        let mut has_admin_capability_determined = false;

        // "If this terminal is already the controlling terminal of a different
        // session group, then the ioctl fails with EPERM, unless the caller
        // has the CAP_SYS_ADMIN capability and arg equals 1, in which case the
        // terminal is stolen, and all processes that had it as controlling
        // terminal lose it." - tty_ioctl(4)
        if let Some(other_session) =
            terminal_state.controller.as_ref().and_then(|cs| cs.session.upgrade())
        {
            if other_session != process_group.session {
                if !steal {
                    return error!(EPERM);
                }
                security::check_task_capable(current_task, CAP_SYS_ADMIN)?;
                has_admin_capability_determined = true;

                // Steal the TTY away. Unlike TIOCNOTTY, don't send signals.
                other_session.write().controlling_terminal = None;
            }
        }

        if !is_readable && !has_admin_capability_determined {
            security::check_task_capable(current_task, CAP_SYS_ADMIN)?;
        }

        session_writer.controlling_terminal = Some(ControllingTerminal::new(terminal, is_main));
        terminal_state.controller = TerminalController::new(&process_group.session);
        Ok(())
    }

    pub fn release_controlling_terminal<L>(
        &self,
        locked: &mut Locked<L>,
        _current_task: &CurrentTask,
        terminal: &Terminal,
        is_main: bool,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ProcessGroupState>,
    {
        let process_group;
        {
            // Keep locks to ensure atomicity.
            let state = self.read();
            process_group = Arc::clone(&state.process_group);
            let mut terminal_state = terminal.write();
            let mut session_writer = process_group.session.write();

            // tty must be the controlling terminal.
            Self::check_terminal_controller(&process_group.session, &terminal_state.controller)?;
            if !session_writer
                .controlling_terminal
                .as_ref()
                .map_or(false, |ct| ct.matches(terminal, is_main))
            {
                return error!(ENOTTY);
            }

            // "If the process was session leader, then send SIGHUP and SIGCONT to the foreground
            // process group and all processes in the current session lose their controlling terminal."
            // - tty_ioctl(4)

            // Remove tty as the controlling tty for each process in the session, then
            // send them SIGHUP and SIGCONT.

            session_writer.controlling_terminal = None;
            terminal_state.controller = None;
        }

        if process_group.session.leader == self.leader {
            process_group.send_signals(locked, &[SIGHUP, SIGCONT]);
        }

        Ok(())
    }

    fn check_orphans<L>(&self, locked: &mut Locked<L>, pids: &PidTable)
    where
        L: LockBefore<ProcessGroupState>,
    {
        let mut thread_groups = self.read().children().collect::<Vec<_>>();
        let this = self.weak_self.upgrade().unwrap();
        thread_groups.push(this);
        let process_groups =
            thread_groups.iter().map(|tg| Arc::clone(&tg.read().process_group)).unique();
        for pg in process_groups {
            pg.check_orphaned(locked, pids);
        }
    }

    pub fn get_rlimit<L>(&self, locked: &mut Locked<L>, resource: Resource) -> u64
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        self.limits.lock(locked).get(resource).rlim_cur
    }

    /// Adjusts the rlimits of the ThreadGroup to which `target_task` belongs to.
    pub fn adjust_rlimits<L>(
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        target_task: &Task,
        resource: Resource,
        maybe_new_limit: Option<rlimit>,
    ) -> Result<rlimit, Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        let thread_group = target_task.thread_group();
        let can_increase_rlimit = security::is_task_capable_noaudit(current_task, CAP_SYS_RESOURCE);
        let mut limit_state = thread_group.limits.lock(locked);
        let old_limit = limit_state.get(resource);
        if let Some(new_limit) = maybe_new_limit {
            if new_limit.rlim_max > old_limit.rlim_max && !can_increase_rlimit {
                return error!(EPERM);
            }
            security::task_setrlimit(current_task, &target_task, old_limit, new_limit)?;
            limit_state.set(resource, new_limit)
        }
        Ok(old_limit)
    }

    pub fn time_stats(&self) -> TaskTimeStats {
        let process: &zx::Process = if self.process.as_handle_ref().is_invalid() {
            // `process` must be valid for all tasks, except `kthreads`. In that case get the
            // stats from starnix process.
            assert_eq!(
                self as *const ThreadGroup,
                Arc::as_ptr(&self.kernel.kthreads.system_thread_group())
            );
            &self.kernel.kthreads.starnix_process
        } else {
            &self.process
        };

        let info =
            zx::Task::get_runtime_info(process).expect("Failed to get starnix process stats");
        TaskTimeStats {
            user_time: zx::MonotonicDuration::from_nanos(info.cpu_time),
            // TODO(https://fxbug.dev/42078242): How can we calculate system time?
            system_time: zx::MonotonicDuration::default(),
        }
    }

    /// For each task traced by this thread_group that matches the given
    /// selector, acquire its TaskMutableState and ptracees lock and execute the
    /// given function.
    pub fn get_ptracees_and(
        &self,
        selector: &ProcessSelector,
        pids: &PidTable,
        f: &mut dyn FnMut(&Task, &TaskMutableState),
    ) {
        for tracee in self
            .ptracees
            .lock()
            .keys()
            .filter(|tracee_tid| selector.match_tid(**tracee_tid, &pids))
            .map(|tracee_tid| pids.get_task(*tracee_tid))
        {
            if let Ok(task_ref) = tracee {
                let task_state = task_ref.write();
                if task_state.ptrace.is_some() {
                    f(&task_ref, &task_state);
                }
            }
        }
    }

    /// Returns a tracee whose state has changed, so that waitpid can report on
    /// it. If this returns a value, and the pid is being traced, the tracer
    /// thread is deemed to have seen the tracee ptrace-stop for the purposes of
    /// PTRACE_LISTEN.
    pub fn get_waitable_ptracee(
        &self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &mut PidTable,
    ) -> Option<TaskExitState> {
        // This checks to see if the target is a zombie ptracee.
        let waitable_entry = self.write().zombie_ptracees.get_waitable_entry(selector, options);
        match waitable_entry {
            None => (),
            Some((zombie, None)) => {
                return zombie.exit_state().map(|state| state.clone());
            }
            Some((zombie, Some(tg))) => {
                if let Some(tg) = tg.upgrade() {
                    if Arc::as_ptr(&tg) != self as *const Self {
                        tg.notify_child_zombie(pids, &zombie);
                    } else {
                        {
                            let mut state = tg.write();
                            state.children.remove(&zombie.get_pid());
                            state.deferred_zombie_ptracers.retain(|dzp| {
                                dzp.tracee_thread_group_key != zombie.thread_group_key
                            });
                        }

                        pids.remove_task(zombie.tid);
                    };
                }
                return zombie.exit_state().map(|state| state.clone());
            }
        }

        let mut tasks = vec![];

        // This checks to see if the target is a living ptracee
        self.get_ptracees_and(selector, pids, &mut |task: &Task, _| {
            tasks.push(task.weak_self.clone());
        });
        for task in tasks {
            let Some(task_ref) = task.upgrade() else {
                continue;
            };

            let process_state = &mut task_ref.thread_group().write();
            let mut task_state = task_ref.write();
            if task_state
                .ptrace
                .as_ref()
                .is_some_and(|ptrace| ptrace.is_waitable(task_ref.load_stopped(), options))
            {
                // We've identified a potential target.  Need to return either
                // the process's information (if we are in group-stop) or the
                // thread's information (if we are in a different stop).

                // The shared information:
                let info = process_state.tasks.values().next().unwrap().info().clone();
                let uid = info.real_creds().uid;
                let mut exit_status = None;
                let exit_signal = process_state.exit_signal.clone();
                let time_stats =
                    process_state.base.time_stats() + process_state.children_time_stats;
                let task_stopped = task_ref.load_stopped();

                #[derive(PartialEq)]
                enum ExitType {
                    None,
                    Cont,
                    Stop,
                    Kill,
                }
                if process_state.is_waitable() {
                    let ptrace = &mut task_state.ptrace;
                    // The information for processes, if we were in group stop.
                    let process_stopped = process_state.base.load_stopped();
                    let mut fn_type = ExitType::None;
                    if process_stopped == StopState::Awake && options.wait_for_continued {
                        fn_type = ExitType::Cont;
                    }
                    let mut event = ptrace
                        .as_ref()
                        .map_or(PtraceEvent::None, |ptrace| {
                            ptrace.event_data.as_ref().map_or(PtraceEvent::None, |data| data.event)
                        })
                        .clone();
                    // Tasks that are ptrace'd always get stop notifications.
                    if process_stopped == StopState::GroupStopped
                        && (options.wait_for_stopped || ptrace.is_some())
                    {
                        fn_type = ExitType::Stop;
                    }
                    if fn_type != ExitType::None {
                        let siginfo = if options.keep_waitable_state {
                            process_state.last_signal.clone()
                        } else {
                            process_state.last_signal.take()
                        };
                        if let Some(mut siginfo) = siginfo {
                            if task_ref.thread_group().load_stopped() == StopState::GroupStopped
                                && ptrace.as_ref().is_some_and(|ptrace| ptrace.is_seized())
                            {
                                if event == PtraceEvent::None {
                                    event = PtraceEvent::Stop;
                                }
                                siginfo.code |= (PtraceEvent::Stop as i32) << 8;
                            }
                            if siginfo.signal == SIGKILL {
                                fn_type = ExitType::Kill;
                            }
                            exit_status = match fn_type {
                                ExitType::Stop => Some(ExitStatus::Stop(siginfo, event)),
                                ExitType::Cont => Some(ExitStatus::Continue(siginfo, event)),
                                ExitType::Kill => Some(ExitStatus::Kill(siginfo)),
                                _ => None,
                            };
                        }
                        // Clear the wait status of the ptrace, because we're
                        // using the tg status instead.
                        ptrace
                            .as_mut()
                            .map(|ptrace| ptrace.get_last_signal(options.keep_waitable_state));
                    }
                }
                if exit_status == None {
                    if let Some(ptrace) = task_state.ptrace.as_mut() {
                        // The information for the task, if we were in a non-group stop.
                        let mut fn_type = ExitType::None;
                        let event = ptrace
                            .event_data
                            .as_ref()
                            .map_or(PtraceEvent::None, |event| event.event);
                        if task_stopped == StopState::Awake {
                            fn_type = ExitType::Cont;
                        }
                        if task_stopped.is_stopping_or_stopped()
                            || ptrace.stop_status == PtraceStatus::Listening
                        {
                            fn_type = ExitType::Stop;
                        }
                        if fn_type != ExitType::None {
                            if let Some(siginfo) =
                                ptrace.get_last_signal(options.keep_waitable_state)
                            {
                                if siginfo.signal == SIGKILL {
                                    fn_type = ExitType::Kill;
                                }
                                exit_status = match fn_type {
                                    ExitType::Stop => Some(ExitStatus::Stop(siginfo, event)),
                                    ExitType::Cont => Some(ExitStatus::Continue(siginfo, event)),
                                    ExitType::Kill => Some(ExitStatus::Kill(siginfo)),
                                    _ => None,
                                };
                            }
                        }
                    }
                }
                if let Some(exit_status) = exit_status {
                    return Some(TaskExitState {
                        tid: task_ref.get_tid(),
                        uid,
                        status: exit_status,
                        signal: exit_signal,
                        time_stats,
                    });
                }
            }
        }
        None
    }

    /// Attempts to send an unchecked signal to this thread group.
    ///
    /// - `current_task`: The task that is sending the signal.
    /// - `unchecked_signal`: The signal that is to be sent. Unchecked, since `0` is a sentinel value
    /// where rights are to be checked but no signal is actually sent.
    ///
    /// # Returns
    /// Returns Ok(()) if the signal was sent, or the permission checks passed with a 0 signal, otherwise
    /// the error that was encountered.
    pub fn send_signal_unchecked(
        &self,
        current_task: &CurrentTask,
        unchecked_signal: UncheckedSignal,
    ) -> Result<(), Errno> {
        if let Some(signal) = self.check_signal_access(current_task, unchecked_signal)? {
            let signal_info = SignalInfo::with_detail(
                signal,
                SI_USER as i32,
                SignalDetail::Kill {
                    pid: current_task.thread_group().leader,
                    uid: current_task.current_creds().uid,
                },
            );

            self.write().send_signal(signal_info);
        }

        Ok(())
    }

    /// Sends a signal to this thread_group without performing any access checks.
    ///
    /// # Safety
    /// This is unsafe, because it should only be called by tools and tests.
    pub unsafe fn send_signal_unchecked_debug(
        &self,
        current_task: &CurrentTask,
        unchecked_signal: UncheckedSignal,
    ) -> Result<(), Errno> {
        let signal = Signal::try_from(unchecked_signal)?;
        let signal_info = SignalInfo::with_detail(
            signal,
            SI_USER as i32,
            SignalDetail::Kill {
                pid: current_task.thread_group().leader,
                uid: current_task.current_creds().uid,
            },
        );

        self.write().send_signal(signal_info);
        Ok(())
    }

    /// Attempts to send an unchecked signal to this thread group, with info read from
    /// `siginfo_ref`.
    ///
    /// - `current_task`: The task that is sending the signal.
    /// - `unchecked_signal`: The signal that is to be sent. Unchecked, since `0` is a sentinel value
    /// where rights are to be checked but no signal is actually sent.
    /// - `siginfo_ref`: The siginfo that will be enqueued.
    /// - `options`: Options for how to convert the siginfo into a signal info.
    ///
    /// # Returns
    /// Returns Ok(()) if the signal was sent, or the permission checks passed with a 0 signal, otherwise
    /// the error that was encountered.
    #[track_caller]
    pub fn send_signal_unchecked_with_info(
        &self,
        current_task: &CurrentTask,
        unchecked_signal: UncheckedSignal,
        siginfo_ref: UserAddress,
        options: IntoSignalInfoOptions,
    ) -> Result<(), Errno> {
        if let Some(signal) = self.check_signal_access(current_task, unchecked_signal)? {
            let siginfo = UncheckedSignalInfo::read_from_siginfo(current_task, siginfo_ref)?;
            if self.leader != current_task.get_pid()
                && (siginfo.code() >= 0 || siginfo.code() == SI_TKILL)
            {
                return error!(EPERM);
            }

            self.write().send_signal(siginfo.into_signal_info(signal, options)?);
        }

        Ok(())
    }

    /// Checks whether or not `current_task` can signal this thread group with `unchecked_signal`.
    ///
    /// Returns:
    ///   - `Ok(Some(Signal))` if the signal passed checks and should be sent.
    ///   - `Ok(None)` if the signal passed checks, but should not be sent. This is used by
    ///   userspace for permission checks.
    ///   - `Err(_)` if the permission checks failed.
    fn check_signal_access(
        &self,
        current_task: &CurrentTask,
        unchecked_signal: UncheckedSignal,
    ) -> Result<Option<Signal>, Errno> {
        // Pick an arbitrary task in thread_group to check permissions.
        //
        // Tasks can technically have different credentials, but in practice they are kept in sync.
        let target_task = self.read().get_any_task()?;
        current_task.can_signal(&target_task, unchecked_signal)?;

        // 0 is a sentinel value used to do permission checks.
        if unchecked_signal.is_zero() {
            return Ok(None);
        }

        let signal = Signal::try_from(unchecked_signal)?;
        security::check_signal_access(current_task, &target_task, signal)?;

        Ok(Some(signal))
    }

    pub fn has_signal_queued(&self, signal: Signal) -> bool {
        self.pending_signals.lock().has_queued(signal)
    }

    pub fn num_signals_queued(&self) -> usize {
        self.pending_signals.lock().num_queued()
    }

    pub fn get_pending_signals(&self) -> SigSet {
        self.pending_signals.lock().pending()
    }

    pub fn is_any_signal_allowed_by_mask(&self, mask: SigSet) -> bool {
        self.pending_signals.lock().is_any_allowed_by_mask(mask)
    }

    pub fn take_next_signal_where<F>(&self, predicate: F) -> Option<SignalInfo>
    where
        F: Fn(&SignalInfo) -> bool,
    {
        let mut signals = self.pending_signals.lock();
        let r = signals.take_next_where(predicate);
        self.has_pending_signals.store(!signals.is_empty(), Ordering::Relaxed);
        r
    }

    /// Drive this `ThreadGroup` to exit, allowing it time to handle SIGTERM before sending SIGKILL.
    ///
    /// Returns once `ThreadGroup::exit()` has completed.
    ///
    /// Must be called from the system task.
    pub async fn shut_down(this: Weak<Self>) {
        const SHUTDOWN_SIGNAL_HANDLING_TIMEOUT: zx::MonotonicDuration =
            zx::MonotonicDuration::from_seconds(1);

        // Prepare for shutting down the thread group.
        let (tg_name, mut on_exited) = {
            // Nest this upgraded access so upgraded references aren't held across await-points.
            let Some(this) = this.upgrade() else {
                return;
            };

            if this.read().is_exited() {
                return;
            }

            // Register a channel to be notified when exit() is complete.
            let (on_exited_send, on_exited) = futures::channel::oneshot::channel();
            this.write().exit_notifier = Some(on_exited_send);

            // We want to be able to log about this thread group without upgrading the `Weak`.
            let tg_name = format!("{this:?}");

            (tg_name, on_exited)
        };

        log_debug!(tg:% = tg_name; "shutting down thread group, sending SIGTERM");
        this.upgrade().map(|tg| tg.write().send_signal(SignalInfo::kernel(SIGTERM)));

        // Give thread groups some time to handle SIGTERM, proceeding early if they exit
        let timeout = fuchsia_async::Timer::new(SHUTDOWN_SIGNAL_HANDLING_TIMEOUT);
        futures::pin_mut!(timeout);

        // Use select_biased instead of on_timeout() so that we can await on on_exited later
        futures::select_biased! {
            _ = &mut on_exited => (),
            _ = timeout => {
                log_debug!(tg:% = tg_name; "sending SIGKILL");
                this.upgrade().map(|tg| tg.write().send_signal(SignalInfo::kernel(SIGKILL)));
            },
        };

        log_debug!(tg:% = tg_name; "waiting for exit");
        // It doesn't matter whether ThreadGroup::exit() was called or the process exited with
        // a return code and dropped the sender end of the channel.
        on_exited.await.ok();
        log_debug!(tg:% = tg_name; "thread group shutdown complete");
    }

    /// Returns the KOID of the process for this thread group.
    /// This method should be used to when mapping 32 bit linux process ids to KOIDs
    /// to avoid breaking the encapsulation of the zx::process within the ThreadGroup.
    /// This encapsulation is important since the relationship between the ThreadGroup
    /// and the Process may change over time. See [ThreadGroup::process] for more details.
    pub fn get_process_koid(&self) -> Result<Koid, Status> {
        self.process.koid()
    }
}

pub enum WaitableChildResult {
    ReadyNow(Box<TaskExitState>),
    ShouldWait,
    NoneFound,
}

#[apply(state_implementation!)]
impl ThreadGroupMutableState<Base = ThreadGroup> {
    pub fn leader(&self) -> pid_t {
        self.base.leader
    }

    pub fn leader_command(&self) -> TaskCommand {
        self.get_task(self.leader())
            .map(|l| l.command())
            .unwrap_or_else(|| TaskCommand::new(b"<leader exited>"))
    }

    pub fn is_running(&self) -> bool {
        matches!(self.run_state, ThreadGroupRunState::Running)
    }

    pub fn is_exited(&self) -> bool {
        matches!(self.run_state, ThreadGroupRunState::Exited(_))
    }

    pub fn children(&self) -> impl Iterator<Item = Arc<ThreadGroup>> + '_ {
        self.children.values().map(|v| {
            v.upgrade().expect("Weak references to processes in ThreadGroup must always be valid")
        })
    }

    pub fn tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.values().flat_map(|t| t.upgrade()).collect()
    }

    pub fn task_ids(&self) -> impl Iterator<Item = &tid_t> {
        self.tasks.keys()
    }

    pub fn contains_task(&self, tid: tid_t) -> bool {
        self.tasks.contains_key(&tid)
    }

    pub fn get_task(&self, tid: tid_t) -> Option<Arc<Task>> {
        self.tasks.get(&tid).and_then(|t| t.upgrade())
    }

    pub fn tasks_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn get_ppid(&self) -> pid_t {
        match &self.parent {
            Some(parent) => parent.upgrade().leader,
            None => 0,
        }
    }

    fn set_process_group<L>(
        &mut self,
        locked: &mut Locked<L>,
        process_group: Arc<ProcessGroup>,
        pids: &PidTable,
    ) where
        L: LockBefore<ProcessGroupState>,
    {
        if self.process_group == process_group {
            return;
        }
        self.leave_process_group(locked, pids);
        self.process_group = process_group;
        self.process_group.insert(locked, self.base);
    }

    fn leave_process_group<L>(&mut self, locked: &mut Locked<L>, pids: &PidTable)
    where
        L: LockBefore<ProcessGroupState>,
    {
        if self.process_group.remove(locked, self.base) {
            self.process_group.session.write().remove(self.process_group.leader);
            pids.remove_process_group(self.process_group.leader);
        }
    }

    /// Indicates whether the thread group is waitable via waitid and waitpid for
    /// either WSTOPPED or WCONTINUED.
    pub fn is_waitable(&self) -> bool {
        return self.last_signal.is_some() && !self.base.load_stopped().is_in_progress();
    }

    pub fn get_waitable_zombie(
        &mut self,
        zombie_list: &dyn Fn(&mut ThreadGroupMutableState) -> &mut Vec<tid_t>,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &mut PidTable,
    ) -> Option<TaskExitState> {
        // We look for the last zombie in the vector that matches pid selector and waiting options
        let selected_zombie_position = zombie_list(self)
            .iter()
            .rev()
            .position(|&tid| {
                pids.get_task(tid)
                    .map_or(false, |task| selector.match_task_and_waiting_options(&task, options))
            })
            .map(|position_starting_from_the_back| {
                zombie_list(self).len() - 1 - position_starting_from_the_back
            });

        selected_zombie_position.map(|position| {
            let tid = if options.keep_waitable_state {
                zombie_list(self)[position]
            } else {
                zombie_list(self).remove(position)
            };

            let zombie = pids.get_task(tid).expect("Zombie task must not yet be reaped");
            let result = zombie.exit_state().expect("Zombie task must have exited").clone();

            if !options.keep_waitable_state {
                self.children_time_stats += zombie.time_stats();
                let tid = zombie.tid;
                drop(zombie);
                pids.remove_task(tid);
            }

            result
        })
    }

    pub fn is_correct_exit_signal(for_clone: bool, exit_code: Option<Signal>) -> bool {
        for_clone == (exit_code != Some(SIGCHLD))
    }

    fn get_waitable_running_children(
        &self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &PidTable,
    ) -> WaitableChildResult {
        // The children whose pid matches the pid selector queried.
        let filter_children_by_pid_selector = |child: &ThreadGroup| match *selector {
            ProcessSelector::Any => true,
            ProcessSelector::Pid(pid) => child.leader == pid,
            ProcessSelector::Pgid(pgid) => {
                pids.get_process_group(pgid).as_ref() == Some(&child.read().process_group)
            }
            ProcessSelector::Process(ref key) => *key == ThreadGroupKey::from(child),
        };

        // The children whose exit signal matches the waiting options queried.
        let filter_children_by_waiting_options = |child: &ThreadGroup| {
            if options.wait_for_all {
                return true;
            }
            Self::is_correct_exit_signal(options.wait_for_clone, child.read().exit_signal)
        };

        // If wait_for_exited flag is disabled or no exited children were found we look for living children.
        let mut selected_children = self
            .children
            .values()
            .map(|t| t.upgrade().unwrap())
            .filter(|tg| filter_children_by_pid_selector(&tg))
            .filter(|tg| filter_children_by_waiting_options(&tg))
            .peekable();
        if selected_children.peek().is_none() {
            // There still might be a process that ptrace hasn't looked at yet.
            if self.deferred_zombie_ptracers.iter().any(|dzp| match *selector {
                ProcessSelector::Any => true,
                ProcessSelector::Pid(pid) => dzp.tracee_thread_group_key.pid() == pid,
                ProcessSelector::Pgid(pgid) => pgid == dzp.tracee_pgid,
                ProcessSelector::Process(ref key) => *key == dzp.tracee_thread_group_key,
            }) {
                return WaitableChildResult::ShouldWait;
            }

            return WaitableChildResult::NoneFound;
        }
        for child in selected_children {
            let child = child.write();
            if child.last_signal.is_some() {
                let build_wait_result = |mut child: ThreadGroupWriteGuard<'_>,
                                         exit_status: &dyn Fn(SignalInfo) -> ExitStatus|
                 -> TaskExitState {
                    let siginfo = if options.keep_waitable_state {
                        child.last_signal.clone().unwrap()
                    } else {
                        child.last_signal.take().unwrap()
                    };
                    let exit_status = if siginfo.signal == SIGKILL {
                        // This overrides the stop/continue choice.
                        ExitStatus::Kill(siginfo)
                    } else {
                        exit_status(siginfo)
                    };
                    let task_container = child.tasks.values().next().unwrap();
                    let uid = task_container.1.real_creds().uid;
                    TaskExitState {
                        tid: child.base.leader,
                        uid,
                        status: exit_status,
                        signal: child.exit_signal.clone(),
                        time_stats: child.base.time_stats() + child.children_time_stats,
                    }
                };
                let child_stopped = child.base.load_stopped();
                if child_stopped == StopState::Awake && options.wait_for_continued {
                    return WaitableChildResult::ReadyNow(Box::new(build_wait_result(
                        child,
                        &|siginfo| ExitStatus::Continue(siginfo, PtraceEvent::None),
                    )));
                }
                if child_stopped == StopState::GroupStopped && options.wait_for_stopped {
                    return WaitableChildResult::ReadyNow(Box::new(build_wait_result(
                        child,
                        &|siginfo| ExitStatus::Stop(siginfo, PtraceEvent::None),
                    )));
                }
            }
        }

        WaitableChildResult::ShouldWait
    }

    /// Returns any waitable child matching the given `selector` and `options`. Returns None if no
    /// child matching the selector is waitable. Returns ECHILD if no child matches the selector at
    /// all.
    ///
    /// Will remove the waitable status from the child depending on `options`.
    pub fn get_waitable_child(
        &mut self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &mut PidTable,
    ) -> WaitableChildResult {
        if options.wait_for_exited {
            if let Some(waitable_zombie) = self.get_waitable_zombie(
                &|state: &mut ThreadGroupMutableState| &mut state.zombie_children,
                selector,
                options,
                pids,
            ) {
                return WaitableChildResult::ReadyNow(Box::new(waitable_zombie));
            }
        }

        self.get_waitable_running_children(selector, options, pids)
    }

    /// Returns a task in the current thread group.
    pub fn get_live_task(&self) -> Result<Arc<Task>, Errno> {
        self.tasks
            .iter()
            .find_map(|container| container.1.upgrade().filter(|task| task.live().is_ok()))
            .ok_or_else(|| errno!(ESRCH))
    }

    /// Returns a task representative of the [`ThreadGroup`].
    ///
    /// If the task list contains at least one live task, an arbitrary live task is returned.
    /// Otherwise, if the task list is empty, the process must be a zombie. In this case, the exited
    /// leader task is returned.
    pub fn get_any_task(&self) -> Result<Arc<Task>, Errno> {
        self.get_live_task()
            .ok()
            .or_else(|| self.base.leader_task.get().and_then(|t| t.upgrade()))
            .ok_or_else(|| errno!(ESRCH))
    }

    /// Set the stop status of the process.  If you pass |siginfo| of |None|,
    /// does not update the signal.  If |finalize_only| is set, will check that
    /// the set will be a finalize (Stopping -> Stopped or Stopped -> Stopped)
    /// before executing it.
    ///
    /// Returns the latest stop state after any changes.
    pub fn set_stopped(
        mut self,
        new_stopped: StopState,
        siginfo: Option<SignalInfo>,
        finalize_only: bool,
    ) -> StopState {
        if let Some(stopped) = self.base.check_stopped_state(new_stopped, finalize_only) {
            return stopped;
        }

        // Thread groups don't transition to group stop if they are waking, because waking
        // means something told it to wake up (like a SIGCONT) but hasn't finished yet.
        if self.base.load_stopped() == StopState::Waking
            && (new_stopped == StopState::GroupStopping || new_stopped == StopState::GroupStopped)
        {
            return self.base.load_stopped();
        }

        // TODO(https://g-issues.fuchsia.dev/issues/306438676): When thread
        // group can be stopped inside user code, tasks/thread groups will
        // need to be either restarted or stopped here.
        self.store_stopped(new_stopped);
        if let Some(signal) = &siginfo {
            // We don't want waiters to think the process was unstopped
            // because of a sigkill.  They will get woken when the
            // process dies.
            if signal.signal != SIGKILL {
                self.last_signal = siginfo;
            }
        }
        if new_stopped == StopState::Waking || new_stopped == StopState::ForceWaking {
            self.lifecycle_waiters.notify_value(ThreadGroupLifecycleWaitValue::Stopped);
        };

        let parent = (!new_stopped.is_in_progress()).then(|| self.parent.clone()).flatten();

        // Drop the lock before locking the parent.
        std::mem::drop(self);
        if let Some(parent) = parent {
            let parent = parent.upgrade();
            parent
                .write()
                .lifecycle_waiters
                .notify_value(ThreadGroupLifecycleWaitValue::ChildStatus);
        }

        new_stopped
    }

    fn store_stopped(&mut self, state: StopState) {
        // We don't actually use the guard but we require it to enforce that the
        // caller holds the thread group's mutable state lock (identified by
        // mutable access to the thread group's mutable state).

        self.base.stop_state.store(state, Ordering::Relaxed)
    }

    /// Sends the signal `signal_info` to this thread group.
    #[allow(unused_mut, reason = "needed for some but not all macro outputs")]
    pub fn send_signal(mut self, signal_info: SignalInfo) {
        let sigaction = self.base.signal_actions.get(signal_info.signal);
        let action = action_for_signal(&signal_info, sigaction);

        {
            let mut pending_signals = self.base.pending_signals.lock();
            pending_signals.enqueue(signal_info.clone());
            self.base.has_pending_signals.store(true, Ordering::Relaxed);
        }
        let tasks: Vec<Weak<Task>> = self.tasks.values().map(|t| t.weak_clone()).collect();

        // Set state to waking before interrupting any tasks.
        if signal_info.signal == SIGKILL {
            self.set_stopped(StopState::ForceWaking, Some(signal_info.clone()), false);
        } else if signal_info.signal == SIGCONT {
            self.set_stopped(StopState::Waking, Some(signal_info.clone()), false);
        }

        let mut has_interrupted_task = false;
        for task in tasks.iter().flat_map(|t| t.upgrade()) {
            let mut task_state = task.write();

            if signal_info.signal == SIGKILL {
                task_state.thaw();
                task_state.set_stopped(StopState::ForceWaking, None, None, None);
            } else if signal_info.signal == SIGCONT {
                task_state.set_stopped(StopState::Waking, None, None, None);
            }

            let is_masked = task_state.is_signal_masked(signal_info.signal);
            let was_masked = task_state.is_signal_masked_by_saved_mask(signal_info.signal);

            let is_queued = action != DeliveryAction::Ignore
                || is_masked
                || was_masked
                || task_state.is_ptraced();

            if is_queued {
                task_state.notify_signal_waiters(&signal_info.signal);

                if !is_masked && action.must_interrupt(Some(sigaction)) && !has_interrupted_task {
                    // Only interrupt one task, and only interrupt if the signal was actually queued
                    // and the action must interrupt.
                    drop(task_state);
                    task.interrupt();
                    has_interrupted_task = true;
                }
            }
        }
    }
}

/// Container around a weak task and a strong `TaskPersistentInfo`. It is needed to keep the
/// information even when the task is not upgradable, because when the task is dropped, there is a
/// moment where the task is not yet released, yet the weak pointer is not upgradeable anymore.
/// During this time, it is still necessary to access the persistent info to compute the state of
/// the thread for the different wait syscalls.
pub struct TaskContainer(Weak<Task>, TaskPersistentInfo);

impl From<&Arc<Task>> for TaskContainer {
    fn from(task: &Arc<Task>) -> Self {
        Self(Arc::downgrade(task), task.persistent_info.clone())
    }
}

impl From<TaskContainer> for TaskPersistentInfo {
    fn from(container: TaskContainer) -> TaskPersistentInfo {
        container.1
    }
}

impl TaskContainer {
    fn upgrade(&self) -> Option<Arc<Task>> {
        self.0.upgrade()
    }

    fn weak_clone(&self) -> Weak<Task> {
        self.0.clone()
    }

    fn info(&self) -> &TaskPersistentInfo {
        &self.1
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::testing::*;

    #[::fuchsia::test]
    async fn test_shut_down() {
        spawn_kernel_and_run(async |locked, current_task| {
            let child = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            let weak_tg = Arc::downgrade(child.thread_group());
            child.thread_group().exit(locked, ExitStatus::Exit(0), None);
            std::mem::drop(child);
            // Verify that shut_down completes successfully and doesn't hang.
            ThreadGroup::shut_down(weak_tg).await;
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setsid() {
        spawn_kernel_and_run(async |locked, current_task| {
            fn get_process_group(task: &Task) -> Arc<ProcessGroup> {
                Arc::clone(&task.thread_group().read().process_group)
            }
            assert_eq!(current_task.thread_group().setsid(locked), error!(EPERM));

            let child_task = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            assert_eq!(get_process_group(&current_task), get_process_group(&child_task));

            let old_process_group = child_task.thread_group().read().process_group.clone();
            assert_eq!(child_task.thread_group().setsid(locked), Ok(()));
            assert_eq!(
                child_task.thread_group().read().process_group.session.leader,
                child_task.get_pid()
            );
            assert!(
                !old_process_group.read(locked).thread_groups().contains(child_task.thread_group())
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_exit_status() {
        spawn_kernel_and_run(async |locked, current_task| {
            let child = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            child.thread_group().exit(locked, ExitStatus::Exit(42), None);
            std::mem::drop(child);
            let tg = current_task.thread_group().read();
            let tid = tg.zombie_children[0];
            let zombie = current_task.kernel().pids.read().get_task(tid).unwrap();
            let exit_state = zombie.exit_state().unwrap();
            assert_eq!(exit_state.status, ExitStatus::Exit(42));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setgpid() {
        spawn_kernel_and_run(async |locked, current_task| {
            assert_eq!(current_task.thread_group().setsid(locked), error!(EPERM));

            let child_task1 = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            let child_task2 = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            let execd_child_task = current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            execd_child_task.thread_group().write().did_exec = true;
            let other_session_child_task =
                current_task.clone_task_for_test(locked, 0, Some(SIGCHLD));
            assert_eq!(other_session_child_task.thread_group().setsid(locked), Ok(()));

            assert_eq!(
                child_task1.thread_group().setpgid(locked, &current_task, &current_task, 0),
                error!(ESRCH)
            );
            assert_eq!(
                current_task.thread_group().setpgid(locked, &current_task, &execd_child_task, 0),
                error!(EACCES)
            );
            assert_eq!(
                current_task.thread_group().setpgid(locked, &current_task, &current_task, 0),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    locked,
                    &current_task,
                    &other_session_child_task,
                    0
                ),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(locked, &current_task, &child_task1, -1),
                error!(EINVAL)
            );
            assert_eq!(
                current_task.thread_group().setpgid(locked, &current_task, &child_task1, 255),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    locked,
                    &current_task,
                    &child_task1,
                    other_session_child_task.tid
                ),
                error!(EPERM)
            );

            assert_eq!(
                child_task1.thread_group().setpgid(locked, &current_task, &child_task1, 0),
                Ok(())
            );
            assert_eq!(
                child_task1.thread_group().read().process_group.session.leader,
                current_task.tid
            );
            assert_eq!(child_task1.thread_group().read().process_group.leader, child_task1.tid);

            let old_process_group = child_task2.thread_group().read().process_group.clone();
            assert_eq!(
                current_task.thread_group().setpgid(
                    locked,
                    &current_task,
                    &child_task2,
                    child_task1.tid
                ),
                Ok(())
            );
            assert_eq!(child_task2.thread_group().read().process_group.leader, child_task1.tid);
            assert!(
                !old_process_group
                    .read(locked)
                    .thread_groups()
                    .contains(child_task2.thread_group())
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_adopt_children() {
        spawn_kernel_and_run(async |locked, current_task| {
            let task1 = current_task.clone_task_for_test(locked, 0, None);
            let task2 = task1.clone_task_for_test(locked, 0, None);
            let task3 = task2.clone_task_for_test(locked, 0, None);

            assert_eq!(task3.thread_group().read().get_ppid(), task2.tid);

            task2.thread_group().exit(locked, ExitStatus::Exit(0), None);
            std::mem::drop(task2);

            // Task3 parent should be current_task.
            assert_eq!(task3.thread_group().read().get_ppid(), current_task.tid);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_getppid_after_self_and_parent_exit() {
        spawn_kernel_and_run(async |locked, current_task| {
            let task1 = current_task.clone_task_for_test(locked, 0, None);
            let task2 = task1.clone_task_for_test(locked, 0, None);

            // Take strong references to the ThreadGroups.
            let tg1 = task1.thread_group().clone();
            let tg2 = task2.thread_group().clone();

            assert_eq!(tg1.read().get_ppid(), current_task.tid);
            assert_eq!(tg2.read().get_ppid(), task1.tid);

            // Exit `task2` first, so that when `task1` exits, it will not be reparented to init.
            tg2.exit(locked, ExitStatus::Exit(0), None);
            std::mem::drop(task2);

            // Exit `task1`, and drop the task and ThreadGroup.
            tg1.exit(locked, ExitStatus::Exit(0), None);
            std::mem::drop(task1);
            std::mem::drop(tg1);

            // It should still be valid to call `get_ppid()` on `tg2`, though is parent ThreadGroup
            // no longer exists.
            let _ = tg2.read().get_ppid();
        })
        .await;
    }
}
