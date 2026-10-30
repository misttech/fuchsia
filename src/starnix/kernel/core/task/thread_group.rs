// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::terminal::{ControllingSession, ControllingTerminal, Terminal, TerminalSide};
use crate::mutable_state::{state_accessor, state_implementation};
use crate::ptrace::{
    AtomicStopState, PtraceAllowedPtracers, PtraceEvent, PtraceOptions, PtraceStatus, PtraceTracer,
    StopState, ZombiePtracees, ptrace_detach,
};
use crate::security;
use crate::signals::syscalls::WaitingOptions;
use crate::signals::{
    DeliveryAction, IntoSignalInfoOptions, QueuedSignals, SignalActions, SignalDetail, SignalInfo,
    UncheckedSignalInfo, action_for_signal, send_standard_signal,
};
use crate::task::memory_attribution::MemoryAttributionLifecycleEvent;
use crate::task::{
    CurrentTask, ExitStatus, Kernel, Pid, PidEntry, PidTableGuard, Task, TaskFlags,
    TaskMutableState, TaskPersistentInfo, TypedWaitQueue, WaitResult, ZombieProcess, ZombieState,
};
use crate::time::{IntervalTimerHandle, TimerTable};
use fuchsia_rcu::subtle::RcuPtr;
use fuchsia_rcu::{RcuDroppable, RcuDroppableArc, RcuReadScope};
use fuchsia_rcu_collections::rcu_intrusive_list::{Link, RcuListAdapter, rcu_list_adapter};
use itertools::Itertools;
use macro_rules_attribute::apply;
use starnix_lifecycle::{AtomicCounter, DropNotifier};
use starnix_logging::{log_debug, log_error, log_info, log_warn, track_stub};
use starnix_sync::{
    LockDepMutex, LockDepRwLock, ThreadGroupLimits, ThreadGroupMutableStateLock,
    ThreadGroupPendingSignalsLock, ThreadGroupPtraceesLock, allow_subclass,
};
use starnix_task_command::TaskCommand;
use starnix_types::ownership::{OwnedRef, Releasable};
use starnix_types::stats::TaskTimeStats;
use starnix_types::time::{itimerspec_from_itimerval, timeval_from_duration};
use starnix_uapi::auth::{CAP_SYS_ADMIN, CAP_SYS_RESOURCE};
use starnix_uapi::errors::Errno;
use starnix_uapi::personality::PersonalityFlags;
use starnix_uapi::resource_limits::{Resource, ResourceLimits};
use starnix_uapi::signals::{
    SIGCHLD, SIGCONT, SIGHUP, SIGKILL, SIGTERM, SIGTTOU, SigSet, Signal, UncheckedSignal,
};
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    ITIMER_PROF, ITIMER_REAL, ITIMER_VIRTUAL, SA_NOCLDWAIT, SI_USER, SIG_IGN, errno, error,
    itimerval, pid_t, rlimit, tid_t,
};
use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use zx::{Koid, Status};

const INIT_PID: pid_t = 1;

#[derive(Debug)]
pub struct ZirconProcess {
    process: zx::Process,
    koid: Result<Koid, Status>,
}

impl ZirconProcess {
    pub fn new(process: zx::Process) -> Self {
        let koid = process.koid();
        Self { process, koid }
    }

    pub fn koid(&self) -> Result<Koid, Status> {
        self.koid
    }
}

impl std::ops::Deref for ZirconProcess {
    type Target = zx::Process;
    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

/// Values used for waiting on the [ThreadGroup] lifecycle wait queue.
#[repr(u64)]
pub enum ThreadGroupLifecycleWaitValue {
    /// Wait for updates to the WaitResults of tasks in the group.
    ChildStatus,
    /// Wait for updates to `stopped`.
    Stopped,
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
    pub tracer_pid: Pid,
    /// Tracee tid
    pub tracee_tid: Pid,
    /// Tracee thread group
    pub tracee_pid: Pid,
    /// Tracee thread group, used to resolve the tracee's current process group.
    pub tracee_thread_group: Weak<ThreadGroup>,
}

impl DeferredZombiePTracer {
    fn new(tracer: &ThreadGroup, tracee: &Task) -> Self {
        Self {
            tracer_pid: tracer.leader.clone(),
            tracee_tid: tracee.tid.clone(),
            tracee_pid: tracee.pid.clone(),
            tracee_thread_group: Arc::downgrade(tracee.thread_group()),
        }
    }

    /// Returns the tracee's current process group ID, if its thread group is still alive.
    fn tracee_pgid(&self) -> Option<Pid> {
        self.tracee_thread_group.upgrade().map(|thread_group| thread_group.pgid())
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

    /// Parent death [`Signal`] of this process, set by `prctl(PR_SET_PDEATHSIG)`.
    ///
    /// Cleared on `fork(2)`, upon changes to effective or filesystem UID/GID, and
    /// during `execve(2)` if the executed file is set-user-ID, set-group-ID, or
    /// has associated capabilities.
    parent_death_signal: Option<Signal>,

    /// The tasks in the thread group.
    ///
    /// The references to Task is weak to prevent cycles as Task have a Arc reference to their
    /// thread group.
    /// It is still expected that these weak references are always valid, as tasks must unregister
    /// themselves before they are deleted.
    tasks: HashSet<TaskPersistentInfo>,

    /// The children of this thread group.
    ///
    /// Holds `Pid` references to prevent cycles. Child thread groups must unregister
    /// themselves before they are deleted.
    pub children: HashSet<Pid>,

    /// Child tasks that have exited, but not yet been waited for.
    pub zombie_children: Vec<OwnedRef<ZombieProcess>>,

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

    /// The session leader PID.
    pub session: Pid,

    /// The controlling terminal of the thread group.
    pub controlling_terminal: Option<ControllingTerminal>,

    pub did_exec: bool,

    /// A signal that indicates whether the process is going to become waitable
    /// via waitid and waitpid for either WSTOPPED or WCONTINUED, depending on
    /// the value of `stopped`. If not None, contains the SignalInfo to return.
    pub last_signal: Option<SignalInfo>,

    /// Whether the `ThreadGroup` is running or not.
    ///
    /// For exited thread groups, this contains the exit status.
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

/// Wrapper around a [`ThreadGroup`]'s process group [`PidEntry`] that allows lock-free RCU
/// reads while requiring both the [`PidTable`](crate::task::PidTable) and [`ThreadGroup`] mutable
/// state locks to be held when updating.
#[derive(Debug)]
struct ProcessGroup(RcuDroppableArc<PidEntry>);

impl ProcessGroup {
    fn new(process_group: Pid) -> Self {
        Self(RcuDroppableArc::new(process_group))
    }

    fn get(&self) -> Pid {
        self.0.to_arc()
    }

    fn update(&self, new_pgid: Pid, pids: &PidTableGuard<'_>, state: &ThreadGroupWriteGuard<'_>) {
        debug_assert!(
            std::ptr::eq(self, &state.base.process_group),
            "ThreadGroupWriteGuard must belong to the ThreadGroup whose process_group is being updated"
        );
        debug_assert!(
            std::ptr::eq(&**pids, &state.base.kernel.pids),
            "PidTableGuard must belong to the Kernel of the ThreadGroup being updated"
        );
        self.0.update(new_pgid);
    }
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
    pub process: ZirconProcess,

    /// A handle to the restricted address space for the Zircon process object.
    pub root_vmar: zx::Vmar,

    /// The lead task of this thread group.
    ///
    /// The lead task is typically the initial thread created in the thread group.
    pub leader: Pid,

    /// The process group used for job control and signal delivery.
    ///
    /// # Thread Safety
    ///
    /// Must only be modified while holding the `PidTable` and `mutable_state` locks.
    process_group: ProcessGroup,

    /// Current node in `PidEntry::pgid_thread_groups`. Protected by `PidTableLock`.
    pub(super) pgrp_node: RcuPtr<ThreadGroupLinkNode>,

    /// Current node in `PidEntry::sid_thread_groups`. Protected by `PidTableLock`.
    pub(super) session_node: RcuPtr<ThreadGroupLinkNode>,

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
    mutable_state: LockDepRwLock<ThreadGroupMutableState, ThreadGroupMutableStateLock>,

    /// The resource limits for this thread group.  This is outside mutable_state
    /// to avoid deadlocks where the thread_group lock is held when acquiring
    /// the task lock, and vice versa.
    pub limits: LockDepMutex<ResourceLimits, ThreadGroupLimits>,

    /// The next unique identifier for a seccomp filter.  These are required to be
    /// able to distinguish identical seccomp filters, which are treated differently
    /// for the purposes of SECCOMP_FILTER_FLAG_TSYNC.  Inherited across clone because
    /// seccomp filters are also inherited across clone.
    pub next_seccomp_filter_id: AtomicCounter<u64>,

    /// Tasks ptraced by this process
    pub ptracees: LockDepMutex<HashSet<TaskPersistentInfo>, ThreadGroupPtraceesLock>,

    /// The signals that are currently pending for this thread group.
    pub pending_signals: LockDepMutex<QueuedSignals, ThreadGroupPendingSignalsLock>,

    /// Whether or not there are any pending signals available for tasks in this thread group.
    /// Used to avoid having to acquire the signal state lock in hot paths.
    pub has_pending_signals: AtomicBool,

    /// The monotonic time at which the thread group started.
    pub start_time: zx::MonotonicInstant,

    /// Whether to log syscalls at INFO level for this thread group.
    log_syscalls_as_info: AtomicBool,

    /// Whether this process was created during shutdown, which only deferred shutdown processes
    /// may do. See `create_zircon_process()`.
    pub created_during_shutdown: AtomicBool,
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
    pub fn to_owned(&self) -> Arc<Self> {
        self.weak_self.upgrade().expect("ThreadGroup is alive")
    }

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

/// Non-reusable list node linking a [`ThreadGroup`] into a process group or session list.
///
/// Allocated per list attachment (`attach_pgid` / `attach_sid`) and retired via `rcu_drop`
/// upon detachment so moving a process between process groups or sessions never clobbers
/// `link.next` for concurrent lock-free RCU readers.
#[derive(Debug, RcuDroppable)]
pub(super) struct ThreadGroupLinkNode {
    pub(super) thread_group: Weak<ThreadGroup>,
    pub(super) link: Link,
}

impl ThreadGroupLinkNode {
    pub(super) fn new(thread_group: Weak<ThreadGroup>) -> Self {
        Self { thread_group, link: Link::default() }
    }
}

/// RCU intrusive list adapter for [`ThreadGroupLinkNode`].
#[derive(Debug, RcuDroppable)]
pub(super) struct ThreadGroupLinkAdapter;

impl RcuListAdapter<ThreadGroupLinkNode> for ThreadGroupLinkAdapter {
    rcu_list_adapter!(ThreadGroupLinkNode, link);
}

impl Drop for ThreadGroup {
    fn drop(&mut self) {
        let state = self.mutable_state.get_mut();
        assert!(state.tasks.is_empty());
        assert!(state.children.is_empty());
        assert!(state.zombie_children.is_empty());
        assert!(state.zombie_ptracees.is_empty());
        #[cfg(any(test, debug_assertions))]
        {
            let scope = fuchsia_rcu::RcuReadScope::new();
            assert!(self.pgrp_node.read(&scope).is_null());
            assert!(self.session_node.read(&scope).is_null());
            assert!(
                state
                    .parent
                    .as_ref()
                    .and_then(|p| p
                        .thread_group
                        .upgrade()
                        .map(|p| !p.read().children.contains(&self.leader)))
                    .unwrap_or(true)
            );
        }
    }
}

/// Parent process reference for a [`ThreadGroup`], tracking both the parent
/// [`ThreadGroup`] and, prior to reparenting, the specific [`Task`] within that
/// parent that created the child process.
#[derive(Clone)]
pub struct ThreadGroupParent {
    /// Weak reference to the current parent [`ThreadGroup`].
    thread_group: Weak<ThreadGroup>,

    /// Specific [`Task`] within the original parent [`ThreadGroup`] that created this child
    /// process, or `None` once the child has been reparented to an ancestor reaper.
    creator_task: Option<Weak<Task>>,
}

impl ThreadGroupParent {
    /// Creates a [`ThreadGroupParent`] referencing `thread_group`, defaulting `creator_task`
    /// to the leader [`Task`] of `thread_group`.
    ///
    /// When a child process is created by a specific thread via `clone(2)` / `fork(2)`, callers
    /// override `creator_task` via [`Self::set_creator_task`] with the actual creating [`Task`]
    /// (or the inherited `creator_task` when `CLONE_PARENT` is specified).
    pub fn new(thread_group: Weak<ThreadGroup>) -> Self {
        let upgraded = thread_group.upgrade();
        debug_assert!(upgraded.is_some());
        let creator_task =
            upgraded.and_then(|tg| tg.leader.get_task().ok()).map(|t| t.weak_self.clone());
        Self { thread_group, creator_task }
    }

    /// Creates a [`ThreadGroupParent`] for a child reparented to `thread_group` after its
    /// previous parent process terminated.
    ///
    /// Sets `creator_task` to `None` so that subsequent `PR_SET_PDEATHSIG` delivery is triggered
    /// only if `thread_group` is a subreaper and only when `thread_group` itself terminates as a
    /// process, rather than when individual threads within the reaper exit.
    pub(super) fn from_reaper(thread_group: Weak<ThreadGroup>) -> Self {
        debug_assert!(thread_group.upgrade().is_some());
        Self { thread_group, creator_task: None }
    }

    /// Upgrades the parent reference to an [`Arc<ThreadGroup>`].
    pub fn upgrade(&self) -> Arc<ThreadGroup> {
        self.thread_group.upgrade().expect("ThreadGroupParent references must always be valid")
    }

    /// Returns the specific [`Task`] in the original parent [`ThreadGroup`] that created this
    /// process, or `None` if this process has been reparented.
    pub(super) fn creator_task(&self) -> Option<&Weak<Task>> {
        self.creator_task.as_ref()
    }

    /// Sets the [`Task`] in the parent [`ThreadGroup`] whose termination triggers
    /// `PR_SET_PDEATHSIG` delivery for this child process.
    pub(super) fn set_creator_task(&mut self, creator_task: Option<Weak<Task>>) {
        self.creator_task = creator_task;
    }
}

/// A selector that can match a process. Works as a representation of the pid argument to syscalls
/// like wait and kill.
#[derive(Debug, Clone)]
pub enum ProcessSelector {
    /// Matches any process at all.
    Any,
    /// Matches only the process with the specified pid
    Pid(Pid),
    /// Matches all the processes in the given process group
    Pgid(Pid),
}

impl ProcessSelector {
    pub fn match_tid(&self, tid: &Pid) -> bool {
        match self {
            ProcessSelector::Pid(pid) => {
                if pid == tid {
                    true
                } else if let Ok(task_ref) = tid.get_task() {
                    &task_ref.pid == pid
                } else {
                    false
                }
            }
            ProcessSelector::Any => true,
            ProcessSelector::Pgid(pgid) => {
                if let Ok(task_ref) = tid.get_task() {
                    &task_ref.thread_group().pgid() == pgid
                } else {
                    false
                }
            }
        }
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
    pub fn new(
        kernel: Arc<Kernel>,
        process: zx::Process,
        root_vmar: zx::Vmar,
        parent: Option<ThreadGroupWriteGuard<'_>>,
        leader: Pid,
        exit_signal: Option<Signal>,
        signal_actions: Arc<SignalActions>,
    ) -> Arc<ThreadGroup> {
        debug_assert!(!process.is_invalid());
        debug_assert!(!root_vmar.is_invalid());
        Self::new_internal(kernel, process, root_vmar, parent, leader, exit_signal, signal_actions)
    }

    /// Creates a ThreadGroup for a kernel system task (e.g., kthreadd).
    pub fn for_system(kernel: Arc<Kernel>, leader: Pid) -> Arc<ThreadGroup> {
        Self::new_internal(
            kernel,
            zx::Process::invalid(),
            zx::Vmar::invalid(),
            None,
            leader,
            Some(SIGCHLD),
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
    pub fn for_test(
        kernel: Arc<Kernel>,
        process: zx::Process,
        parent: ThreadGroupWriteGuard<'_>,
        leader: Pid,
    ) -> Arc<ThreadGroup> {
        Self::new_internal(
            kernel,
            process,
            zx::Vmar::invalid(),
            Some(parent),
            leader,
            Some(SIGCHLD),
            SignalActions::default(),
        )
    }

    fn new_internal(
        kernel: Arc<Kernel>,
        process: zx::Process,
        root_vmar: zx::Vmar,
        parent: Option<ThreadGroupWriteGuard<'_>>,
        leader: Pid,
        exit_signal: Option<Signal>,
        signal_actions: Arc<SignalActions>,
    ) -> Arc<ThreadGroup> {
        Arc::new_cyclic(|weak_self| {
            let process = ZirconProcess::new(process);
            let process_group =
                parent.as_ref().map(|p| p.base.pgid()).unwrap_or_else(|| leader.clone());
            let session =
                parent.as_ref().map(|p| p.session.clone()).unwrap_or_else(|| leader.clone());
            let mut thread_group = ThreadGroup {
                weak_self: weak_self.clone(),
                kernel,
                process,
                root_vmar,
                leader,
                process_group: ProcessGroup::new(process_group),
                pgrp_node: RcuPtr::null(),
                session_node: RcuPtr::null(),
                signal_actions,
                timers: Default::default(),
                drop_notifier: Default::default(),
                // A child process created via fork(2) inherits its parent's
                // resource limits.  Resource limits are preserved across execve(2).
                // Root tasks without a parent (such as `init` and `kthreadd`) start
                // with the system default resource limits, with `RLIMIT_NPROC` and
                // `RLIMIT_SIGPENDING` scaled from the system's total physical memory.
                limits: LockDepMutex::new(
                    parent.as_ref().map(|p| p.base.limits.lock().clone()).unwrap_or_else(|| {
                        ResourceLimits::from_total_ram(zx::system_get_physmem())
                    }),
                ),
                next_seccomp_filter_id: Default::default(),
                ptracees: Default::default(),
                stop_state: AtomicStopState::new(StopState::Awake),
                pending_signals: Default::default(),
                has_pending_signals: Default::default(),
                start_time: zx::MonotonicInstant::get(),
                mutable_state: ThreadGroupMutableState {
                    parent: parent
                        .as_ref()
                        .map(|p| ThreadGroupParent::new(p.base.weak_self.clone())),
                    exit_signal,
                    // From <https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html>:
                    //
                    //   The parent-death signal setting is cleared for the child of a
                    //   fork(2).
                    parent_death_signal: None,
                    tasks: HashSet::new(),
                    children: HashSet::new(),
                    zombie_children: vec![],
                    zombie_ptracees: ZombiePtracees::new(),
                    deferred_zombie_ptracers: vec![],
                    lifecycle_waiters: TypedWaitQueue::<ThreadGroupLifecycleWaitValue>::default(),
                    is_child_subreaper: false,
                    session,
                    controlling_terminal: None,
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
                }
                .into(),
                log_syscalls_as_info: AtomicBool::new(false),
                created_during_shutdown: AtomicBool::new(false),
            };

            if let Some(mut parent) = parent {
                thread_group.next_seccomp_filter_id.reset(parent.base.next_seccomp_filter_id.get());
                parent.children.insert(thread_group.leader.clone());
            };
            thread_group
        })
    }

    state_accessor!(ThreadGroup, mutable_state);

    pub fn load_stopped(&self) -> StopState {
        self.stop_state.load(Ordering::Relaxed)
    }

    /// Returns `true` if this [`ThreadGroup`] is the `init` process (PID 1).
    fn is_init(&self) -> bool {
        // TODO(https://fxbug.dev/519607444): Support per-PID-namespace `init` tasks once
        // `CLONE_NEWPID` is implemented.
        self.leader.id == INIT_PID
    }

    /// Returns `true` if `signal_info` should be ignored because this [`ThreadGroup`] is `init`
    /// (PID 1).
    pub(crate) fn ignores_signal(&self, signal_info: &SignalInfo) -> bool {
        // From <https://man7.org/linux/man-pages/man2/kill.2.html>:
        //
        //   The only signals that can be sent to process ID 1, the init
        //   process, are those for which init has explicitly installed
        //   signal handlers.
        //
        // Because `SIGKILL` and `SIGSTOP` cannot have signal handlers
        // installed, `init` ignores them unless forced by the kernel.
        // TODO(https://fxbug.dev/573154732): Also ignore other unhandled
        // default-fatal / default-stop signals sent to `init` once unit tests
        // that run as PID 1 install handlers or use child tasks.
        signal_info.signal.is_unblockable() && self.is_init() && !signal_info.force
    }

    /// Causes the thread group to exit.
    ///
    /// Transitions the thread group to exiting, marks and interrupts sibling tasks to initiate
    /// teardown, and delivers any `PTRACE_EVENT_EXIT` stop for `current_task`.
    ///
    /// If this is being called from a task that is part of the current thread group, the caller
    /// should pass `current_task` (or call [`CurrentTask::kill_thread_group()`]).
    pub fn kill(&self, exit_status: ExitStatus, mut current_task: Option<&mut CurrentTask>) {
        let state = self.write();
        if !state.is_running() {
            if let Some(current_task) = current_task {
                current_task.write().set_exit_status_if_not_already(exit_status);
            }
            return;
        }

        // Mark and wake all sibling tasks while holding the thread-group write lock so no
        // sibling thread can execute in userspace once the thread group transitions to `Exiting`.
        // Keep `current_task` unexited (`!TaskFlags::EXITED`) until after `ptrace_event` so it can
        // block in `PTRACE_EVENT_EXIT`.
        let current_tid = current_task.as_ref().map(|task| task.tid.clone());
        state.terminate_tasks(exit_status.clone(), current_tid.as_ref());

        // From <https://man7.org/linux/man-pages/man2/ptrace.2.html>:
        //
        //   PTRACE_O_TRACEEXIT (since Linux 2.5.60)
        //     The tracee is stopped early during process exit, when registers
        //     are still available, allowing the tracer to see where the exit
        //     occurred, whereas the normal exit notification is done after the
        //     process is finished exiting. Even though context is available,
        //     the tracer cannot prevent the exit from happening at this point.
        //
        // Because the thread group is already `Exiting` and all sibling threads
        // have been marked `TaskFlags::EXITED` and interrupted, `process_mrelease(2)`
        // can safely release memory while `current_task` is stopped in
        // `PTRACE_EVENT_EXIT`.
        if let Some(ref mut current_task) = current_task {
            current_task
                .ptrace()
                .on_event(PtraceOptions::TRACEEXIT, exit_status.signal_info_status() as u64);
            let mut task_state = current_task.write();
            task_state.set_exit_status(exit_status);
            task_state.force_wake_for_exit();
        }
    }

    pub fn add(&self, task: Arc<Task>) -> Result<(), Errno> {
        let mut state = self.write();
        if !state.is_running() {
            if state.tasks_count() == 0 {
                log_warn!(
                    "Task {} with leader {} not running while adding its first task, \
                not sending creation notification",
                    task.tid,
                    self.leader
                );
            }
            return error!(EINVAL);
        }
        state.tasks.insert(task.persistent_info.clone());

        Ok(())
    }

    /// Remove the task from the children of this ThreadGroup.
    ///
    /// It is important that the task is taken as an `Arc`. It ensures the tasks of the
    /// ThreadGroup are always valid as they are still valid when removed.
    pub fn remove(&self, mut pids: PidTableGuard<'_>, task: &Arc<Task>) {
        task.set_ptrace_zombie(&mut pids);

        let mut state = self.write();

        if !state.tasks.remove(&task.persistent_info) {
            // The task has never been added. The only expected case is that this thread group
            // is not running.
            debug_assert!(!state.is_running());
            pids.remove_task(&task.tid);
            return;
        }

        if state.tasks.is_empty() {
            let pgid = self.pgid();
            let sid = Arc::clone(&state.session);
            pids.detach_pgid(&pgid, self);
            pids.detach_sid(&sid, self);

            let exit_status = if let ThreadGroupRunState::Exiting(exit_status) = &state.run_state {
                exit_status.clone()
            } else {
                let exit_status = task.exit_status().unwrap_or_else(|| {
                    log_error!("Exiting without an exit code.");
                    ExitStatus::Exit(u8::MAX)
                });
                state.set_exiting(exit_status.clone());
                exit_status
            };

            // Detach from any ptraced zombie tasks.
            let zombie_notifications = state.zombie_ptracees.detach_all(&mut pids);

            // Replace PID table entry with a zombie before clearing `task.tid` so that
            // lock-free RCU readers calling `PidEntry::get_process_task()` never observe
            // an empty `ThreadGroup` before `ProcessEntry::Zombie` is published.
            let zombie = ZombieProcess::new(
                task.clone(),
                state.as_ref(),
                exit_status,
                state.exit_signal.clone(),
            );
            pids.kill_process(&self.leader, Arc::downgrade(task));
            pids.remove_task(&task.tid);

            let disassociated_controlling_terminal =
                if state.session == self.leader { state.controlling_terminal.take() } else { None };

            // I have no idea if dropping the lock here is correct, and I don't want to think about
            // it. If problems do turn up with another thread observing an intermediate state of
            // this exit operation, the solution is to unify locks. It should be sensible and
            // possible for there to be a single lock that protects all (or nearly all) of the
            // data accessed by both exit and wait. In gvisor and linux this is the lock on the
            // equivalent of the PidTable. This is made more difficult by rust locks being
            // containers that only lock the data they contain, but see
            // https://docs.google.com/document/d/1YHrhBqNhU1WcrsYgGAu3JwwlVmFXPlwWHTJLAbwRebY/edit
            // for an idea.
            std::mem::drop(state);

            // Disassociate the controlling terminal when the session leader exits.
            // This cannot be called while holding the ThreadGroup state lock.
            if let Some(controlling_terminal) = disassociated_controlling_terminal {
                controlling_terminal.terminal.disassociate_controlling_session_if(
                    Some(&sid),
                    Some(controlling_terminal.side),
                );
            }

            for notification in zombie_notifications {
                notification.deliver(&mut pids);
            }

            // Remove the process from the cgroup2 pid table after TG lock is dropped.
            // This function will hold the CgroupState lock which should be before the TG lock. See
            // more in lock_cgroup2_pid_table comments.
            self.kernel.cgroups.lock_cgroup2_pid_table().remove_process(&self.leader);

            self.detach_ptracees(&mut pids);

            // We will need the immediate parent and the reaper. Once we have them, we can make
            // sure to take the locks in the right order: parent before child.
            let parent = self.read().parent.clone();
            let reaper = self.find_reaper();

            // From <https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html>:
            //
            //   The parent-death signal is sent upon subsequent termination of the
            //   parent thread and also upon termination of each subreaper process
            //   (see PR_SET_CHILD_SUBREAPER(2const)) to which the caller is
            //   subsequently reparented.  If the parent thread and all ancestor
            //   subreapers have already terminated by the time of the
            //   PR_SET_PDEATHSIG operation, then no parent-death signal is sent to
            //   the caller.
            //
            //   The parent-death signal is process-directed (see signal(7)) and, if
            //   the child installs a handler using the sigaction(2) SA_SIGINFO flag,
            //   the si_pid field of the siginfo_t argument of the handler contains
            //   the PID of the terminating parent process.
            let mut children = Vec::new();
            {
                // Reparent the children.
                if let Some(reaper) = reaper {
                    let reaper = reaper.upgrade();
                    {
                        let mut reaper_state = reaper.write();
                        // Locking the reaper (an ancestor) before `self` and its children follows
                        // strictly top-down traversal in the process tree, avoiding cycles.
                        let _token = allow_subclass();
                        let mut state = self.write();
                        for child_pid in std::mem::take(&mut state.children) {
                            if let Ok(child) = child_pid.get_thread_group() {
                                let signal = {
                                    // Locking the reaper (an ancestor) before `self` and its
                                    // children follows strictly top-down traversal in the process
                                    // tree, avoiding cycles.
                                    let _token = allow_subclass();
                                    let mut child_state = child.write();

                                    let signal = child_state.parent_death_signal_for_exiting_task(
                                        task,
                                        state.is_child_subreaper,
                                    );
                                    child_state.exit_signal = Some(SIGCHLD);
                                    child_state.parent = Some(ThreadGroupParent::from_reaper(
                                        Arc::downgrade(&reaper),
                                    ));
                                    signal
                                };
                                reaper_state.children.insert(child_pid);
                                children.push((child, signal));
                            }
                        }
                        reaper_state.zombie_children.append(&mut state.zombie_children);
                    }
                    ZombiePtracees::reparent(self, &reaper);

                    for (child, _) in &children {
                        child.kill_orphaned_pgrp(&pids, Some(self), None);
                    }
                } else {
                    // Without a reaper, drop the zombies directly.
                    let mut state = self.write();
                    for child_pid in std::mem::take(&mut state.children) {
                        if let Ok(child) = child_pid.get_thread_group() {
                            let signal = {
                                // Locking `self` (the parent) before its children follows strictly
                                // top-down traversal in the process tree, avoiding cycles.
                                let _token = allow_subclass();
                                let mut child_state = child.write();
                                let signal = child_state.parent_death_signal_for_exiting_task(
                                    task,
                                    state.is_child_subreaper,
                                );
                                child_state.parent = None;
                                signal
                            };
                            children.push((child, signal));
                        }
                    }
                    for zombie in state.zombie_children.drain(..) {
                        zombie.release(&mut pids);
                    }
                }
            }

            self.send_parent_death_signals(
                task,
                children.into_iter().filter_map(|(child, signal)| Some((child, signal?))),
            );

            // Clear the `parent` reference now that children have been re-`parent`ed.
            self.write().parent = None;

            #[cfg(any(test, debug_assertions))]
            {
                let state = self.read();
                assert!(state.zombie_children.is_empty());
                assert!(state.zombie_ptracees.is_empty());
            }

            if let Some(ref parent) = parent {
                let parent = parent.upgrade();

                let tracer_tg = task
                    .read()
                    .ptrace
                    .as_ref()
                    .and_then(|ptrace| ptrace.core_state.thread_group.upgrade());

                let maybe_zombie = match tracer_tg {
                    Some(tracer_tg) => {
                        tracer_tg.maybe_notify_tracer(task, &mut pids, &parent, zombie)
                    }
                    None => Some(zombie),
                };

                if let Some(zombie) = maybe_zombie {
                    parent.do_zombie_notifications(zombie, &mut pids);
                }
            } else {
                zombie.release(&mut pids);
            }

            // TODO: Set the error_code on the Zircon process object. Currently missing a way
            // to do this in Zircon. Might be easier in the new execution model.

            // Once the last zircon thread stops, the zircon process will also stop executing.

            let parent_tg = parent.as_ref().map(|p| p.upgrade());
            self.kill_orphaned_pgrp(&pids, parent_tg.as_deref(), Some(self));

            self.write().set_exited();
        } else {
            pids.remove_task(&task.tid);
            // From <https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html>:
            //
            //   The "parent" in this case is considered to be the thread that created
            //   this process.  In other words, the signal will be sent when that thread
            //   terminates (via, for example, pthread_exit(3)), rather than after all of
            //   the threads in the parent process terminate.
            let mut children_to_signal = Vec::new();
            for child in state.children() {
                // This allow_subclass is safe because we lock `self` (the parent) before
                // locking its children.
                let _token = allow_subclass();
                let child_state = child.read();
                if let Some(signal) = child_state.parent_death_signal_for_exiting_task(
                    task, /* parent_is_subreaper = */ false,
                ) {
                    children_to_signal.push((child.clone(), signal));
                }
            }
            std::mem::drop(state);
            self.send_parent_death_signals(task, children_to_signal);
        }
    }

    fn send_parent_death_signals(
        &self,
        task: &Arc<Task>,
        children_to_signal: impl IntoIterator<Item = (Arc<ThreadGroup>, Signal)>,
    ) {
        let uid = task.real_creds().uid;
        for (child, signal) in children_to_signal {
            // `PR_SET_PDEATHSIG(2const)` requires `si_pid` in `siginfo_t` to contain the PID
            // of the terminating parent process. In `sigaction(2)`, `si_pid` and `si_uid`
            // reside in the `_kill` union variant (`SignalDetail::Kill`), which is decoded
            // when `si_code` is `SI_USER` (0), matching the `si_code` observed on Linux.
            child.write().send_signal(SignalInfo::with_detail(
                signal,
                SI_USER as i32,
                SignalDetail::Kill { pid: self.leader.clone(), uid },
            ));
        }
    }

    /// Detach from any ptraced tasks, killing the ones that set `PTRACE_O_EXITKILL`.
    fn detach_ptracees(&self, pids: &mut PidTableGuard<'_>) {
        let tracee_tids = self.ptracees.lock().iter().map(|info| info.tid.clone()).collect_vec();
        for tracee_tid in tracee_tids {
            let Ok(tracee) = tracee_tid.get_task() else {
                continue;
            };

            let mut should_send_sigkill = false;
            if let Some(ptrace) = &tracee.read().ptrace {
                should_send_sigkill = ptrace.has_option(PtraceOptions::EXITKILL);
            }
            if should_send_sigkill {
                send_standard_signal(tracee.as_ref(), SignalInfo::kernel(SIGKILL));
            }

            let _ = ptrace_detach(
                pids,
                PtraceTracer::Exiting(self),
                tracee.as_ref(),
                &UserAddress::NULL,
            );
        }
    }

    pub fn do_zombie_notifications(
        &self,
        zombie: OwnedRef<ZombieProcess>,
        pids: &mut PidTableGuard<'_>,
    ) {
        let mut state = self.write();

        state.children.remove(&zombie.task.pid);
        state.deferred_zombie_ptracers.retain(|dzp| dzp.tracee_pid != zombie.task.pid);

        let exit_signal = zombie.exit_signal;
        let mut signal_info = zombie.to_wait_result().as_signal_info();

        // From https://man7.org/linux/man-pages/man2/sigaction.2.html
        //
        // > SA_NOCLDWAIT (since Linux 2.6)
        // >
        // >     If signum is SIGCHLD, do not transform children into
        // >     zombies when they terminate.  See also waitpid(2).  This
        // >     flag is meaningful only when establishing a handler for
        // >     SIGCHLD, or when setting that signal's disposition to
        // >     SIG_DFL.
        let should_make_zombie = if exit_signal == Some(SIGCHLD) {
            let action = self.signal_actions.get(SIGCHLD);
            action.sa_handler != SIG_IGN && (action.sa_flags & SA_NOCLDWAIT as u64) == 0
        } else {
            true
        };
        if should_make_zombie {
            state.zombie_children.push(zombie);
        } else {
            state.reap_zombie(zombie, pids);
        }

        state.lifecycle_waiters.notify_value(ThreadGroupLifecycleWaitValue::ChildStatus);

        // Send signals
        if let Some(exit_signal) = exit_signal {
            signal_info.signal = exit_signal;
            state.send_signal(signal_info);
        }
    }

    /// Notifies the tracer if appropriate.  Returns Some(zombie) if caller
    /// needs to notify the parent, None otherwise.  The caller should probably
    /// invoke parent.do_zombie_notifications(zombie) on the result.
    fn maybe_notify_tracer(
        &self,
        tracee: &Task,
        pids: &mut PidTableGuard<'_>,
        parent: &ThreadGroup,
        zombie: OwnedRef<ZombieProcess>,
    ) -> Option<OwnedRef<ZombieProcess>> {
        let mut state = self.write();
        if state.zombie_ptracees.has_tracee(&tracee.tid) {
            if self == parent {
                // The tracer is the parent and has not consumed the
                // notification.  Detach to clean up ptrace state, then
                // notify the parent by discarding the notification
                // generated by the detach and returning Some(zombie).
                if let Some(zombie_notification) = state.zombie_ptracees.detach(pids, &tracee.tid) {
                    zombie_notification.discard(pids);
                }
                return Some(zombie);
            } else {
                // The tracer is not the parent and the tracer has not consumed
                // the notification.
                if state.is_exited() {
                    // The tracer exited concurrently. Notify the parent.
                    return Some(zombie);
                }

                // THREAD SAFETY: Release the tracer state lock before acquiring the parent state
                // lock to respect parent => child lock ordering.
                drop(state);
                {
                    // Tell the parent to expect a notification later.
                    let mut parent_state = parent.write();
                    parent_state
                        .deferred_zombie_ptracers
                        .push(DeferredZombiePTracer::new(self, tracee));
                    parent_state.children.remove(&tracee.pid);
                }

                // Tell the tracer that there is a notification pending.
                // THREAD SAFETY: Checking for concurrent exit with is_exited(), releasing the
                // tracer state lock, then reacquiring the lock introduces a TOCTOU race. This
                // hazard is safe because exit synchronizes on the PidTable lock, which is held
                // continuously.
                let mut state = self.write();
                state.zombie_ptracees.set_parent_of(&tracee.tid, Some(zombie), parent);
                tracee.write().notify_ptracers();
                return None;
            }
        } else if self == parent {
            // The tracer is the parent and has already consumed the parent
            // notification.  No further action required.
            state.children.remove(&tracee.pid);
            zombie.release(pids);
            return None;
        }
        // The tracer is not the parent and has already consumed the parent
        // notification.  Notify the parent.
        Some(zombie)
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

    pub fn setsid(&self) -> Result<(), Errno> {
        let mut pids = self.kernel.pids.lock();
        let pid = self.leader.clone();

        // Calling thread group must not already be a process group or session leader.
        let scope = RcuReadScope::new();
        if self.leader.is_process_group(&scope) || self.leader.is_session(&scope) {
            return error!(EPERM);
        }

        let self_arc = self.to_owned();
        let (old_process_group, check_old_pgrp, child_pgrps) = {
            let mut state = self.write();
            if !state.is_running() {
                return error!(ESRCH);
            }
            let old_process_group = self.pgid();
            if state.session == self.leader || old_process_group == self.leader {
                return error!(EPERM);
            }
            self.process_group.update(pid.clone(), &pids, &state);
            let old_session = std::mem::replace(&mut state.session, pid.clone());
            state.controlling_terminal = None;

            pids.detach_pgid(&old_process_group, self);
            pids.detach_sid(&old_session, self);
            pids.attach_pgid(&pid, &self_arc);
            pids.attach_sid(&pid, &self_arc);

            let parent_tg = state.parent.as_ref().map(|p| p.upgrade());
            let mut child_pgrps = HashSet::new();
            for child in state.children() {
                let _token = allow_subclass();
                let child_state = child.read();
                let child_pgid = child.pgid();
                if child_state.session == old_session && child_pgid != old_process_group {
                    child_pgrps.insert(child_pgid);
                }
            }
            std::mem::drop(state);

            let check_old_pgrp = parent_tg.is_some_and(|parent_tg| {
                let parent_state = parent_tg.read();
                parent_state.session == old_session && parent_tg.pgid() != old_process_group
            });

            (old_process_group, check_old_pgrp, child_pgrps)
        };
        std::mem::drop(scope);

        // Evaluate the vacated process group and any child process groups in the old session
        // that may have lost their job-controlling parent.
        if check_old_pgrp {
            Self::check_orphaned_pgrp(&pids, &old_process_group, None);
        }
        for child_pgrp in child_pgrps {
            Self::check_orphaned_pgrp(&pids, &child_pgrp, None);
        }

        Ok(())
    }

    /// Returns the thread group's process group ID, which is the PID of the process group's leader.
    pub fn pgid(&self) -> Pid {
        self.process_group.get()
    }

    pub fn setpgid(
        &self,
        current_task: &CurrentTask,
        target: &Task,
        pgid: &Pid,
    ) -> Result<(), Errno> {
        let mut pids = self.kernel.pids.lock();

        let (old_process_group, check_old_pgrp, check_new_pgrp) = {
            let current_session = Arc::clone(&self.read().session);

            // The target process must be either the current process or a child of the current process.
            let target_thread_group = target.thread_group().write();
            if !target_thread_group.is_running() {
                return error!(ESRCH);
            }
            let parent_tg = target_thread_group.parent.as_ref().map(|p| p.upgrade());
            let is_target_current_process_child =
                parent_tg.as_ref().is_some_and(|tg| tg.leader == self.leader);
            if target_thread_group.base.leader != self.leader && !is_target_current_process_child {
                return error!(ESRCH);
            }

            // If the target process is a child of the current task, it must not have executed an exec function.
            if is_target_current_process_child && target_thread_group.did_exec {
                return error!(EACCES);
            }

            // The target process must not be a session leader and must be in the same session as the current process.
            if target_thread_group.base.leader == target_thread_group.session
                || current_session != target_thread_group.session
            {
                return error!(EPERM);
            }

            let old_process_group = target.thread_group().pgid();
            if *pgid == old_process_group {
                return Ok(());
            }

            // If joining an existing process group, it must be non-empty and belong to the same session.
            if *pgid != target_thread_group.base.leader {
                let scope = RcuReadScope::new();
                if !pgid.is_process_group_in_session(&target_thread_group.session, &scope) {
                    return error!(EPERM);
                }
            }
            security::check_setpgid_access(current_task, target)?;

            target.thread_group().process_group.update(pgid.clone(), &pids, &target_thread_group);
            pids.detach_pgid(&old_process_group, target.thread_group());
            pids.attach_pgid(pgid, target.thread_group());

            let check_new_pgrp = target_thread_group.children().any(|child| child.pgid() == *pgid);
            std::mem::drop(target_thread_group);

            let check_old_pgrp = parent_tg.is_some_and(|parent_tg| {
                let parent_state = parent_tg.read();
                parent_state.session == current_session && parent_tg.pgid() != old_process_group
            });

            (old_process_group, check_old_pgrp, check_new_pgrp)
        };

        // Evaluate the vacated and destination process groups for orphanhood if the move
        // removed an external job-controlling parent link.
        if check_old_pgrp {
            Self::check_orphaned_pgrp(&pids, &old_process_group, None);
        }
        if check_new_pgrp {
            Self::check_orphaned_pgrp(&pids, pgid, None);
        }

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

    /// Returns the controlling terminal of this thread group, if any.
    pub fn controlling_terminal(&self) -> Option<ControllingTerminal> {
        self.read().controlling_terminal.clone()
    }

    /// Ensures |session| is the controlling session inside of |controlling_session|.
    fn check_controlling_session(
        session: &Pid,
        controlling_session: &Option<ControllingSession>,
    ) -> Result<(), Errno> {
        if controlling_session.as_ref().is_some_and(|cs| Arc::ptr_eq(session, &cs.session)) {
            Ok(())
        } else {
            error!(ENOTTY)
        }
    }

    pub fn get_foreground_process_group(&self, terminal: &Terminal) -> Result<pid_t, Errno> {
        let state = self.read();
        let terminal_state = terminal.read();

        // "When fd does not refer to the controlling terminal of the calling
        // process, -1 is returned" - tcgetpgrp(3)
        Self::check_controlling_session(&state.session, &terminal_state.controlling_session)?;
        if !state
            .controlling_terminal
            .as_ref()
            .is_some_and(|ct| std::ptr::eq(terminal, Arc::as_ptr(&ct.terminal)))
        {
            return error!(ENOTTY);
        }
        let controlling_session =
            terminal_state.controlling_session.as_ref().ok_or_else(|| errno!(ENOTTY))?;
        Ok(controlling_session.foreground_pgid())
    }

    pub fn set_foreground_process_group(
        &self,
        current_task: &CurrentTask,
        terminal: &Terminal,
        pgid: &Pid,
    ) -> Result<(), Errno> {
        let process_group;
        let send_ttou;
        {
            // Keep locks to ensure atomicity.
            let state = self.read();
            process_group = self.pgid();
            let mut terminal_state = terminal.write();
            Self::check_controlling_session(&state.session, &terminal_state.controlling_session)?;
            if !state
                .controlling_terminal
                .as_ref()
                .is_some_and(|ct| std::ptr::eq(terminal, Arc::as_ptr(&ct.terminal)))
            {
                return error!(ENOTTY);
            }

            // Verify session membership lock-free under RcuReadScope.
            {
                let scope = RcuReadScope::new();
                if !pgid.is_process_group_in_session(&state.session, &scope) {
                    return error!(EPERM);
                }
            }

            let controlling_session =
                terminal_state.controlling_session.as_mut().ok_or_else(|| errno!(ENOTTY))?;

            // If the calling process is a member of a background group and not ignoring SIGTTOU, a
            // SIGTTOU signal is sent to all members of this background process group.
            send_ttou = process_group.id != controlling_session.foreground_pgid()
                && !current_task.read().signal_mask().has_signal(SIGTTOU)
                && self.signal_actions.get(SIGTTOU).sa_handler != SIG_IGN;

            if !send_ttou {
                controlling_session.set_foreground_process_group(pgid.clone());
            }
        }

        // Locks must not be held when sending signals.
        if send_ttou {
            process_group.send_signals_to_pgid(&[SIGTTOU]);
            return error!(EINTR);
        }

        Ok(())
    }

    pub fn set_controlling_terminal(
        &self,
        current_task: &CurrentTask,
        terminal: &Terminal,
        side: TerminalSide,
        steal: bool,
        is_readable: bool,
    ) -> Result<(), Errno> {
        let steal_from_session;
        let new_controlling_terminal = ControllingTerminal::new(terminal, side);
        {
            // Keep locks to ensure atomicity.
            let mut state = self.write();
            let mut terminal_state = terminal.write();

            // "The calling process must be a session leader and not have a
            // controlling terminal already." - tty_ioctl(4)
            if state.session != self.leader {
                return error!(EPERM);
            }

            if let Some(ref current_ct) = state.controlling_terminal {
                if current_ct.matches(terminal, side) {
                    return Ok(());
                } else {
                    return error!(EPERM);
                }
            }

            let session_pid = state.session.clone();
            let other_session_pid = terminal_state.controlling_session().cloned();
            let mut has_admin_capability_determined = false;
            let mut stolen_session = None;

            if let Some(other_sid) = other_session_pid {
                if other_sid != session_pid {
                    // "If this terminal is already the controlling terminal of a different
                    // session group, then the ioctl fails with EPERM, unless the caller
                    // has the CAP_SYS_ADMIN capability and arg equals 1, in which case the
                    // terminal is stolen, and all processes that had it as controlling
                    // terminal lose it." - tty_ioctl(4)
                    if !steal {
                        return error!(EPERM);
                    }
                    security::check_task_capable(current_task, CAP_SYS_ADMIN)?;
                    has_admin_capability_determined = true;
                    stolen_session = Some(other_sid);
                }
            }

            if !is_readable && !has_admin_capability_determined {
                security::check_task_capable(current_task, CAP_SYS_ADMIN)?;
            }

            state.controlling_terminal = Some(new_controlling_terminal);
            terminal_state.controlling_session =
                Some(ControllingSession::new(&session_pid, &self.pgid()));
            steal_from_session = stolen_session;
        }

        // Steal the TTY away from the previous session's processes. Unlike TIOCNOTTY, don't send signals.
        if let Some(other_sid) = steal_from_session {
            other_sid.clear_controlling_terminal(Some((terminal, None)));
        }

        Ok(())
    }

    pub fn release_controlling_terminal(
        &self,
        _current_task: &CurrentTask,
        terminal: &Terminal,
        side: TerminalSide,
    ) -> Result<(), Errno> {
        let controlling_session = {
            // Keep locks to ensure atomicity.
            let mut state = self.write();
            let is_session_leader = state.session == self.leader;
            let mut terminal_state = terminal.write();

            // tty must be the controlling terminal.
            Self::check_controlling_session(&state.session, &terminal_state.controlling_session)?;
            if !state.controlling_terminal.as_ref().is_some_and(|ct| ct.matches(terminal, side)) {
                return error!(ENOTTY);
            }

            state.controlling_terminal = None;
            is_session_leader.then(|| terminal_state.controlling_session.take()).flatten()
        };

        if let Some(controlling_session) = controlling_session {
            controlling_session.disassociate(terminal, Some(side));
        }

        Ok(())
    }

    /// Checks whether `pgrp` is orphaned and has stopped jobs, sending `SIGHUP` and `SIGCONT` if
    /// so.
    fn check_orphaned_pgrp(
        _pids: &PidTableGuard<'_>,
        pgrp: &Pid,
        ignored_tg: Option<&ThreadGroup>,
    ) {
        let scope = RcuReadScope::new();
        if pgrp.will_become_orphaned_pgrp(&scope, ignored_tg) && pgrp.has_stopped_jobs(&scope) {
            pgrp.send_signals_to_pgid(&[SIGHUP, SIGCONT]);
        }
    }

    /// Evaluates whether this thread group's process group became newly orphaned
    /// due to process termination or reparenting, and sends SIGHUP + SIGCONT if stopped jobs exist.
    ///
    /// - `parent`: The parent thread group that was outside this group.
    /// - `ignored_tg`: An optional thread group to ignore during orphan checks.
    pub fn kill_orphaned_pgrp(
        &self,
        pids: &PidTableGuard<'_>,
        parent: Option<&ThreadGroup>,
        ignored_tg: Option<&ThreadGroup>,
    ) {
        let Some(parent_tg) = parent else {
            return;
        };

        let (pgrp, my_sid) = {
            let state = self.read();
            (self.pgid(), state.session.id)
        };

        let (parent_pgid, parent_sid) = {
            let parent_state = parent_tg.read();
            (parent_tg.pgid().id, parent_state.session.id)
        };

        // Filter: Did the exiting parent provide an external link in the same session?
        if parent_pgid == pgrp.id || parent_sid != my_sid {
            return;
        }

        Self::check_orphaned_pgrp(pids, &pgrp, ignored_tg);
    }

    pub fn get_rlimit(&self, resource: Resource) -> u64 {
        self.limits.lock().get(resource).rlim_cur
    }

    /// Adjusts the rlimits of the ThreadGroup to which `target_task` belongs to.
    pub fn adjust_rlimits(
        current_task: &CurrentTask,
        target_task: &Task,
        resource: Resource,
        maybe_new_limit: Option<rlimit>,
    ) -> Result<rlimit, Errno> {
        let thread_group = target_task.thread_group();
        let mut limit_state = thread_group.limits.lock();
        let old_limit = limit_state.get(resource);
        if let Some(new_limit) = maybe_new_limit {
            if new_limit.rlim_max > old_limit.rlim_max
                && !security::is_task_capable_noaudit(current_task, CAP_SYS_RESOURCE)
            {
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
        f: &mut dyn FnMut(&Task, &TaskMutableState),
    ) {
        for task_ref in self
            .ptracees
            .lock()
            .iter()
            .filter(|info| selector.match_tid(&info.tid))
            .filter_map(|info| info.tid.get_task().ok())
        {
            let task_state = task_ref.write();
            if task_state.ptrace.is_some() {
                f(&task_ref, &task_state);
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
        pids: &mut PidTableGuard<'_>,
    ) -> Option<WaitResult> {
        // This checks to see if the target is a zombie ptracee.
        let waitable_entry = self.write().zombie_ptracees.get_waitable_entry(selector, options);
        match waitable_entry {
            None => (),
            Some((zombie, None)) => return Some(zombie.to_wait_result()),
            Some((zombie, Some((tg, z)))) => {
                if let Some(tg) = tg.upgrade() {
                    if Arc::as_ptr(&tg) != self as *const Self {
                        tg.do_zombie_notifications(z, pids);
                    } else {
                        {
                            let mut state = tg.write();
                            state.children.remove(&z.task.pid);
                            state
                                .deferred_zombie_ptracers
                                .retain(|dzp| dzp.tracee_pid != z.task.pid);
                        }

                        z.release(pids);
                    };
                }
                return Some(zombie.to_wait_result());
            }
        }

        let mut tasks = vec![];

        // This checks to see if the target is a running ptracee.
        self.get_ptracees_and(selector, &mut |task: &Task, _| {
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
                let info = process_state.tasks.iter().next().unwrap().clone();
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
                                ExitType::Stop => Some((
                                    ExitStatus::Stop(siginfo, event),
                                    process_state.base.leader.clone(),
                                )),
                                ExitType::Cont => Some((
                                    ExitStatus::Continue(siginfo, event),
                                    process_state.base.leader.clone(),
                                )),
                                ExitType::Kill => Some((
                                    ExitStatus::Kill(siginfo),
                                    process_state.base.leader.clone(),
                                )),
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
                if exit_status.is_none() {
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
                                    ExitType::Stop => Some((
                                        ExitStatus::Stop(siginfo, event),
                                        task_ref.tid.clone(),
                                    )),
                                    ExitType::Cont => Some((
                                        ExitStatus::Continue(siginfo, event),
                                        task_ref.tid.clone(),
                                    )),
                                    ExitType::Kill => {
                                        Some((ExitStatus::Kill(siginfo), task_ref.tid.clone()))
                                    }
                                    _ => None,
                                };
                            }
                        }
                    }
                }
                if let Some((exit_status, pid)) = exit_status {
                    return Some(WaitResult {
                        pid,
                        uid,
                        zombie_state: ZombieState { exit_status, time_stats },
                        exit_signal,
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
                    pid: current_task.pid.clone(),
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
                pid: current_task.pid.clone(),
                uid: current_task.current_creds().uid,
            },
        );

        self.write().send_signal(signal_info);
        Ok(())
    }

    /// Attempts to send an unchecked signal to this thread group, with the given `siginfo`.
    ///
    /// - `current_task`: The task that is sending the signal.
    /// - `unchecked_signal`: The signal that is to be sent. Unchecked, since `0` is a sentinel value
    /// where rights are to be checked but no signal is actually sent.
    /// - `siginfo`: The siginfo that will be enqueued. The caller is responsible for reading it
    ///   from userspace and for rejecting `si_code` values that userspace may not send to this
    ///   thread group.
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
        siginfo: UncheckedSignalInfo,
        options: IntoSignalInfoOptions,
    ) -> Result<(), Errno> {
        if matches!(options, IntoSignalInfoOptions::CheckSigno)
            && siginfo.signo() as u64 != unchecked_signal.raw()
        {
            return error!(EINVAL);
        }

        if let Some(signal) = self.check_signal_access(current_task, unchecked_signal)? {
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
        let Some(target_task) = self.read().get_signalable_task() else {
            // If there are no signalable tasks in the thread group, all tasks have exited and the
            // process is terminating or a zombie. A signal sent to a terminating or zombie process
            // is ignored, matching the behavior for Zombie processes in sys_kill.
            return Ok(None);
        };
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
    /// A deferred shutdown process (still handling a core-dump signal it got during shutdown, or
    /// created during shutdown) may run until `deferred_shutdown_deadline` before it is sent
    /// SIGKILL.
    ///
    /// Returns once `ThreadGroup::exit()` has completed.
    ///
    /// Must be called from the system task.
    pub async fn shut_down(this: Weak<Self>, deferred_shutdown_deadline: zx::MonotonicInstant) {
        const SHUTDOWN_SIGNAL_HANDLING_TIMEOUT: zx::MonotonicDuration =
            zx::MonotonicDuration::from_seconds(1);
        // How long after the deferred shutdown deadline a crashing process whose dumper was killed
        // may run to report its crash. Comes out of the margin left by `DEFERRED_SHUTDOWN_TIMEOUT`.
        const SHUTDOWN_CRASH_REPORT_TIMEOUT: zx::MonotonicDuration =
            zx::MonotonicDuration::from_millis(500);

        // Prepare for shutting down the thread group.
        let (tg_name, mut on_exited) = {
            // Nest this upgraded access so upgraded references aren't held across await-points.
            let Some(this) = this.upgrade() else {
                return;
            };

            let mut state = this.write();
            if state.is_exited() {
                // Do not set an exit notifier on an exited thread group. It will never be notified.
                return;
            }

            // Register a channel to be notified when exit() is complete.
            let (on_exited_send, on_exited) = futures::channel::oneshot::channel();
            state.exit_notifier = Some(on_exited_send);

            // We want to be able to log about this thread group without upgrading the `Weak`.
            let tg_name = format!("{this:?}");

            (tg_name, on_exited)
        };

        // Waits until the thread group exits or `deadline` passes. Returns whether it exited.
        async fn exited_by(
            on_exited: &mut futures::channel::oneshot::Receiver<()>,
            deadline: zx::MonotonicInstant,
        ) -> bool {
            let timeout = fuchsia_async::Timer::new(deadline);
            futures::pin_mut!(timeout);
            futures::select_biased! {
                _ = &mut *on_exited => true,
                _ = timeout => false,
            }
        }

        // A process created during shutdown (e.g. a crash dumper) is a deferred shutdown process, so
        // it isn't asked to exit before the deferred shutdown deadline.
        let created_during_shutdown =
            this.upgrade().is_some_and(|tg| tg.created_during_shutdown.load(Ordering::Relaxed));
        let mut exited = false;
        if !created_during_shutdown {
            log_debug!(tg:% = tg_name; "shutting down thread group, sending SIGTERM");
            this.upgrade().map(|tg| tg.write().send_signal(SignalInfo::kernel(SIGTERM)));

            // Give thread groups some time to handle SIGTERM, proceeding early if they exit.
            exited = exited_by(
                &mut on_exited,
                zx::MonotonicInstant::after(SHUTDOWN_SIGNAL_HANDLING_TIMEOUT),
            )
            .await;
        }

        // So is a running thread group that is still handling a core-dump signal it got during
        // shutdown: it is likely writing a crash dump (e.g. its handler forked a dumper).
        let deferred = created_during_shutdown
            || (!exited
                && this.upgrade().is_some_and(|tg| {
                    let state = tg.read();
                    state.is_running() && state.is_handling_core_dump_signal()
                }));
        if deferred {
            if zx::MonotonicInstant::get() < deferred_shutdown_deadline {
                log_info!(tg:% = tg_name; "deferred shutdown process, deferring SIGKILL");
                exited = exited_by(&mut on_exited, deferred_shutdown_deadline).await;
            }
            if !exited {
                this.upgrade().map(|tg| tg.kernel.kill_processes_created_during_shutdown());
                // Give a crashing process whose dumper was just killed a moment to re-raise the
                // core-dump signal, so that Starnix files a crash report for it. See
                // `Kernel::kill_processes_created_during_shutdown()`.
                exited = exited_by(
                    &mut on_exited,
                    zx::MonotonicInstant::after(SHUTDOWN_CRASH_REPORT_TIMEOUT),
                )
                .await;
            }
        }

        if !exited {
            log_debug!(tg:% = tg_name; "sending SIGKILL");
            this.upgrade().map(|tg| tg.write().send_signal(SignalInfo::forced(SIGKILL)));
        }

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
    ReadyNow(Box<WaitResult>),
    ShouldWait,
    NoneFound,
}

#[apply(state_implementation!)]
impl ThreadGroupMutableState<Base = ThreadGroup> {
    /// Returns the parent-death [`Signal`] configured for this process via `PR_SET_PDEATHSIG`.
    pub fn parent_death_signal(&self) -> Option<Signal> {
        self.parent_death_signal
    }

    /// Sets or clears the parent-death [`Signal`] for this process.
    pub fn set_parent_death_signal(&mut self, signal: Option<Signal>) {
        self.parent_death_signal = signal;
    }

    /// Returns the parent-death [`Signal`] to deliver to this child when `exiting_task` in its
    /// parent [`ThreadGroup`] terminates, or `None` if no signal should be delivered.
    ///
    /// Prior to reparenting (`creator_task` is `Some`), the signal is delivered only when the
    /// specific creating [`Task`] exits. Once reparented to an ancestor reaper (`creator_task` is
    /// `None`), the signal is delivered only if the terminating parent is a subreaper
    /// (`parent_is_subreaper` is `true` on final process exit).
    fn parent_death_signal_for_exiting_task(
        &self,
        exiting_task: &Arc<Task>,
        parent_is_subreaper: bool,
    ) -> Option<Signal> {
        let should_signal =
            self.parent.as_ref().is_some_and(|parent| match parent.creator_task() {
                Some(creator_task) => Weak::ptr_eq(creator_task, &exiting_task.weak_self),
                None => parent_is_subreaper,
            });
        should_signal.then_some(self.parent_death_signal).flatten()
    }

    pub fn leader(&self) -> pid_t {
        self.base.leader.id
    }

    pub fn leader_command(&self) -> TaskCommand {
        self.get_task(self.leader())
            .map(|l| l.command())
            .unwrap_or_else(|| TaskCommand::new(b"<leader exited>"))
    }

    pub fn is_running(&self) -> bool {
        matches!(self.run_state, ThreadGroupRunState::Running)
    }

    /// Whether a task in this thread group is running a handler for a core-dump signal it got
    /// during shutdown. See `TaskFlags::IN_CORE_DUMP_SIGNAL_HANDLER`.
    pub fn is_handling_core_dump_signal(&self) -> bool {
        self.tasks().iter().any(|t| t.flags().contains(TaskFlags::IN_CORE_DUMP_SIGNAL_HANDLER))
    }

    /// Whether this is a deferred shutdown process, which may create processes during shutdown
    /// until the deferred shutdown deadline: it is handling a core-dump signal it got during
    /// shutdown (e.g. forking a crash dumper), or a deferred shutdown process created it during
    /// shutdown (e.g. the dumper forking). See `ThreadGroup::shut_down()`.
    pub fn is_deferred_shutdown_process(&self) -> bool {
        self.base.kernel.is_shutting_down()
            && (self.base.created_during_shutdown.load(Ordering::Relaxed)
                || self.is_handling_core_dump_signal())
    }

    pub fn is_exited(&self) -> bool {
        matches!(self.run_state, ThreadGroupRunState::Exited(_))
    }

    /// Transitions this [`ThreadGroup`] to [`ThreadGroupRunState::Exiting`].
    fn set_exiting(&mut self, exit_status: ExitStatus) {
        self.run_state = ThreadGroupRunState::Exiting(exit_status);
    }

    fn set_exited(&mut self) {
        let ThreadGroupRunState::Exiting(exit_status) = std::mem::take(&mut self.run_state) else {
            panic!("Must transition from Exiting to Exited");
        };
        self.run_state = ThreadGroupRunState::Exited(exit_status);

        if let Some(notifier) = self.exit_notifier.take() {
            let _ = notifier.send(());
        }
    }

    /// Returns the running child thread groups.
    ///
    /// A `Pid` in `self.children` may not resolve to a `ThreadGroup` while a child is in the
    /// middle of task creation (before `PidTableGuard::add_task`) or exit (between
    /// `PidTableGuard::kill_process` and removing itself from `parent.children`).
    pub fn children(&self) -> impl Iterator<Item = Arc<ThreadGroup>> + '_ {
        self.children.iter().flat_map(|pid| pid.get_thread_group().ok())
    }

    pub fn tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.iter().flat_map(|info| info.tid.get_task().ok()).collect()
    }

    pub fn first_task(&self) -> Option<Arc<Task>> {
        self.tasks.iter().find_map(|info| info.tid.get_task().ok())
    }

    pub fn task_ids(&self) -> impl Iterator<Item = tid_t> + '_ {
        self.tasks.iter().map(|info| info.tid.id)
    }

    pub fn contains_task(&self, tid: tid_t) -> bool {
        self.tasks.iter().any(|info| info.tid.id == tid)
    }

    pub fn get_task(&self, tid: tid_t) -> Option<Arc<Task>> {
        self.tasks.iter().find(|info| info.tid.id == tid).and_then(|info| info.tid.get_task().ok())
    }

    pub fn tasks_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn get_ppid(&self) -> pid_t {
        match &self.parent {
            Some(parent) => parent.upgrade().leader.id,
            None => 0,
        }
    }

    /// Reaps the given zombie, making its PID available for reuse.
    fn reap_zombie(&mut self, zombie: OwnedRef<ZombieProcess>, pids: &mut PidTableGuard<'_>) {
        self.children_time_stats += zombie.state.time_stats;
        zombie.release(pids);
    }

    /// Indicates whether the thread group is waitable via waitid and waitpid for
    /// either WSTOPPED or WCONTINUED.
    pub fn is_waitable(&self) -> bool {
        return self.last_signal.is_some() && !self.base.load_stopped().is_in_progress();
    }

    pub fn get_waitable_zombie(
        &mut self,
        zombie_list: &dyn Fn(&mut ThreadGroupMutableState) -> &mut Vec<OwnedRef<ZombieProcess>>,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &mut PidTableGuard<'_>,
    ) -> Option<WaitResult> {
        // We look for the last zombie in the vector that matches pid selector and waiting options
        let selected_zombie_position = zombie_list(self)
            .iter()
            .rev()
            .position(|zombie| zombie.matches_selector_and_waiting_option(selector, options))
            .map(|position_starting_from_the_back| {
                zombie_list(self).len() - 1 - position_starting_from_the_back
            });

        selected_zombie_position.map(|position| {
            if options.keep_waitable_state {
                zombie_list(self)[position].to_wait_result()
            } else {
                let zombie = zombie_list(self).remove(position);
                let result = zombie.to_wait_result();
                self.reap_zombie(zombie, pids);
                result
            }
        })
    }

    pub fn is_correct_exit_signal(for_clone: bool, exit_code: Option<Signal>) -> bool {
        for_clone == (exit_code != Some(SIGCHLD))
    }

    fn get_waitable_running_children(
        &self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
    ) -> WaitableChildResult {
        // The children whose pid matches the pid selector queried.
        let filter_children_by_pid_selector = |child: &ThreadGroup| match selector {
            ProcessSelector::Any => true,
            ProcessSelector::Pid(pid) => &child.leader == pid,
            ProcessSelector::Pgid(pgid) => &child.pgid() == pgid,
        };

        // The children whose exit signal matches the waiting options queried.
        let filter_children_by_waiting_options = |child: &ThreadGroup| {
            if options.wait_for_all {
                return true;
            }
            // This allow_subclass is safe because the lock is being acquired
            // in a strictly top-down traversal of the ThreadGroup tree (from parent
            // to child), so no lock ordering cycles can be formed.
            let _token = allow_subclass();
            Self::is_correct_exit_signal(options.wait_for_clone, child.read().exit_signal)
        };

        // If wait_for_exited flag is disabled or no exited children were found we look for running
        // children.
        let mut selected_children = self
            .children()
            .filter(|tg| filter_children_by_pid_selector(&tg))
            .filter(|tg| filter_children_by_waiting_options(&tg))
            .peekable();
        if selected_children.peek().is_none() {
            // There still might be a process that ptrace hasn't looked at yet.
            if self.deferred_zombie_ptracers.iter().any(|dzp| match selector {
                ProcessSelector::Any => true,
                ProcessSelector::Pid(pid) => &dzp.tracee_pid == pid,
                ProcessSelector::Pgid(pgid) => dzp.tracee_pgid().as_ref() == Some(pgid),
            }) {
                return WaitableChildResult::ShouldWait;
            }

            return WaitableChildResult::NoneFound;
        }
        for child in selected_children {
            // This allow_subclass is safe because the lock is being acquired
            // in a strictly top-down traversal of the ThreadGroup tree (from parent
            // to child), so no lock ordering cycles can be formed.
            let _token = allow_subclass();
            let child = child.write();
            if child.last_signal.is_some() {
                let build_wait_result = |mut child: ThreadGroupWriteGuard<'_>,
                                         exit_status: &dyn Fn(SignalInfo) -> ExitStatus|
                 -> WaitResult {
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
                    let info = child.tasks.iter().next().unwrap();
                    let uid = info.real_creds().uid;
                    WaitResult {
                        pid: child.base.leader.clone(),
                        uid,
                        zombie_state: ZombieState {
                            exit_status,
                            time_stats: child.base.time_stats() + child.children_time_stats,
                        },
                        exit_signal: child.exit_signal,
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
        pids: &mut PidTableGuard<'_>,
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

        self.get_waitable_running_children(selector, options)
    }

    /// Returns a running task in the current thread group.
    pub fn get_running_task(&self) -> Result<Arc<Task>, Errno> {
        self.tasks
            .iter()
            .find_map(|info| info.tid.get_task().ok().filter(|task| task.is_running()))
            .ok_or_else(|| errno!(ESRCH))
    }

    /// Returns a running task with an intact file table in the current thread group, preferring
    /// the thread group leader.
    pub fn get_running_task_with_files(&self) -> Result<Arc<Task>, Errno> {
        if let Ok(task) = self.base.leader.get_task() {
            if task.is_running() && task.files().is_ok() {
                return Ok(task);
            }
        }
        self.tasks
            .iter()
            .find_map(|info| {
                info.tid.get_task().ok().filter(|task| task.is_running() && task.files().is_ok())
            })
            .ok_or_else(|| errno!(ESRCH))
    }

    /// Returns a task representative of the [`ThreadGroup`] for signal access checks.
    ///
    /// Prefers a running task, but falls back to the first available non-running task.
    /// Returns `None` if the task list is empty or no tasks can be upgraded.
    fn get_signalable_task(&self) -> Option<Arc<Task>> {
        let mut non_running = if let Ok(task) = self.base.leader.get_task() {
            if task.is_running() {
                return Some(task);
            }
            Some(task)
        } else {
            None
        };
        for container in &self.tasks {
            if let Ok(task) = container.tid.get_task() {
                if task.is_running() {
                    return Some(task);
                }
                if non_running.is_none() {
                    non_running = Some(task);
                }
            }
        }
        non_running
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

    /// Synchronously transitions this [`ThreadGroup`] to [`ThreadGroupRunState::Exiting`] and marks
    /// all running member [`Task`]s (except `excluded_tid`, if specified) as
    /// [`TaskFlags::EXITED`] before waking and interrupting them.
    ///
    /// All targeted member tasks must have [`TaskFlags::EXITED`] set while the
    /// [`ThreadGroupWriteGuard`] is still held, before [`Self::set_stopped`] notifies
    /// `lifecycle_waiters` and drops the lock. This guarantees that stopped sibling threads cannot
    /// wake and return to userspace, and that `process_mrelease(2)` cannot observe a non-running
    /// [`ThreadGroup`] before all member tasks are marked [`TaskFlags::EXITED`].
    #[allow(unused_mut, reason = "needed for some but not all macro outputs")]
    fn terminate_tasks(mut self, exit_status: ExitStatus, excluded_tid: Option<&Pid>) {
        if self.is_running() {
            self.set_exiting(exit_status.clone());
        }

        let tasks: Vec<Arc<Task>> = self
            .tasks()
            .into_iter()
            .filter(|task| task.is_running() && excluded_tid != Some(&task.tid))
            .collect();
        for task in &tasks {
            let mut task_state = task.write();
            task_state.set_exit_status_if_not_already(exit_status.clone());
            task_state.force_wake_for_exit();
        }

        self.set_stopped(
            StopState::ForceWaking,
            /* siginfo = */ None,
            /* finalize_only = */ false,
        );

        for task in tasks {
            task.interrupt();
        }
    }

    #[allow(unused_mut, reason = "needed for some but not all macro outputs")]
    pub(crate) fn force_exit_for_default_fatal(mut self, signal_info: SignalInfo) {
        let exit_status = ExitStatus::Kill(signal_info);
        self.terminate_tasks(exit_status, /* excluded_tid = */ None);
    }

    /// Sends the signal `signal_info` to this thread group.
    #[allow(unused_mut, reason = "needed for some but not all macro outputs")]
    pub fn send_signal(mut self, signal_info: SignalInfo) {
        let any_task_ptraced = self
            .tasks
            .iter()
            .filter_map(|info| info.tid.get_task().ok())
            .any(|task| task.is_running() && task.read().is_ptraced());
        let ignores_signal = self.base.ignores_signal(&signal_info);
        if ignores_signal && (signal_info.signal == SIGKILL || !any_task_ptraced) {
            return;
        }

        if signal_info.signal == SIGKILL {
            self.force_exit_for_default_fatal(signal_info);
            return;
        }

        let sigaction = self.base.signal_actions.get(signal_info.signal);
        let action = action_for_signal(&signal_info, sigaction);

        // Like `send_signal_prio` does for a single task, discard an ignored signal instead of
        // queueing it: no task would ever act on it, but it would still make interruptible syscalls
        // fail with EINTR. It must still be queued when a task blocks it with its current or its
        // saved mask, as it can be accepted later (see the `SigtimedwaitTest.IgnoredUnmaskedSignal`
        // gvisor test), or when a task is ptraced, for the signal-delivery-stop.
        let queue_on_group = action != DeliveryAction::Ignore
            || any_task_ptraced
            || self
                .tasks
                .iter()
                .filter_map(|info| info.tid.get_task().ok())
                .filter(|task| task.is_running())
                .any(|task| {
                    let task_state = task.read();
                    task_state.is_signal_masked(signal_info.signal)
                        || task_state.is_signal_masked_by_saved_mask(signal_info.signal)
                });

        if queue_on_group {
            let mut pending_signals = self.base.pending_signals.lock();
            pending_signals.enqueue(signal_info.clone());
            self.base.has_pending_signals.store(true, Ordering::Relaxed);
        }

        if !ignores_signal
            && action == DeliveryAction::Terminate
            && !self.base.load_stopped().is_stopping_or_stopped()
            && !self.base.leader.get_task().is_ok_and(|leader| leader.read().is_ptraced())
            && self
                .tasks
                .iter()
                .filter_map(|info| info.tid.get_task().ok())
                .any(|task| task.is_running() && task.read().can_accept_fatal_signal(&signal_info))
        {
            self.force_exit_for_default_fatal(signal_info);
            return;
        }

        let tasks: Vec<Pid> = self.tasks.iter().map(|info| info.tid.clone()).collect();

        // Set state to waking before interrupting any tasks.
        if signal_info.signal == SIGCONT {
            self.set_stopped(
                StopState::Waking,
                Some(signal_info.clone()),
                /* finalize_only = */ false,
            );
        }

        let mut has_interrupted_task = false;
        for task in tasks.iter().flat_map(|pid| pid.get_task().ok()) {
            if !task.is_running() {
                continue;
            }

            let mut task_state = task.write();

            if signal_info.signal == SIGCONT {
                task_state.set_stopped(
                    StopState::Waking,
                    /* siginfo = */ None,
                    /* current_task = */ None,
                    /* event = */ None,
                );
            }

            let is_masked = task_state.is_signal_masked(signal_info.signal);
            let was_masked = task_state.is_signal_masked_by_saved_mask(signal_info.signal);

            let is_queued = (!ignores_signal
                && (action != DeliveryAction::Ignore || is_masked || was_masked))
                || task_state.is_ptraced();

            if is_queued {
                task_state.notify_signal_waiters(&signal_info.signal);

                let is_fatal = action == DeliveryAction::Terminate && !task_state.is_ptraced();

                if !is_masked
                    && action.must_interrupt(Some(sigaction))
                    && (!has_interrupted_task || is_fatal)
                {
                    // Interrupt every task if the action is fatal, or only one task for catchable
                    // signals.
                    drop(task_state);
                    task.interrupt();
                    has_interrupted_task = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ptrace::{PtraceAttachType, ptrace_attach, ptrace_dispatch, ptrace_traceme};
    use crate::signals::{dequeue_signal, send_signal};
    use crate::testing::*;
    use starnix_syscalls::SUCCESS;
    use starnix_uapi::signals::SIGSTOP;
    use starnix_uapi::user_address::{UserAddress, UserRef};
    use starnix_uapi::{
        CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, PTRACE_CONT, PTRACE_KILL, PTRACE_O_EXITKILL,
        SI_TKILL,
    };

    #[::fuchsia::test]
    async fn test_setsid() {
        spawn_kernel_and_run(async |current_task| {
            fn get_process_group(task: &Task) -> Pid {
                task.thread_group().pgid()
            }
            assert_eq!(current_task.thread_group().setsid(), error!(EPERM));

            let child_task = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(get_process_group(&current_task), get_process_group(&child_task));

            let old_process_group = child_task.thread_group().pgid();
            assert_eq!(child_task.thread_group().setsid(), Ok(()));
            assert_eq!(child_task.thread_group().read().session, child_task.pid);
            let scope = RcuReadScope::new();
            assert!(
                !old_process_group
                    .pgid_thread_groups(&scope)
                    .any(|tg| Arc::ptr_eq(&tg, child_task.thread_group()))
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_exit_status() {
        spawn_kernel_and_run(async |current_task| {
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            child.thread_group().kill(ExitStatus::Exit(42), None);
            assert_eq!(child.exit_status(), Some(ExitStatus::Exit(42)));
            std::mem::drop(child);
            assert_eq!(
                current_task.thread_group().read().zombie_children[0].state.exit_status,
                ExitStatus::Exit(42)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_kill_thread_group_when_already_exiting() {
        spawn_kernel_and_run(async |current_task| {
            let clone_thread_flags = (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) as u64;
            let mut leader = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let mut sibling = leader.clone_task_for_test(clone_thread_flags, None);

            // Simulate the thread group having already transitioned to `Exiting(42)` while a
            // sibling thread concurrently calls `kill_thread_group` (e.g. `exit_group(99)`).
            leader.thread_group().write().set_exiting(ExitStatus::Exit(42));
            sibling.kill_thread_group(ExitStatus::Exit(99));
            assert!(sibling.is_exitted());

            leader.kill_thread_group(ExitStatus::Exit(0));
            assert!(leader.is_exitted());
            std::mem::drop(sibling);
            std::mem::drop(leader);
            assert_eq!(
                current_task.thread_group().read().zombie_children[0].state.exit_status,
                ExitStatus::Exit(42)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_unblockable_signals_to_init_ignored() {
        spawn_kernel_and_run(async |current_task| {
            assert!(current_task.thread_group().is_init());

            for signal in [SIGKILL, SIGSTOP] {
                // Process-directed SIGKILL and SIGSTOP from userspace to init (PID 1) are ignored.
                assert_eq!(
                    current_task.thread_group().send_signal_unchecked(current_task, signal.into()),
                    Ok(())
                );
                assert!(current_task.thread_group().read().is_running());
                assert!(!current_task.is_exitted());
                assert_eq!(current_task.read().queued_signal_count(signal), 0);

                // Task-directed SIGKILL and SIGSTOP from userspace to init (PID 1) are also
                // ignored.
                let user_signal_info = SignalInfo::with_sender(
                    signal,
                    SI_TKILL,
                    SignalDetail::Kill {
                        pid: current_task.pid.clone(),
                        uid: current_task.current_creds().uid,
                    },
                    Some(current_task.weak_self.clone()),
                );
                assert_eq!(send_signal(current_task, user_signal_info), Ok(()));
                assert!(current_task.thread_group().read().is_running());
                assert!(!current_task.is_exitted());
                assert_eq!(current_task.read().queued_signal_count(signal), 0);
            }

            // Ptrace operations (`PTRACE_KILL`, `PTRACE_CONT` with `SIGKILL` or `SIGSTOP`, and
            // `PTRACE_O_EXITKILL` on tracer exit) send unforced signals and must not terminate or
            // stop `init`.
            let mut tracer = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                ptrace_attach(
                    &mut tracer,
                    &current_task.tid,
                    PtraceAttachType::Seize,
                    UserAddress::from(PTRACE_O_EXITKILL as u64),
                ),
                Ok(SUCCESS)
            );

            for signal in [SIGKILL, SIGSTOP] {
                // Place `init` into a ptrace stop so `PTRACE_KILL` and `PTRACE_CONT` are accepted.
                current_task.write().set_stopped(
                    StopState::SignalDeliveryStopping,
                    Some(SignalInfo::kernel(SIGSTOP)),
                    None,
                    None,
                );
                current_task.write().set_stopped(
                    StopState::SignalDeliveryStopped,
                    None,
                    None,
                    None,
                );

                if signal == SIGKILL {
                    assert_eq!(
                        ptrace_dispatch(
                            &mut tracer,
                            PTRACE_KILL,
                            &current_task.tid,
                            UserAddress::NULL,
                            UserAddress::NULL,
                        ),
                        Ok(SUCCESS)
                    );
                    assert!(current_task.thread_group().read().is_running());
                    assert!(!current_task.is_exitted());
                }

                // `PTRACE_KILL` does not wake the tracee; resume via `PTRACE_CONT` with `signal`
                // and verify `init` ignores it while waking from the ptrace stop.
                assert_eq!(
                    ptrace_dispatch(
                        &mut tracer,
                        PTRACE_CONT,
                        &current_task.tid,
                        UserAddress::NULL,
                        UserAddress::from(signal.number() as u64),
                    ),
                    Ok(SUCCESS)
                );
                assert!(current_task.thread_group().read().is_running());
                assert!(!current_task.is_exitted());
                assert_eq!(current_task.load_stopped(), StopState::Waking);
                assert_eq!(current_task.read().queued_signal_count(signal), 0);
            }

            // When `init` is ptraced, `SIGSTOP` is still queued so the tracer can observe
            // signal-delivery-stop, and is ignored if delivered after detach.
            current_task.write().set_stopped(StopState::Awake, None, None, None);
            current_task.thread_group().set_stopped(StopState::Awake, None, false);
            assert_eq!(
                current_task.thread_group().send_signal_unchecked(current_task, SIGSTOP.into()),
                Ok(())
            );
            assert!(current_task.thread_group().has_signal_queued(SIGSTOP));
            dequeue_signal(current_task);
            assert_eq!(current_task.load_stopped(), StopState::SignalDeliveryStopping);
            current_task.write().set_stopped(StopState::Awake, None, None, None);

            assert_eq!(send_signal(current_task, SignalInfo::kernel(SIGSTOP)), Ok(()));
            assert_eq!(current_task.read().queued_signal_count(SIGSTOP), 1);

            // Dropping the tracer triggers `detach_ptracees` with `PTRACE_O_EXITKILL`, which must
            // also not terminate `init`.
            std::mem::drop(tracer);
            assert!(current_task.thread_group().read().is_running());
            assert!(!current_task.is_exitted());
            assert_eq!(current_task.read().queued_signal_count(SIGKILL), 0);

            // Dequeuing the `SIGSTOP` that was queued while ptraced now goes through
            // `deliver_signal`, which ignores it for `init`.
            dequeue_signal(current_task);
            assert_eq!(current_task.read().queued_signal_count(SIGSTOP), 0);
            assert_eq!(current_task.thread_group().load_stopped(), StopState::Awake);
            assert!(current_task.thread_group().read().is_running());
            assert!(!current_task.is_exitted());

            // Forced kernel SIGKILL (such as during container shutdown) is honored by `init`.
            current_task.thread_group().write().send_signal(SignalInfo::forced(SIGKILL));
            assert!(!current_task.thread_group().read().is_running());
            assert!(current_task.is_exitted());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_sigkill_fast_path_exiting() {
        spawn_kernel_and_run(async |current_task| {
            let clone_thread_flags = (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) as u64;

            // Process-directed SIGKILL synchronously transitions the thread group and all tasks.
            let process_killed_leader = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let process_killed_thread =
                process_killed_leader.clone_task_for_test(clone_thread_flags, None);
            assert!(process_killed_leader.thread_group().read().is_running());
            assert!(!process_killed_leader.is_exitted());
            assert!(!process_killed_thread.is_exitted());

            assert_eq!(
                process_killed_leader
                    .thread_group()
                    .send_signal_unchecked(current_task, SIGKILL.into()),
                Ok(())
            );
            assert!(!process_killed_leader.thread_group().read().is_running());
            assert!(process_killed_leader.is_exitted());
            assert!(process_killed_thread.is_exitted());
            std::mem::drop(process_killed_thread);
            std::mem::drop(process_killed_leader);

            // Task-directed SIGKILL also synchronously transitions the thread group and all tasks.
            let task_killed_leader = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let task_killed_thread =
                task_killed_leader.clone_task_for_test(clone_thread_flags, None);
            let task_sigkill_info = SignalInfo::with_sender(
                SIGKILL,
                SI_TKILL,
                SignalDetail::Kill {
                    pid: current_task.pid.clone(),
                    uid: current_task.current_creds().uid,
                },
                Some(current_task.weak_self.clone()),
            );
            assert_eq!(send_signal(&task_killed_thread, task_sigkill_info), Ok(()));
            assert!(!task_killed_leader.thread_group().read().is_running());
            assert!(task_killed_leader.is_exitted());
            assert!(task_killed_thread.is_exitted());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setgpid() {
        spawn_kernel_and_run(async |current_task| {
            assert_eq!(current_task.thread_group().setsid(), error!(EPERM));

            let child_task1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child_task2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let execd_child_task = current_task.clone_task_for_test(0, Some(SIGCHLD));
            execd_child_task.thread_group().write().did_exec = true;
            let other_session_child_task = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(other_session_child_task.thread_group().setsid(), Ok(()));

            assert_eq!(
                child_task1.thread_group().setpgid(&current_task, &current_task, &current_task.pid),
                error!(ESRCH)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    &current_task,
                    &execd_child_task,
                    &execd_child_task.pid
                ),
                error!(EACCES)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    &current_task,
                    &current_task,
                    &current_task.pid
                ),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    &current_task,
                    &other_session_child_task,
                    &other_session_child_task.pid
                ),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child_task1, &child_task2.pid),
                error!(EPERM)
            );
            assert_eq!(
                current_task.thread_group().setpgid(
                    &current_task,
                    &child_task1,
                    &other_session_child_task.pid
                ),
                error!(EPERM)
            );

            assert_eq!(
                child_task1.thread_group().setpgid(&current_task, &child_task1, &child_task1.pid),
                Ok(())
            );
            assert_eq!(child_task1.thread_group().read().session, current_task.tid);
            assert_eq!(child_task1.thread_group().pgid(), child_task1.tid);

            let old_process_group = child_task2.thread_group().pgid();
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child_task2, &child_task1.pid),
                Ok(())
            );
            assert_eq!(child_task2.thread_group().pgid(), child_task1.tid);
            let scope = RcuReadScope::new();
            assert!(
                !old_process_group
                    .pgid_thread_groups(&scope)
                    .any(|tg| Arc::ptr_eq(&tg, child_task2.thread_group()))
            );

            let child_task3 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                child_task3.thread_group().setpgid(&current_task, &child_task3, &child_task3.pid),
                Ok(())
            );
            // Move child_task1 to child_task3's process group.
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child_task1, &child_task3.pid),
                Ok(())
            );
            assert_eq!(child_task1.thread_group().pgid(), child_task3.tid);

            // Rejoin child_task1's original process group (which still contains child_task2).
            assert_eq!(
                child_task1.thread_group().setpgid(&current_task, &child_task1, &child_task1.pid),
                Ok(())
            );
            assert_eq!(child_task1.thread_group().pgid(), child_task1.tid);
            let pg1 = child_task1.thread_group().pgid();
            let pg2 = child_task2.thread_group().pgid();
            assert_eq!(pg1, pg2);

            assert_eq!(
                crate::task::syscalls::sys_setpgid(&current_task, child_task1.pid.id, -1),
                error!(EINVAL)
            );
            assert_eq!(
                crate::task::syscalls::sys_setpgid(&current_task, child_task1.pid.id, 255),
                error!(EPERM)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_adopt_children() {
        spawn_kernel_and_run(async |current_task| {
            let task1 = current_task.clone_task_for_test(0, None);
            let task2 = task1.clone_task_for_test(0, None);
            let task3 = task2.clone_task_for_test(0, None);

            assert_eq!(task3.thread_group().read().get_ppid(), task2.tid.id);
            assert!(current_task.thread_group().read().children.contains(&task1.pid));
            assert!(task1.thread_group().read().children.contains(&task2.pid));
            assert!(task2.thread_group().read().children.contains(&task3.pid));

            task2.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(task2);

            // Task3 parent should be current_task.
            assert_eq!(task3.thread_group().read().get_ppid(), current_task.tid.id);
            assert!(current_task.thread_group().read().children.contains(&task3.pid));
            assert!(!task1.thread_group().read().children.contains(&task3.pid));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_thread_group_children_lifecycle() {
        spawn_kernel_and_run(async |current_task| {
            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child1_pid = child1.pid.clone();
            let child2_pid = child2.pid.clone();

            assert!(current_task.thread_group().read().children.contains(&child1_pid));
            assert!(current_task.thread_group().read().children.contains(&child2_pid));

            let children_pids: HashSet<Pid> =
                current_task.thread_group().read().children().map(|tg| tg.leader.clone()).collect();
            assert!(children_pids.contains(&child1_pid));
            assert!(children_pids.contains(&child2_pid));

            child1.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child1);

            assert!(child1_pid.get_thread_group().is_err());
            assert!(!current_task.thread_group().read().children.contains(&child1_pid));
            assert!(current_task.thread_group().read().children.contains(&child2_pid));
            let children_pids: HashSet<Pid> =
                current_task.thread_group().read().children().map(|tg| tg.leader.clone()).collect();
            assert!(!children_pids.contains(&child1_pid));
            assert!(children_pids.contains(&child2_pid));

            child2.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child2);

            assert!(child2_pid.get_thread_group().is_err());
            assert!(current_task.thread_group().read().children.is_empty());
            assert_eq!(current_task.thread_group().read().children().count(), 0);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ptrace_unspawned_child_drop() {
        spawn_kernel_and_run(async |current_task| {
            let mut child = create_task(current_task.kernel(), "tracee");
            let child_pid = child.pid.clone();
            assert!(current_task.thread_group().read().children.contains(&child_pid));

            assert_eq!(ptrace_traceme(&mut child), Ok(SUCCESS));
            child.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child);

            assert!(!current_task.thread_group().read().children.contains(&child_pid));
            assert!(current_task.thread_group().read().zombie_children.is_empty());

            // Also verify the case where an unspawned non-leader thread in a multi-threaded child
            // (`tracee.tid != tracee.pid`) is traced by its parent and exits last.
            let child_leader = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child_pid = child_leader.pid.clone();
            let mut child_thread: AutoReleasableTask = child_leader
                .clone_task(
                    (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) as u64,
                    None,
                    UserRef::default(),
                    UserRef::default(),
                    UserRef::default(),
                )
                .expect("failed to create thread")
                .into();
            assert_ne!(child_thread.tid, child_pid);
            assert_eq!(child_thread.pid, child_pid);
            assert!(current_task.thread_group().read().children.contains(&child_pid));

            // Exit the leader thread first so `child_thread` is the last task in the thread group.
            child_leader.write().set_exit_status(ExitStatus::Exit(0));
            std::mem::drop(child_leader);
            assert!(current_task.thread_group().read().children.contains(&child_pid));

            assert_eq!(ptrace_traceme(&mut child_thread), Ok(SUCCESS));
            child_thread.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child_thread);

            assert!(!current_task.thread_group().read().children.contains(&child_pid));
            assert!(current_task.thread_group().read().zombie_children.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_getppid_after_self_and_parent_exit() {
        spawn_kernel_and_run(async |current_task| {
            let task1 = current_task.clone_task_for_test(0, None);
            let task2 = task1.clone_task_for_test(0, None);

            // Take strong references to the ThreadGroups.
            let tg1 = task1.thread_group().clone();
            let tg2 = task2.thread_group().clone();

            assert_eq!(tg1.read().get_ppid(), current_task.tid.id);
            assert_eq!(tg2.read().get_ppid(), task1.tid.id);

            // Exit `task2` first, so that when `task1` exits, it will not be reparented to init.
            tg2.kill(ExitStatus::Exit(0), None);
            std::mem::drop(task2);

            // Exit `task1`, and drop the task and ThreadGroup.
            tg1.kill(ExitStatus::Exit(0), None);
            std::mem::drop(task1);
            std::mem::drop(tg1);

            // It should still be valid to call `get_ppid()` on `tg2`, though is parent ThreadGroup
            // no longer exists.
            let _ = tg2.read().get_ppid();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_thread_group_rcu_links_and_adapters() {
        spawn_kernel_and_run(async |current_task| {
            let scope = RcuReadScope::new();
            let tg = current_task.thread_group();
            let node_ptr = tg.pgrp_node.read(&scope);
            assert!(!node_ptr.is_null());
            let link_ptr = ThreadGroupLinkAdapter::to_link(node_ptr);
            assert_eq!(ThreadGroupLinkAdapter::from_link(link_ptr).as_ptr(), node_ptr.as_ptr());

            let init_pid = current_task.pid.clone();

            assert!(init_pid.is_process_group(&scope));
            assert!(init_pid.is_session(&scope));
            assert_eq!(init_pid.pgid_thread_groups(&scope).count(), 1);
            assert_eq!(init_pid.sid_thread_groups(&scope).count(), 1);

            let child = current_task.clone_task_for_test(0, None);
            assert_eq!(init_pid.pgid_thread_groups(&scope).count(), 2);
            assert_eq!(init_pid.sid_thread_groups(&scope).count(), 2);

            // Moving `child` into its own process group within the same session updates PGID
            // membership without detaching SID membership.
            child
                .thread_group()
                .setpgid(&current_task, &child, &child.pid)
                .expect("setpgid failed");
            assert_eq!(init_pid.pgid_thread_groups(&scope).count(), 1);
            assert_eq!(init_pid.sid_thread_groups(&scope).count(), 2);
            assert!(child.pid.is_process_group(&scope));
            assert!(!child.pid.is_session(&scope));
            assert_eq!(child.pid.pgid_thread_groups(&scope).count(), 1);

            // Move `child` back to `init_pid`'s process group so `setsid()` succeeds.
            child
                .thread_group()
                .setpgid(&current_task, &child, &init_pid)
                .expect("setpgid back to init failed");
            assert_eq!(init_pid.pgid_thread_groups(&scope).count(), 2);
            assert_eq!(init_pid.sid_thread_groups(&scope).count(), 2);

            child.thread_group().setsid().expect("setsid failed");
            assert_eq!(init_pid.pgid_thread_groups(&scope).count(), 1);
            assert_eq!(init_pid.sid_thread_groups(&scope).count(), 1);
            assert!(child.pid.is_process_group(&scope));
            assert!(child.pid.is_session(&scope));
            assert_eq!(child.pid.pgid_thread_groups(&scope).count(), 1);
            assert_eq!(child.pid.sid_thread_groups(&scope).count(), 1);

            // Clone a grandchild in `child`'s session, then exit and reap `child` (the session
            // leader). Calling `setpgid` on `grandchild` within that session must retain `child`'s
            // session `PidEntry` in `PidTable`.
            let grandchild = child.clone_task_for_test(0, None);
            let child_pid = child.pid.clone();
            child.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child);
            {
                let mut pids = current_task.kernel().pids.lock();
                for zombie in current_task.thread_group().write().zombie_children.drain(..) {
                    zombie.release(&mut pids);
                }
            }
            assert!(child_pid.get_process().is_none());
            assert!(current_task.kernel().pids.get(child_pid.id).is_ok());

            grandchild
                .thread_group()
                .setpgid(&grandchild, &grandchild, &grandchild.pid)
                .expect("grandchild setpgid failed");
            assert!(!child_pid.is_process_group(&scope));
            assert!(child_pid.is_session(&scope));
            assert!(current_task.kernel().pids.get(child_pid.id).is_ok());
            assert!(grandchild.pid.is_process_group(&scope));

            grandchild.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(grandchild);
            {
                let mut pids = current_task.kernel().pids.lock();
                for zombie in current_task.thread_group().write().zombie_children.drain(..) {
                    zombie.release(&mut pids);
                }
            }

            assert!(!child_pid.is_process_group(&scope));
            assert!(!child_pid.is_session(&scope));
            assert!(current_task.kernel().pids.get(child_pid.id).is_err());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_orphaned_pgrp_signals_stopped_jobs() {
        spawn_kernel_and_run(async |init| {
            // 1. Exiting a job-controlling parent orphans the child's process group and sends
            // SIGHUP + SIGCONT to stopped jobs via `kill_orphaned_pgrp`.
            let parent = init.clone_task_for_test(0, Some(SIGCHLD));
            parent.thread_group().setsid().expect("setsid");
            let child = parent.clone_task_for_test(0, Some(SIGCHLD));
            child.thread_group().setpgid(&child, &child, &child.pid).expect("setpgid");

            child.thread_group().set_stopped(StopState::GroupStopped, None, false);
            assert_eq!(child.thread_group().load_stopped(), StopState::GroupStopped);
            assert!(!child.read().has_signal_pending(SIGHUP));

            parent.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(parent);

            assert!(child.read().has_signal_pending(SIGHUP));
            assert_ne!(child.thread_group().load_stopped(), StopState::GroupStopped);

            child.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(child);

            // 2. A parent leaving the session via `setsid` orphans its child's process group and
            // sends SIGHUP + SIGCONT to stopped jobs via `check_orphaned_pgrp`.
            let session_leader = init.clone_task_for_test(0, Some(SIGCHLD));
            session_leader.thread_group().setsid().expect("setsid");
            let middle = session_leader.clone_task_for_test(0, Some(SIGCHLD));
            let leaf = middle.clone_task_for_test(0, Some(SIGCHLD));
            leaf.thread_group().setpgid(&leaf, &leaf, &leaf.pid).expect("setpgid");

            leaf.thread_group().set_stopped(StopState::GroupStopped, None, false);
            assert_eq!(leaf.thread_group().load_stopped(), StopState::GroupStopped);
            assert!(!leaf.read().has_signal_pending(SIGHUP));

            middle.thread_group().setsid().expect("middle setsid");

            assert!(leaf.read().has_signal_pending(SIGHUP));
            assert_ne!(leaf.thread_group().load_stopped(), StopState::GroupStopped);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setpgid_multithreaded_task_migration() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn a child process (thread group leader) and a secondary thread.
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child_pid = child.get_pid();
            let child_thread = child.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );

            let old_pgid_entry = kernel.pids.get(current_task.get_pid()).unwrap();
            let scope = RcuReadScope::new();

            // Verify child thread group starts in current_task's process group.
            let initial_members: Vec<_> =
                old_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(initial_members.contains(&child_pid));

            // Move the multithreaded child process to its own process group (PGID = child.pid).
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child, &child.pid),
                Ok(())
            );

            // Verify child thread group was removed from the old process group.
            let old_members: Vec<_> =
                old_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(!old_members.contains(&child_pid));

            // Verify child thread group and all its threads observe the new process group.
            let new_pgid_entry = kernel.pids.get(child_pid).unwrap();
            let new_members: Vec<_> =
                new_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(new_members, vec![child_pid]);
            assert_eq!(child_thread.thread_group().pgid(), child.pid);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setsid_multithreaded_task_migration() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn a child process (thread group leader) and a secondary thread.
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child_pid = child.get_pid();
            let child_thread = child.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );

            let old_sid_entry = kernel.pids.get(current_task.get_pid()).unwrap();
            let scope = RcuReadScope::new();

            // Verify child thread group starts in parent's session.
            let initial_sid_members: Vec<_> =
                old_sid_entry.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(initial_sid_members.contains(&child_pid));

            // Call setsid on the multithreaded child thread group.
            assert_eq!(child.thread_group().setsid(), Ok(()));

            // Verify child thread group was removed from the parent's session.
            let old_sid_members: Vec<_> =
                old_sid_entry.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(!old_sid_members.contains(&child_pid));

            // Verify child thread group is attached to the new session and process group.
            let new_pid_entry = kernel.pids.get(child_pid).unwrap();
            let new_sid_members: Vec<_> =
                new_pid_entry.sid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(new_sid_members, vec![child_pid]);

            let new_pgid_members: Vec<_> =
                new_pid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(new_pgid_members, vec![child_pid]);
            assert_eq!(child_thread.thread_group().read().session, child.pid);
            assert_eq!(child_thread.thread_group().pgid(), child.pid);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_orphaned_pgrp_detection_and_signaling() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn a parent process (PID 2 != 1) so it is not treated as init.
            let parent_proc = current_task.clone_task_for_test(0, Some(SIGCHLD));

            // Spawn a child process from parent_proc and move to a new process group.
            let child = parent_proc.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                parent_proc.thread_group().setpgid(&parent_proc, &child, &child.pid),
                Ok(())
            );

            let child_pgid_entry = kernel.pids.get(child.get_pid()).unwrap();
            let scope = RcuReadScope::new();

            // Child's parent is parent_proc (PID != 1), which is in the same session but outside the group.
            // Therefore, the group is not orphaned.
            assert!(!child_pgid_entry.will_become_orphaned_pgrp(&scope, None));

            // If parent_proc is ignored (as if exiting), the child group becomes orphaned.
            assert!(
                child_pgid_entry
                    .will_become_orphaned_pgrp(&scope, Some(parent_proc.thread_group()))
            );

            // Verify has_stopped_jobs is initially false.
            assert!(!child_pgid_entry.has_stopped_jobs(&scope));

            // Set stopped state on child thread group.
            child.thread_group().set_stopped(StopState::GroupStopped, None, false);
            assert!(child_pgid_entry.has_stopped_jobs(&scope));

            // Verify kill_orphaned_pgrp delivers SIGHUP and SIGCONT.
            {
                let pids = kernel.pids.lock();
                child.thread_group().kill_orphaned_pgrp(
                    &pids,
                    Some(parent_proc.thread_group()),
                    Some(parent_proc.thread_group()),
                );
            }
            assert!(child.thread_group().has_signal_queued(SIGHUP));
            assert!(child.thread_group().has_signal_queued(SIGCONT));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setsid_rejected_on_process_group_leader() {
        spawn_kernel_and_run(async |current_task| {
            // PID 1 is already a session and process group leader.
            assert_eq!(current_task.thread_group().setsid(), error!(EPERM));

            // Spawn child 1.
            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));

            // Move child 1 to its own process group (becomes process group leader).
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child1, &child1.pid),
                Ok(())
            );
            assert_eq!(child1.thread_group().pgid(), child1.tid);

            // setsid on process group leader must be rejected with EPERM.
            assert_eq!(child1.thread_group().setsid(), error!(EPERM));

            // Spawn child 2 in the same process group as child 1.
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child2, &child1.pid),
                Ok(())
            );
            // Child 2 is a member but not the leader of child 1's process group.
            assert_eq!(child2.thread_group().pgid(), child1.tid);

            // Child 2 is not a process group leader, so setsid must succeed.
            assert_eq!(child2.thread_group().setsid(), Ok(()));
            assert_eq!(child2.thread_group().pgid(), child2.tid);
            assert_eq!(child2.thread_group().read().session, child2.tid);

            // Child 2 is now a process group leader; subsequent setsid must fail with EPERM.
            assert_eq!(child2.thread_group().setsid(), error!(EPERM));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setpgid_many_threads_stress_and_teardown() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn child process leader and 7 worker threads (8 threads total).
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let child_pid = child.get_pid();
            let mut thread_tasks = vec![];
            for _ in 0..7 {
                let thread = child.clone_task_for_test(
                    (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                    Some(SIGCHLD),
                );
                thread_tasks.push(thread);
            }
            assert_eq!(child.thread_group().read().tasks_count(), 8);

            let old_pgid_entry = kernel.pids.get(current_task.get_pid()).unwrap();
            let scope = RcuReadScope::new();

            // Child thread group initially belongs to current_task's process group.
            let initial_members: Vec<_> =
                old_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(initial_members.contains(&child_pid));

            // Move entire child thread group to its own process group.
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child, &child.pid),
                Ok(())
            );

            // Verify child thread group left the old process group and joined the new one once.
            let old_members_after: Vec<_> =
                old_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert!(!old_members_after.contains(&child_pid));

            let new_pgid_entry = kernel.pids.get(child_pid).unwrap();
            let new_members: Vec<_> =
                new_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(new_members, vec![child_pid]);

            // Spawn another child process to act as target process group.
            let target_child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                current_task.thread_group().setpgid(
                    &current_task,
                    &target_child,
                    &target_child.pid
                ),
                Ok(())
            );

            // Migrate multithreaded child process to target_child's process group.
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child, &target_child.pid),
                Ok(())
            );

            // Verify child's vacated PGID list is empty.
            assert_eq!(new_pgid_entry.pgid_thread_groups(&scope).count(), 0);

            // Verify target PGID list contains target_child and child thread groups.
            let target_pgid_entry = kernel.pids.get(target_child.get_pid()).unwrap();
            let target_members: Vec<_> =
                target_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(target_members.len(), 2);
            assert!(target_members.contains(&child_pid));
            assert!(target_members.contains(&target_child.get_pid()));

            // Dropping worker threads one by one keeps child's thread group attached while its leader lives.
            for thread in thread_tasks {
                drop(thread);
                let updated_members: Vec<_> =
                    target_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
                assert_eq!(updated_members.len(), 2);
                assert!(updated_members.contains(&child_pid));
            }

            // Dropping the final task in child detaches its thread group, leaving target_child.
            child.thread_group().kill(ExitStatus::Exit(0), None);
            drop(child);
            let remaining_members: Vec<_> =
                target_pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(remaining_members, vec![target_child.get_pid()]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multi_process_orphan_detection_and_signaling() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Session leader P (PID 2 != 1).
            let leader = current_task.clone_task_for_test(0, Some(SIGCHLD));

            // C1 is child of leader.
            let c1 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &c1, &c1.pid), Ok(()));
            let c1_pid = c1.get_pid();
            let c1_pgid_entry = kernel.pids.get(c1_pid).unwrap();

            // C2 is also child of leader, moved into C1's process group.
            let c2 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &c2, &c1.pid), Ok(()));

            // C3 is child of C1, moved into C1's process group.
            let c3 = c1.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(c1.thread_group().setpgid(&c1, &c3, &c1.pid), Ok(()));

            let scope = RcuReadScope::new();

            // While leader is alive: C1 and C2 have parent `leader` (same session, outside PGID).
            // Group is not orphaned.
            assert!(!c1_pgid_entry.will_become_orphaned_pgrp(&scope, None));

            // Mark C1 as stopped. C2 and C3 remain running.
            c1.thread_group().set_stopped(StopState::GroupStopped, None, false);
            assert!(c1_pgid_entry.has_stopped_jobs(&scope));

            // When leader exits (ignored_tg = leader.thread_group()):
            // Neither C1 nor C2 nor C3 has a living parent in the session outside the group.
            assert!(c1_pgid_entry.will_become_orphaned_pgrp(&scope, Some(leader.thread_group())));

            // Trigger orphan killing as occurs during leader exit.
            {
                let pids = kernel.pids.lock();
                c1.thread_group().kill_orphaned_pgrp(
                    &pids,
                    Some(leader.thread_group()),
                    Some(leader.thread_group()),
                );
            }

            // Verify all member processes in the group received SIGHUP and SIGCONT.
            assert!(c1.thread_group().has_signal_queued(SIGHUP));
            assert!(c1.thread_group().has_signal_queued(SIGCONT));
            assert!(c2.thread_group().has_signal_queued(SIGHUP));
            assert!(c2.thread_group().has_signal_queued(SIGCONT));
            assert!(c3.thread_group().has_signal_queued(SIGHUP));
            assert!(c3.thread_group().has_signal_queued(SIGCONT));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_orphan_prevented_by_sibling_external_parent() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Session leader P (PID 2 != 1).
            let leader = current_task.clone_task_for_test(0, Some(SIGCHLD));

            // External parent P2 in same session, but different PGID.
            let p2 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &p2, &p2.pid), Ok(()));

            // C1 is child of leader, in PGID = C1's PID.
            let c1 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &c1, &c1.pid), Ok(()));
            let c1_pid = c1.get_pid();
            let c1_pgid_entry = kernel.pids.get(c1_pid).unwrap();

            // C2 is child of P2, but moved into C1's process group.
            let c2 = p2.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(p2.thread_group().setpgid(&p2, &c2, &c1.pid), Ok(()));

            let scope = RcuReadScope::new();

            // Mark C1 as stopped.
            c1.thread_group().set_stopped(StopState::GroupStopped, None, false);

            // Simulate leader exiting (ignored_tg = leader.thread_group()).
            // C1's parent is leader (exiting).
            // C2 is in the process group, and C2's parent is P2 (running, same session, outside PGID).
            // Therefore, C1's process group is not orphaned even though leader exited.
            assert!(!c1_pgid_entry.will_become_orphaned_pgrp(&scope, Some(leader.thread_group())));

            // If kill_orphaned_pgrp is invoked with leader, no signals should be queued.
            {
                let pids = kernel.pids.lock();
                c1.thread_group().kill_orphaned_pgrp(
                    &pids,
                    Some(leader.thread_group()),
                    Some(leader.thread_group()),
                );
            }
            assert!(!c1.thread_group().has_signal_queued(SIGHUP));
            assert!(!c2.thread_group().has_signal_queued(SIGHUP));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_setpgid_vacated_group_orphaned_and_signaled() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Leader P (PID 2 != 1).
            let leader = current_task.clone_task_for_test(0, Some(SIGCHLD));

            // Child 1 in new process group PGID = C1's PID.
            let c1 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &c1, &c1.pid), Ok(()));
            let c1_pid = c1.get_pid();

            // Child 2 joined to C1's process group.
            let c2 = leader.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(leader.thread_group().setpgid(&leader, &c2, &c1.pid), Ok(()));

            // Mark C1 as stopped.
            c1.thread_group().set_stopped(StopState::GroupStopped, None, false);

            // Set C1's parent to None (as if C1 was reparented to init) and remove C1 from leader's children.
            leader.thread_group().write().children.remove(&c1.tid);
            c1.thread_group().write().parent = None;

            let scope = RcuReadScope::new();
            let c1_pgid_entry = kernel.pids.get(c1_pid).unwrap();

            // Because C2 is in C1's process group and C2's parent is leader (outside group),
            // C1's process group is not orphaned yet.
            assert!(!c1_pgid_entry.will_become_orphaned_pgrp(&scope, None));

            // Move C2 out of C1's process group into its own group.
            assert_eq!(leader.thread_group().setpgid(&leader, &c2, &c2.pid), Ok(()));

            // C1's process group now contains only C1 (whose parent is None/init).
            // Moving C2 out vacated the group's external parent link.
            // C1 was stopped, so C1's process group should now have received SIGHUP and SIGCONT.
            assert!(c1.thread_group().has_signal_queued(SIGHUP));
            assert!(c1.thread_group().has_signal_queued(SIGCONT));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_lifecycle_stress_concurrent_rcu_traversal() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn child process leader in its own session and process group.
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            child.thread_group().setsid().expect("setsid");
            let child_pid = child.get_pid();

            let pgid_entry = kernel.pids.get(child_pid).unwrap();
            let running = Arc::new(AtomicBool::new(true));

            // Background reader 1: iterates pgid_thread_groups and dereferences member groups.
            let r1_entry = pgid_entry.clone();
            let r1_running = Arc::clone(&running);
            let reader1 = std::thread::spawn(move || {
                while r1_running.load(Ordering::Relaxed) {
                    let scope = RcuReadScope::new();
                    let mut count = 0;
                    for tg in r1_entry.pgid_thread_groups(&scope) {
                        count += 1;
                        let _pid = tg.leader.id;
                        let _is_running = tg.read().is_running();
                        assert!(count <= 20, "Cycle or unexpected count in pgid_thread_groups");
                    }
                }
            });

            // Background reader 2: iterates sid_thread_groups and dereferences member groups.
            let r2_entry = pgid_entry.clone();
            let r2_running = Arc::clone(&running);
            let reader2 = std::thread::spawn(move || {
                while r2_running.load(Ordering::Relaxed) {
                    let scope = RcuReadScope::new();
                    let mut count = 0;
                    for tg in r2_entry.sid_thread_groups(&scope) {
                        count += 1;
                        let _pid = tg.leader.id;
                        let _is_running = tg.read().is_running();
                        assert!(count <= 20, "Cycle or unexpected count in sid_thread_groups");
                    }
                }
            });

            // Background reader 3: repeatedly evaluates orphan status and stopped jobs.
            let r3_entry = pgid_entry.clone();
            let r3_running = Arc::clone(&running);
            let reader3 = std::thread::spawn(move || {
                while r3_running.load(Ordering::Relaxed) {
                    let scope = RcuReadScope::new();
                    let _orphaned = r3_entry.will_become_orphaned_pgrp(&scope, None);
                    let _has_stopped = r3_entry.has_stopped_jobs(&scope);
                }
            });

            // Background worker 4: repeatedly dispatches signals to the process group.
            let r4_entry = pgid_entry.clone();
            let r4_running = Arc::clone(&running);
            let signaler = std::thread::spawn(move || {
                while r4_running.load(Ordering::Relaxed) {
                    r4_entry.send_signals_to_pgid(&[SIGCONT]);
                }
            });

            // Writer loop: rapidly spawn, kill, and reap child processes while readers run.
            for _ in 0..15 {
                let mut procs = Vec::new();
                for _ in 0..4 {
                    let p = child.clone_task_for_test(0, Some(SIGCHLD));
                    procs.push(p);
                }

                for p in procs.drain(..2) {
                    p.thread_group().kill(ExitStatus::Exit(0), None);
                    drop(p);
                }

                let p = child.clone_task_for_test(0, Some(SIGCHLD));
                procs.push(p);

                for p in procs {
                    p.thread_group().kill(ExitStatus::Exit(0), None);
                    drop(p);
                }

                let mut pids = kernel.pids.lock();
                for zombie in child.thread_group().write().zombie_children.drain(..) {
                    zombie.release(&mut pids);
                }
            }

            running.store(false, Ordering::Relaxed);
            reader1.join().expect("Reader 1 failed");
            reader2.join().expect("Reader 2 failed");
            reader3.join().expect("Reader 3 failed");
            signaler.join().expect("Signaler failed");

            // Final state verification.
            let scope = RcuReadScope::new();
            let remaining: Vec<_> =
                pgid_entry.pgid_thread_groups(&scope).map(|tg| tg.leader.id).collect();
            assert_eq!(remaining, vec![child_pid]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_process_group_thread_groups_deduplicates_multithreaded() {
        spawn_kernel_and_run(async |current_task| {
            // Spawn child process 1 and move it into its own process group.
            let child1 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child1, &child1.pid),
                Ok(())
            );
            let pgrp = child1.thread_group().pgid();

            // Spawn 3 additional threads in child1 (4 tasks total in child1).
            let _c1_t1 = child1.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );
            let _c1_t2 = child1.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );
            let _c1_t3 = child1.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );

            // Spawn child process 2, move it into child1's process group, and spawn 2 threads (3 tasks total in child2).
            let child2 = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child2, &child1.pid),
                Ok(())
            );
            let _c2_t1 = child2.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );
            let _c2_t2 = child2.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );

            // Verify PidEntry::pgid_thread_groups() yields each distinct ThreadGroup once.
            let scope = RcuReadScope::new();
            let groups: Vec<_> = pgrp.pgid_thread_groups(&scope).collect();
            assert_eq!(groups.len(), 2);
            assert!(groups.contains(child1.thread_group()));
            assert!(groups.contains(child2.thread_group()));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_thread_group_remove_detaches_last_task() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();

            // Spawn a child process in its own process group.
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            assert_eq!(
                current_task.thread_group().setpgid(&current_task, &child, &child.pid),
                Ok(())
            );
            let child_pid = child.get_pid();
            let pgid_entry = kernel.pids.get(child_pid).unwrap();
            let sid_entry = kernel.pids.get(current_task.get_pid()).unwrap();

            // Spawn a second thread in the child process.
            let extra_thread = child.clone_task_for_test(
                (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64,
                Some(SIGCHLD),
            );

            // Dropping the non-final thread leaves the thread group attached to PGID and SID.
            drop(extra_thread);
            let scope = RcuReadScope::new();
            assert!(pgid_entry.pgid_thread_groups(&scope).any(|tg| tg.leader.id == child_pid));
            assert!(sid_entry.sid_thread_groups(&scope).any(|tg| tg.leader.id == child_pid));

            // Killing and dropping the final task detaches the thread group from PGID and SID.
            child.thread_group().kill(ExitStatus::Exit(0), None);
            drop(child);
            assert!(!pgid_entry.pgid_thread_groups(&scope).any(|tg| tg.leader.id == child_pid));
            assert!(!sid_entry.sid_thread_groups(&scope).any(|tg| tg.leader.id == child_pid));
        })
        .await;
    }
}
