// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub use crate::backtrace::Backtrace;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::ffi::{CStr, c_char, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{PinInit, pin_data, pinned_drop};
use thread_lock_guard_bindings as lock_guard_bindings;
use zr::{Opaque, OpaqueFacade};
use zx_status::Status;
use zx_types::{zx_instant_mono_t, zx_status_t};

use crate::kconcurrent::{ChainLock, ChainLockTransactionToken};
use crate::kernel::restricted_state::RestrictedState;
use crate::kernel::scheduler_state::{SchedulerState, SchedulerStateBaseProfile};
use crate::kernel::types::{cpu_mask_t, cpu_num_t};
use crate::ktrace_rs::InternedString;
use crate::vm::vm_aspace::VmAspace;

pub use crate::kconcurrent::CallsiteInfo as CltTag;
use crate::kconcurrent::{ChainLockProof, ChainLocked, ClassedChainLock};

/// Lock class for data guarded by a [`Thread`]'s chain lock.
///
/// A marker only: tokens of this class exist solely inside a [`ChainLockProof`] for a [`Thread`]
/// owner, never via `KMutex`.
pub enum ThreadLockClass {}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_thread_create_default(
        name: *const c_char,
        entry: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
    ) -> *mut Thread;
    fn cpp_thread_create_with_priority(
        name: *const c_char,
        entry: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
        priority: i32,
    ) -> *mut Thread;
    fn cpp_thread_create_with_profile(
        name_ptr: *const c_char,
        name_len: usize,
        entry: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
        profile: *const SchedulerStateBaseProfile,
    ) -> *mut Thread;
    fn cpp_thread_set_base_profile(thread: *mut Thread, profile: *const SchedulerStateBaseProfile);
    fn cpp_thread_resume(thread: *mut Thread);
    fn cpp_thread_join(
        thread: *mut Thread,
        out_retcode: *mut i32,
        deadline: zx_instant_mono_t,
    ) -> i32;
    fn cpp_thread_current_yield();
    fn cpp_thread_kill(thread: *mut Thread);
    fn cpp_thread_suspend(thread: *mut Thread) -> zx_status_t;
    fn cpp_thread_restricted_kick(thread: *mut Thread) -> zx_status_t;
    fn cpp_thread_set_memory_stall_state_inactive(thread: *mut Thread);
    fn cpp_thread_current_exit(retcode: i32) -> !;
    fn cpp_thread_current_kill();
    fn cpp_thread_is_blocked(thread: *mut Thread) -> bool;
    fn cpp_thread_lock_guard_init(
        guard: *mut lock_guard_bindings::ThreadLockGuard,
        thread: *mut Thread,
        label: &'static InternedString,
        line: u32,
    );
    fn cpp_thread_lock_guard_destroy(guard: *mut lock_guard_bindings::ThreadLockGuard);
    fn cpp_thread_current_get() -> *mut Thread;
    fn cpp_thread_current_active_aspace() -> *mut VmAspace;
    fn cpp_thread_fxt_ref(thread: *mut Thread) -> FxtRef;
    fn cpp_thread_preempt_set_timeslice_extension(duration: DurationMono) -> bool;
    fn cpp_thread_preempt_clear_timeslice_extension();
    fn cpp_thread_preempt_disable();
    fn cpp_thread_preempt_reenable();
    fn cpp_thread_eager_resched_disable();
    fn cpp_thread_eager_resched_reenable();
    fn cpp_thread_preempt_disable_count() -> u32;
    fn cpp_thread_eager_resched_disable_count() -> u32;
    fn cpp_thread_preempt_set_pending();
    fn cpp_thread_preempt_is_enabled() -> bool;
    fn cpp_thread_preempt();
    fn cpp_thread_current_sleep_etc(
        deadline: *const crate::kernel::deadline::Deadline,
        interruptible: Interruptible,
        now: zx_instant_mono_t,
    ) -> zx_status_t;
    fn cpp_thread_current_sleep(deadline: InstantMono) -> zx_status_t;
    fn cpp_thread_current_sleep_relative(duration: DurationMono) -> zx_status_t;
    fn cpp_thread_current_sleep_interruptible(deadline: InstantMono) -> zx_status_t;
    fn cpp_thread_current_soft_fault(va: usize, flags: u32) -> zx_status_t;
    fn cpp_thread_get_arch(thread: *mut Thread) -> *mut c_void;
    fn cpp_thread_get_stack_top(thread: *mut Thread) -> usize;
    fn cpp_thread_get_shadow_call_base(thread: *mut Thread) -> usize;
    fn cpp_thread_dump_current_stack();
    fn cpp_thread_is_user_state_saved_locked(thread: *mut Thread) -> bool;
    fn cpp_thread_is_running(thread: *const Thread) -> bool;
    fn cpp_thread_name(thread: *const Thread) -> *const c_char;
    fn cpp_thread_process_pending_signals(frame: *mut c_void);
    fn cpp_thread_in_restricted(thread: *mut Thread) -> bool;
    fn cpp_thread_is_user_thread(thread: *const Thread) -> bool;
    fn cpp_thread_active_aspace(thread: *mut Thread) -> *mut VmAspace;
    fn cpp_thread_current_restricted_state() -> *mut RestrictedState;
    fn cpp_thread_current_set_restricted_state(raw_rs: *mut RestrictedState);
    fn cpp_thread_current_is_signaled() -> bool;
    fn cpp_thread_current_check_for_restricted_kick() -> bool;
    fn cpp_thread_current_memory_allocation_state_enable();
    fn cpp_thread_current_memory_allocation_state_disable();
    fn cpp_thread_current_memory_allocation_state_is_enabled() -> bool;
    fn cpp_thread_current_signal_policy_exception(
        policy_exception_code: u32,
        policy_exception_data: u32,
    );
    fn cpp_thread_memory_stall_state(thread: *const Thread) -> ThreadStallState;
    fn cpp_thread_set_memory_stall_state(thread: *mut Thread, value: ThreadStallState);
    fn cpp_thread_scheduler_state(thread: *const Thread) -> *const SchedulerState;
    fn cpp_thread_get_cpu_affinity(thread: *const Thread) -> cpu_mask_t;
    fn cpp_thread_set_cpu_affinity(thread: *mut Thread, affinity: cpu_mask_t) -> cpu_mask_t;
    fn cpp_thread_current_migrate_to_cpu(target_cpu: cpu_num_t);
    fn cpp_thread_get_lock(thread: *mut Thread) -> *mut ChainLock;
    fn cpp_thread_wait_queue_state_blocked_status_locked(thread: *const Thread) -> zx_status_t;
    fn cpp_thread_user_thread(thread: *mut Thread) -> *mut crate::object::ThreadDispatcher;
    fn cpp_thread_tid(thread: *const Thread) -> zx_types::zx_koid_t;
    fn cpp_thread_lock_flow_id(thread: *const Thread) -> u64;
    fn cpp_thread_take_next_lock_flow_id(thread: *mut Thread) -> u64;
    fn cpp_thread_get_list_lock() -> *mut ksync::KSpinlock<ThreadListLockClass>;
    fn cpp_thread_id_to_thread_slow(tid: zx_types::zx_koid_t) -> *mut Thread;
    fn cpp_thread_get_backtrace(thread: *mut Thread, out_bt: *mut Backtrace);
    fn cpp_thread_dump(thread: *const Thread, full: bool);
    fn cpp_thread_canary_assert(thread: *const Thread);
    fn cpp_thread_signals(thread: *const Thread) -> u32;
    fn cpp_thread_check_kill_or_suspend_signal(thread: *const Thread) -> zx_status_t;
    fn cpp_thread_state(thread: *const Thread) -> crate::kernel::scheduler_state::ThreadStateKind;
    fn cpp_thread_set_blocked(thread: *mut Thread);
    fn cpp_thread_set_blocked_read_lock(thread: *mut Thread);
    fn cpp_thread_wait_queue_state(
        thread: *const Thread,
    ) -> *const crate::kernel::wait_queue::WaitQueueCollectionThreadState;
    // `scheduler.rs` declares `cpp_thread_get_scheduler_state` returning
    // `*mut crate::kernel::scheduler::SchedulerState` (a distinct type from
    // `scheduler_state::SchedulerState`), so this declaration must match to satisfy
    // `-D clashing-extern-declarations`.
    fn cpp_thread_get_scheduler_state(
        thread: *mut Thread,
    ) -> *mut crate::kernel::scheduler::SchedulerState;
    fn cpp_thread_dump_tid(tid: zx_types::zx_koid_t, full: bool);
    fn cpp_thread_dump_tid_during_panic(tid: zx_types::zx_koid_t, full: bool);
    fn cpp_thread_dump_all(match_owner: *const c_char, full: bool);
    fn cpp_thread_dump_all_during_panic(match_owner: *const c_char, full: bool);
    fn cpp_thread_current_reschedule();
}

/// Stall accounting state of a thread.
///
/// Kernel-only threads are never considered for the purpose of memory stall accounting
/// (`IgnoredKernelOnly`).
///
/// If a user thread is not signalling a stall, it will contribute to the current CPU's memory
/// stall accumulator as `Progressing` while running and do not contribute (`Inactive`) in any
/// other state.
///
/// If a user thread signals that a stall is ongoing, it will contribute to the current CPU's
/// accumulator as `Stalling`. If preempted, the contribution will persist on the last CPU's
/// accumulator. Normal operation is resumed when the thread signals that the stall is over.
// LINT.IfChange(ThreadStallState)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStallState {
    IgnoredKernelOnly = 0,
    Inactive = 1,
    Progressing = 2,
    Stalling = 3,
}
// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:ThreadStallState)

// `enum class ThreadStallState` has no fixed underlying type, so it is `int`.
zr::static_assert!(core::mem::size_of::<ThreadStallState>() == 4);
zr::static_assert!(core::mem::align_of::<ThreadStallState>() == 4);

/// Returns `thread`'s stall accounting state.
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for the duration of the
/// call, with no concurrent writes to its `memory_stall_state_`. The state is guarded by
/// `preempt_disabled_token`, so the caller must have preemption (or interrupts, as during a
/// scheduler context switch) disabled on the current CPU.
#[inline]
pub unsafe fn memory_stall_state(thread: *const Thread) -> ThreadStallState {
    // SAFETY: The caller guarantees `thread` is a valid, live `Thread` and that preemption or
    // interrupts are disabled with no concurrent writes to `memory_stall_state_`.
    unsafe { cpp_thread_memory_stall_state(thread) }
}

/// Sets `thread`'s stall accounting state.
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for the duration of the
/// call, with exclusive access to its `memory_stall_state_`. The state is guarded by
/// `preempt_disabled_token`, so the caller must have preemption (or interrupts, as during a
/// scheduler context switch) disabled on the current CPU.
#[inline]
pub unsafe fn set_memory_stall_state(thread: *mut Thread, value: ThreadStallState) {
    // SAFETY: The caller guarantees `thread` is a valid, live `Thread` and that preemption or
    // interrupts are disabled with exclusive access to `memory_stall_state_`.
    unsafe { cpp_thread_set_memory_stall_state(thread, value) }
}

/// Returns a reference to `thread`'s [`SchedulerState`].
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for `'a`, and the
/// caller must hold `thread`'s lock for `'a` with no concurrent mutable access to its
/// `scheduler_state_`.
#[inline]
pub unsafe fn scheduler_state<'a>(thread: *const Thread) -> &'a SchedulerState {
    // SAFETY: The caller guarantees `thread` is a valid, live `Thread` and holds its lock for
    // `'a`, and `cpp_thread_scheduler_state` returns a non-null pointer to its embedded
    // `SchedulerState`.
    unsafe { &*cpp_thread_scheduler_state(thread) }
}

pub const THREAD_SIGNAL_KILL: u32 = 1 << 0;
pub const THREAD_SIGNAL_SUSPEND: u32 = 1 << 1;
pub const THREAD_SIGNAL_POLICY_EXCEPTION: u32 = 1 << 2;
pub const THREAD_SIGNAL_RESTRICTED_KICK: u32 = 1 << 3;
pub const THREAD_SIGNAL_SAMPLE_STACK: u32 = 1 << 4;
pub const THREAD_SIGNAL_CHECK_RSEQ: u32 = 1 << 5;

// LINT.IfChange(FxtRef)
/// Rust representation of the C++ `FxtRef` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FxtRef {
    pub pid: u64,
    pub tid: u64,
}
// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:FxtRef)

/// An opaque type representing the C++ `Thread` class.
///
/// # Safety Invariant
///
/// Any `&Thread` or `&mut Thread` reference must point to a valid, initialized
/// C++ `Thread` instance.
#[repr(C, align(8))]
pub struct Thread {
    // Safety invariant: `_facade` is zero-sized and private so safe code cannot construct a
    // `Thread` by value; any `&Thread` reference points to a live, initialized `Thread` instance,
    // and `OpaqueFacade` marks `Thread` as `!Freeze` and `!Unpin` for FFI mutation.
    _facade: OpaqueFacade,
}

/// Lock class for `Thread::list_lock_`.
///
/// `Thread::list_lock_` is constructed in C++ and only accessed via a `&KSpinlock` facade, so
/// `LockClass::ID` is never used to construct a Rust spinlock.
pub enum ThreadListLockClass {}

impl ksync::LockClass for ThreadListLockClass {
    const ID: *mut core::ffi::c_void = core::ptr::null_mut();
}

impl Thread {
    /// Domain-specific conversion: returns raw pointer for [`Thread`].
    #[inline]
    pub fn as_raw(&self) -> *mut Self {
        core::ptr::from_ref(self).cast_mut()
    }

    /// Asserts that `self`'s canary magic value is valid.
    #[inline]
    pub fn assert_canary(&self) {
        // SAFETY: By `Thread`'s safety invariant, `self` points to a live, initialized `Thread`.
        unsafe {
            cpp_thread_canary_assert(self);
        }
    }

    /// Returns the current pending signals bitmask on this thread.
    #[inline]
    pub fn signals(&self) -> u32 {
        // SAFETY: By `Thread`'s safety invariant, `self` points to a live, initialized `Thread`,
        // and `signals()` performs a relaxed atomic load.
        unsafe { cpp_thread_signals(self) }
    }

    /// Checks whether the kill or suspend signal has been raised. If kill has been raised, returns
    /// `Err(Status::INTERNAL_INTR_KILLED)`. If suspend has been raised, returns
    /// `Err(Status::INTERNAL_INTR_RETRY)`. Otherwise, returns `Ok(())`.
    #[inline]
    pub fn check_kill_or_suspend_signal(&self) -> Result<(), Status> {
        // SAFETY: By `Thread`'s safety invariant, `self` points to a live, initialized `Thread`.
        Status::ok(unsafe { cpp_thread_check_kill_or_suspend_signal(self) })
    }

    /// Returns a reference to the global thread list spinlock (`Thread::get_list_lock()`).
    #[inline]
    pub fn get_list_lock() -> &'static ksync::KSpinlock<ThreadListLockClass> {
        // SAFETY: `cpp_thread_get_list_lock` returns the static `Thread::list_lock_` instance,
        // which is layout-compatible with `ksync::KSpinlock`.
        unsafe { &*cpp_thread_get_list_lock() }
    }

    /// Finds a thread whose `tid()` matches `tid` and dumps it (`Thread::DumpTid`).
    #[inline]
    pub fn dump_tid(tid: zx_types::zx_koid_t, full: bool) {
        // SAFETY: Safe FFI call into `Thread::DumpTid`.
        unsafe { cpp_thread_dump_tid(tid, full) };
    }

    /// Dumps the thread with `tid` without acquiring locks during a panic
    /// (`Thread::DumpTidDuringPanic`).
    ///
    /// # Safety
    ///
    /// Because this method bypasses `Thread::list_lock_` and per-thread locks, it must only be
    /// called during a kernel panic when normal locking is unsafe or unavailable.
    #[inline]
    pub unsafe fn dump_tid_during_panic(tid: zx_types::zx_koid_t, full: bool) {
        // SAFETY: Caller guarantees the system is in a panic context.
        unsafe { cpp_thread_dump_tid_during_panic(tid, full) };
    }

    /// Dumps all threads matching `match_owner` (or all threads if `None`) (`Thread::DumpAll`).
    #[inline]
    pub fn dump_all(match_owner: Option<&CStr>, full: bool) {
        let ptr = match_owner.map_or(core::ptr::null(), CStr::as_ptr);
        // SAFETY: `ptr` is either null or a valid null-terminated C string.
        unsafe { cpp_thread_dump_all(ptr, full) };
    }

    /// Dumps all threads matching `match_owner` (or all threads if `None`) during a panic
    /// (`Thread::DumpAllDuringPanic`).
    ///
    /// # Safety
    ///
    /// Because this method bypasses `Thread::list_lock_` and per-thread locks, it must only be
    /// called during a kernel panic when normal locking is unsafe or unavailable.
    #[inline]
    pub unsafe fn dump_all_during_panic(match_owner: Option<&CStr>, full: bool) {
        let ptr = match_owner.map_or(core::ptr::null(), CStr::as_ptr);
        // SAFETY: `ptr` is null or a valid C string and the caller guarantees the system is in a
        // panic context.
        unsafe { cpp_thread_dump_all_during_panic(ptr, full) };
    }
}

/// Finds a thread based on the thread ID (`thread_id_to_thread_slow`).
///
/// # Safety
///
/// The caller must hold [`Thread::get_list_lock`], and the returned pointer is only guaranteed to
/// remain valid while that lock is held.
#[inline]
pub unsafe fn thread_id_to_thread_slow(tid: zx_types::zx_koid_t) -> Option<ThreadPtr> {
    // SAFETY: Caller holds `Thread::get_list_lock()`, and `ThreadPtr::from_raw` wraps the raw
    // pointer without dereferencing it.
    unsafe { ThreadPtr::from_raw(cpp_thread_id_to_thread_slow(tid)) }
}

/// Reschedules the current CPU (`Thread::Current::Reschedule()`).
#[inline]
pub fn reschedule() {
    // SAFETY: Safe FFI call into `Thread::Current::Reschedule`.
    unsafe { cpp_thread_current_reschedule() };
}

/// Accessors and locked operations for a [`Thread`] guarded by its chain lock.
impl<'p> ChainLockProof<'p, Thread> {
    /// Returns the scheduling state of this thread.
    #[inline]
    pub fn state(&self) -> crate::kernel::scheduler_state::ThreadStateKind {
        // SAFETY: `self` proves `self.owner().lock()` is held.
        unsafe { cpp_thread_state(self.owner()) }
    }

    /// Sets the thread's state to `ThreadStateKind::Blocked`.
    #[inline]
    pub fn set_blocked(&mut self) {
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively.
        unsafe {
            cpp_thread_set_blocked(self.owner().as_raw());
        }
    }

    /// Sets the thread's state to `ThreadStateKind::BlockedReadLock`.
    #[inline]
    pub fn set_blocked_read_lock(&mut self) {
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively.
        unsafe {
            cpp_thread_set_blocked_read_lock(self.owner().as_raw());
        }
    }

    /// Returns a reference to this thread's
    /// [`WaitQueueCollectionThreadState`](super::wait_queue::WaitQueueCollectionThreadState).
    #[inline]
    pub fn wait_queue_state(
        &self,
    ) -> &'p crate::kernel::wait_queue::WaitQueueCollectionThreadState {
        // SAFETY: `self` proves `self.owner().lock()` is held, and `cpp_thread_wait_queue_state`
        // returns a valid pointer to `self.owner()`'s embedded `WaitQueueCollection::ThreadState`.
        unsafe { &*cpp_thread_wait_queue_state(self.owner()) }
    }

    /// Returns a shared reference to this thread's [`SchedulerState`].
    #[inline]
    pub fn scheduler_state(&self) -> &SchedulerState {
        // SAFETY: `self` proves `self.owner().lock()` is held.
        unsafe { &*cpp_thread_scheduler_state(self.owner()) }
    }

    /// Returns a mutable reference to this thread's [`SchedulerState`].
    #[inline]
    pub fn scheduler_state_mut(&mut self) -> &mut SchedulerState {
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively, and the unique borrow
        // of `self` ensures no other reference to `scheduler_state_` is live.
        unsafe {
            &mut *cpp_thread_get_scheduler_state(self.owner().as_raw()).cast::<SchedulerState>()
        }
    }
}

// SAFETY: `cpp_thread_get_lock` returns the thread's embedded `ChainLock`, which guards its
// `KCell<_, ThreadLockClass>` fields and returns the same lock instance on every call.
unsafe impl ChainLocked for Thread {
    type Class = ThreadLockClass;

    fn lock(&self) -> &ClassedChainLock<ThreadLockClass> {
        // SAFETY: Validity: by `Thread`'s safety invariant, `self` points to a live, initialized
        // `Thread` for the lifetime of `&self`, and `cpp_thread_get_lock` returns a non-null
        // pointer to its embedded `ChainLock`. Class binding (`from_chain_lock`): this lock guards
        // the thread's `ThreadLockClass` fields.
        unsafe { ClassedChainLock::from_chain_lock(&*cpp_thread_get_lock(self.as_raw())) }
    }
}

/// Enters restricted mode on the current thread using the given vector table pointer and context.
///
/// # Errors
/// - `Status::INVALID_ARGS`: `vector_table_ptr` is not a valid user-accessible address.
/// - `Status::BAD_STATE`: No `RestrictedState` is bound to the current thread, or state is invalid.
/// - `Status::INTERRUPTED_RETRY`: Current thread has pending signals that must be processed.
pub fn restricted_enter(vector_table_ptr: usize, context: usize) -> Result<(), Status> {
    crate::kernel::restricted::restricted_enter(vector_table_ptr, context)
}

/// Type-safe wrapper around a raw pointer to a Zircon kernel Thread.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadPtr(NonNull<Thread>);

// SAFETY: A ThreadPtr is just a pointer to a kernel thread, which can be safely passed
// between threads to perform join or kill operations.
unsafe impl Send for ThreadPtr {}
unsafe impl Sync for ThreadPtr {}

impl From<&Thread> for ThreadPtr {
    #[inline]
    fn from(thread: &Thread) -> Self {
        Self(NonNull::from(thread))
    }
}

impl ThreadPtr {
    /// Creates a `ThreadPtr` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a live kernel thread.
    pub const unsafe fn from_raw(ptr: *mut Thread) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    pub const fn as_raw(self) -> *mut Thread {
        self.0.as_ptr()
    }

    /// Returns the raw const pointer.
    pub const fn as_ptr(self) -> *const Thread {
        self.0.as_ptr()
    }

    /// Returns a shared reference to the underlying [`Thread`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the underlying [`Thread`] is valid and remains alive for `'a`.
    #[inline]
    pub const unsafe fn as_ref<'a>(self) -> &'a Thread {
        // SAFETY: The caller guarantees the underlying `Thread` is valid and remains alive for
        // `'a`.
        unsafe { self.0.as_ref() }
    }

    /// Makes a suspended thread executable.
    ///
    /// This function is called to start a thread which has just been created with [`create`] or
    /// which has been suspended with [`ThreadPtr::suspend`]. It cannot fail.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread has not been joined or destroyed.
    pub unsafe fn resume(self) {
        unsafe { cpp_thread_resume(self.as_raw()) }
    }

    /// Waits `deadline` time for a thread to complete execution then releases its memory.
    ///
    /// Returns the thread's return code on success.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread has not been joined yet.
    pub unsafe fn join(self, deadline: InstantMono) -> Result<i32, Status> {
        let mut retcode = 0;
        let status = unsafe { cpp_thread_join(self.as_raw(), &mut retcode, deadline.0) };
        Status::ok(status).map(|_| retcode)
    }

    /// Delivers a kill signal to a thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread is still valid.
    pub unsafe fn kill(self) {
        unsafe { cpp_thread_kill(self.as_raw()) }
    }

    /// Suspends an initialized/ready/running thread.
    ///
    /// Returns `Ok(())` on success, `Err(Status::BAD_STATE)` if the thread is dead.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread is still valid.
    pub unsafe fn suspend(self) -> Result<(), Status> {
        let status = unsafe { cpp_thread_suspend(self.as_raw()) };
        Status::ok(status)
    }

    /// Kicks the thread out of restricted mode into normal mode if it is currently in restricted
    /// mode, or remembers the kick state for the next attempt to enter restricted mode.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread is still valid.
    pub unsafe fn restricted_kick(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees self.as_raw() points to a valid, live kernel thread.
        let status = unsafe { cpp_thread_restricted_kick(self.as_raw()) };
        Status::ok(status)
    }

    /// Starts memory stall accounting for the thread by moving it out of the
    /// `IgnoredKernelOnly` stall state and into `Inactive`. Preemption is disabled for the
    /// duration of the update inside the shim.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread is still valid and is currently in the
    /// `IgnoredKernelOnly` stall state, which the shim asserts.
    pub unsafe fn set_memory_stall_state_inactive(self) {
        // SAFETY: The caller guarantees self.as_raw() points to a valid, live kernel thread that is
        // currently in the IgnoredKernelOnly stall state.
        unsafe { cpp_thread_set_memory_stall_state_inactive(self.as_raw()) }
    }

    /// Checks if the thread is currently blocked.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed or joined.
    pub unsafe fn is_blocked(self) -> bool {
        unsafe { cpp_thread_is_blocked(self.as_raw()) }
    }

    /// Returns a `ThreadPtr` representing the currently executing thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called after multi-threading has been
    /// initialized (i.e. after LK_INIT_LEVEL_THREADING).
    pub unsafe fn current() -> Self {
        unsafe { Self::from_raw(cpp_thread_current_get()) }.unwrap()
    }

    /// Returns the pid/tid of the thread as a tracing thread reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn fxt_ref(self) -> FxtRef {
        unsafe { cpp_thread_fxt_ref(self.as_raw()) }
    }

    /// Sets the base profile of the thread (`Thread::SetBaseProfile`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The thread pointer is still valid and the underlying thread has not been destroyed.
    /// - This function is called from normal thread context with no spinlocks, `self`'s lock, or
    ///   active `ChainLockTransaction` held on the current CPU.
    pub unsafe fn set_base_profile(self, profile: &SchedulerStateBaseProfile) {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread` and that the calling
        // context is a normal thread context with no active `ChainLockTransaction` or spinlocks
        // held.
        unsafe { cpp_thread_set_base_profile(self.as_raw(), profile) }
    }

    /// Returns the current CPU affinity mask of the thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn get_cpu_affinity(self) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_get_cpu_affinity(self.as_ptr()) }
    }

    /// Sets the CPU affinity mask of the thread and returns its previous affinity mask.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn set_cpu_affinity(self, affinity: cpu_mask_t) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_set_cpu_affinity(self.as_raw(), affinity) }
    }

    /// Returns a reference to this thread's [`ChainLock`].
    ///
    /// The C++ `ChainLock` returned by `Thread::get_lock()` is layout-compatible with the Rust
    /// [`ChainLock`] (the same pair of atomics, checked by `static_assert`s on both sides) and
    /// implements the same locking protocol, so it can be acquired and released from either
    /// language within the same `ChainLockTransaction`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` which remains alive for as
    /// long as the returned reference is used.
    #[inline]
    pub unsafe fn get_lock(&self) -> &ChainLock {
        // SAFETY: `self` points to a valid `Thread` and its embedded `ChainLock` remains valid for
        // as long as the caller guarantees the thread stays alive. The C++ `ChainLock` has the
        // same size, alignment, and atomic fields as the Rust `ChainLock`, and is only accessed
        // through atomics.
        unsafe { &*cpp_thread_get_lock(self.as_raw()) }
    }

    /// Returns the `blocked_status_` from this thread's `wait_queue_state()`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` and that its chain lock is
    /// held by the transaction identified by `token`.
    #[inline]
    pub unsafe fn wait_queue_state_blocked_status_locked(
        self,
        token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // SAFETY: `self` points to a valid `Thread` which outlives this call.
        unsafe { self.get_lock() }.assert_held(token);
        // SAFETY: `self` points to a valid `Thread` whose chain lock is held by the caller.
        Status::ok(unsafe { cpp_thread_wait_queue_state_blocked_status_locked(self.as_ptr()) })
    }

    /// Returns the associated `ThreadDispatcher` (`user_thread()`), or null if this is a
    /// kernel-only thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn user_thread_raw(self) -> *mut crate::object::ThreadDispatcher {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { cpp_thread_user_thread(self.as_raw()) }
    }

    /// Returns the koid of the associated `ThreadDispatcher` for user threads or an independent
    /// koid for kernel threads (`tid()`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn tid(self) -> zx_types::zx_koid_t {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { cpp_thread_tid(self.as_ptr()) }
    }

    /// Returns the last flow id allocated by [`ThreadPtr::take_next_lock_flow_id`] for this thread
    /// (`lock_flow_id()`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn lock_flow_id(self) -> u64 {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { cpp_thread_lock_flow_id(self.as_ptr()) }
    }

    /// Returns a unique flow id for lock contention tracing (`TakeNextLockFlowId()`).
    ///
    /// The same value is returned by [`ThreadPtr::lock_flow_id`] until another id is allocated
    /// for this thread by calling this method again.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn take_next_lock_flow_id(self) -> u64 {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { cpp_thread_take_next_lock_flow_id(self.as_raw()) }
    }

    /// Generate a backtrace for the calling thread.
    ///
    /// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot be obtained, it
    /// will be left empty.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` that is blocked, sleeping, or
    /// suspended, and that the caller does not hold `self`'s thread lock or an active chain lock
    /// transaction.
    #[inline]
    pub unsafe fn get_backtrace(self, out_bt: &mut Backtrace) {
        // SAFETY: `self` points to a valid `Thread`, `out_bt` is a valid `Backtrace`, and the
        // caller upholds the thread state and locking preconditions.
        unsafe { cpp_thread_get_backtrace(self.as_raw(), out_bt) }
    }

    /// Dump the information of a single thread. If `full` is true then a multi-line verbose dump of
    /// much information is performed, otherwise a single short summary line is dumped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` and that the caller does not
    /// hold `self`'s thread lock or an active chain lock transaction.
    #[inline]
    pub unsafe fn dump(self, full: bool) {
        // SAFETY: `self` points to a valid `Thread` and the caller upholds the locking
        // preconditions.
        unsafe { cpp_thread_dump(self.as_ptr(), full) }
    }
}

/// A thread's lock, held with interrupts disabled until the guard is dropped: the
/// `SingleChainLockGuard{IrqSaveOption, thread->get_lock(), tag}` that a C++ scope would hold.
///
/// Built in place as a pinned local, for example
/// `ksync::lock!(let guard = ThreadLockGuard::lock(thread, clt_tag!("label")))`. The chain lock
/// transaction inside records its own address, so the guard never moves.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ThreadLockGuard {
    raw: Opaque<lock_guard_bindings::ThreadLockGuard>,
    thread: NonNull<Thread>,
    #[pin]
    phantom: PhantomPinned,
}

impl ThreadLockGuard {
    /// Returns an initializer that takes `thread`'s lock with interrupts disabled, tagging the
    /// chain lock transaction with `tag`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `thread` is non-null and points to a valid C++ `Thread` instance for as long as the guard
    ///   lives.
    /// - The calling CPU is not already in a chain lock transaction, and none starts while the
    ///   guard lives.
    /// - The guard is a stack-pinned local (`ksync::lock!` or `pin_init::stack_pin_init!`) that is
    ///   dropped on this CPU, before anything that can block.
    pub unsafe fn lock(
        thread: *mut Thread,
        tag: CltTag,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        debug_assert!(!thread.is_null());
        let label = tag.label.unwrap_or_else(|| crate::ktrace_rs::resolve_string!("<unnamed>"));
        // SAFETY:
        // - `pin_init_from_closure`: the closure initializes every field of `slot` in place and
        //   never moves it (`thread` via `ptr::write`, `raw` via `cpp_thread_lock_guard_init`, and
        //   `phantom` is zero-sized), and returns `Ok(())`.
        // - `NonNull::new_unchecked(thread)`: `thread` is non-null by the caller's `# Safety`
        //   precondition.
        // - `(&raw mut (*slot).thread).write(...)`: `pin_init_from_closure` guarantees `slot` is
        //   non-null, properly aligned, and valid for writes to `Self`.
        // - `cpp_thread_lock_guard_init`: `(&raw mut (*slot).raw).cast()` points to valid,
        //   properly aligned storage for the C++ guard, `label` is a valid `'static`
        //   `InternedString` reference, and the caller upholds `lock`'s remaining `# Safety`
        //   preconditions for `thread` and the active transaction/CPU state.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                (&raw mut (*slot).thread).write(NonNull::new_unchecked(thread));
                cpp_thread_lock_guard_init(
                    (&raw mut (*slot).raw).cast(),
                    thread,
                    label,
                    tag.line_number,
                );
                Ok(())
            })
        }
    }

    /// Returns the raw pointer to the locked thread.
    pub fn thread(&self) -> *mut Thread {
        self.thread.as_ptr()
    }

    /// Returns a pointer to the architecture-specific state (`arch_thread`) of the locked thread.
    pub fn arch(&self) -> *mut c_void {
        // SAFETY: `lock`'s caller guarantees the thread is a live C++ `Thread`.
        unsafe { get_arch(self.thread()) }
    }

    /// Checks whether user state is saved for the locked thread, like C++
    /// `Thread::IsUserStateSavedLocked()`.
    pub fn is_user_state_saved_locked(&self) -> bool {
        // SAFETY: the thread is a live C++ `Thread` whose lock this guard holds.
        unsafe { cpp_thread_is_user_state_saved_locked(self.thread()) }
    }

    /// Returns a reference to the [`SchedulerState`] of the locked thread.
    pub fn scheduler_state(&self) -> &SchedulerState {
        // SAFETY: `lock`'s caller guarantees the thread is a live C++ `Thread`, and this guard
        // holds its lock.
        unsafe { scheduler_state(self.thread()) }
    }

    /// Mints a [`ChainLockProof`] proving that the locked thread's chain lock is held.
    pub fn proof(self: Pin<&mut Self>) -> ChainLockProof<'_, Thread> {
        // `Pin<&mut Self>` -> `&Self` for the lifetime of the unique borrow: the returned proof
        // borrows `self` mutably even though we only read through it.
        let this: &Self = self.into_ref().get_ref();
        // SAFETY: `this.thread` points to a live `Thread` for the lifetime of this guard (`lock`'s
        // contract), and `raw` acquired (never adopted) its lock and holds it until drop. The
        // proof is bounded by the unique borrow of `self`.
        unsafe { ChainLockProof::from_acquiring_guard(this.thread.as_ref()) }
    }
}

#[pinned_drop]
impl PinnedDrop for ThreadLockGuard {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `raw` holds the C++ guard that `lock` built, and this is its only destruction.
        unsafe { cpp_thread_lock_guard_destroy(self.raw.get()) };
    }
}

/// Creates a thread with `name` that will execute `entry` at [`DEFAULT_PRIORITY`]. `arg` will be
/// passed to `entry` when executed, and the return value of `entry` will be passed to `Exit()`.
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or `Detach()`, one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread.
pub unsafe fn create(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { cpp_thread_create_default(name, entry, arg) };
    unsafe { ThreadPtr::from_raw(thread) }.ok_or(Status::NO_MEMORY)
}

/// Kernel thread priority levels matching Zircon C++ definitions in `<kernel/thread.h>`.
pub const LOW_PRIORITY: i32 = 8;
pub const DEFAULT_PRIORITY: i32 = 16;
pub const HIGH_PRIORITY: i32 = 24;

/// Creates a thread with `name` that will execute `entry` at `priority`. `arg` will be passed to
/// `entry` when executed, and the return value of `entry` will be passed to `Exit()`.
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or `Detach()`, one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// Thread priority is an integer from 0 (lowest) to 31 (highest). Standard priorities include
/// [`HIGH_PRIORITY`], [`DEFAULT_PRIORITY`], and [`LOW_PRIORITY`].
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread.
pub unsafe fn create_with_priority(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
    priority: i32,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { cpp_thread_create_with_priority(name, entry, arg, priority) };
    unsafe { ThreadPtr::from_raw(thread) }.ok_or(Status::NO_MEMORY)
}

/// Creates a thread with `name` that will execute `entry` with the given base `profile`. `arg`
/// will be passed to `entry` when executed, and the return value of `entry` will be passed to
/// `Exit()`.
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or `Detach()`, one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread.
pub unsafe fn create_with_profile(
    name: &[u8],
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
    profile: &SchedulerStateBaseProfile,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe {
        cpp_thread_create_with_profile(
            name.as_ptr() as *const c_char,
            name.len(),
            entry,
            arg,
            profile,
        )
    };
    unsafe { ThreadPtr::from_raw(thread) }.ok_or(Status::NO_MEMORY)
}

/// Spawns a new kernel thread with default priority and resumes it.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread,
/// and that the thread is joined before any borrowed data in `arg` is destroyed.
pub unsafe fn spawn(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { create(name, entry, arg)? };
    unsafe { thread.resume() };
    Ok(thread)
}

/// Yields the CPU to another thread.
///
/// This function places the current thread at the end of the run queue and yields the CPU to
/// another waiting thread (if any).
///
/// This function will return at some later time. Possibly immediately if no other threads are
/// waiting to execute.
pub fn r#yield() {
    unsafe { cpp_thread_current_yield() }
}

/// Terminates the current thread with the given return code. Does not return.
///
/// Nothing on the current stack is dropped, since control never comes back. That is a leak
/// rather than unsoundness, so this is a safe function, matching `r#yield` above.
pub fn current_exit(retcode: i32) -> ! {
    // SAFETY: `Thread::Current::Exit` unwinds the kernel stack of the calling thread and never
    // returns, which is what the `!` return type on the declaration states.
    unsafe { cpp_thread_current_exit(retcode) }
}

/// Marks the current thread for termination. The thread will actually terminate when the kernel
/// stack unwinds.
pub fn current_kill() {
    // SAFETY: `Thread::Current::Kill` only raises a signal on the calling thread.
    unsafe { cpp_thread_current_kill() }
}

/// Increments the preempt disable counter for the current thread.
///
/// While preempt disable is non-zero, preemption of the thread is disabled, including preemption
/// from interrupt handlers. During this time, any call to `Reschedule()` will only record that a
/// reschedule is pending, and won't do a context switch.
///
/// Note that this does not disallow blocking operations (e.g. `mutex.Acquire()`). Disabling
/// preemption does not prevent switching away from the current thread if it blocks.
///
/// A call to [`preempt_disable`] must be matched by a later call to [`preempt_reenable`] to
/// decrement the preempt disable counter.
#[inline]
pub fn preempt_disable() {
    // SAFETY: Calling this FFI function safely increments the preemption disable count for the
    // current thread.
    unsafe { cpp_thread_preempt_disable() }
}

/// Decrements the preempt disable counter and flushes any pending local preemption operation.
///
/// Callers must ensure that they are calling from a context where blocking is allowed, as the call
/// may result in the immediate preemption of the calling thread.
#[inline]
pub fn preempt_reenable() {
    // SAFETY: Calling this FFI function safely decrements the preemption disable count for the
    // current thread.
    unsafe { cpp_thread_preempt_reenable() }
}

/// Increments the eager resched disable counter for the current thread.
///
/// When eager resched disable is non-zero, issuing local and remote preemptions is disabled,
/// including from interrupt handlers. During this time, any call to `Reschedule()` or other
/// scheduler entry points that imply a reschedule will only record the pending reschedule for the
/// affected CPU, but will not perform reschedule IPIs or a local context switch.
///
/// As with [`preempt_disable`], blocking operations are still allowed while eager resched disable
/// is non-zero.
///
/// A call to [`eager_resched_disable`] must be matched by a later call to
/// [`eager_resched_reenable`] to decrement the eager resched disable counter.
#[inline]
pub fn eager_resched_disable() {
    // SAFETY: Calling this FFI function safely increments the eager resched disable count for the
    // current thread.
    unsafe { cpp_thread_eager_resched_disable() }
}

/// Decrements the eager resched disable counter and flushes pending local and/or remote
/// preemptions if enabled, respectively.
#[inline]
pub fn eager_resched_reenable() {
    // SAFETY: Calling this FFI function safely decrements the eager resched disable count for the
    // current thread.
    unsafe { cpp_thread_eager_resched_reenable() }
}

/// Returns the current thread's preemption disable count.
#[inline]
pub fn preempt_disable_count() -> u32 {
    // SAFETY: Calling this FFI function safely reads the current thread's preemption disable count.
    unsafe { cpp_thread_preempt_disable_count() }
}

/// Returns the current thread's eager resched disable count.
#[inline]
pub fn eager_resched_disable_count() -> u32 {
    // SAFETY: Calling this FFI function safely reads the current thread's eager resched disable
    // count.
    unsafe { cpp_thread_eager_resched_disable_count() }
}

/// Marks a pending preemption for the current CPU on the current thread's preemption state.
///
/// This is similar to `Reschedule()`, except that it may only be used inside an interrupt handler
/// while interrupts and preemption are disabled, between [`preempt_disable`] and
/// [`preempt_reenable`]. Unlike `Reschedule()`, it does not need to be called with the thread's
/// lock held.
#[inline]
pub fn preempt_set_pending() {
    // SAFETY: Calling this FFI function safely marks a pending preemption on the current thread's
    // preemption state.
    unsafe { cpp_thread_preempt_set_pending() }
}

/// Returns `true` if preemption is currently enabled on the current thread.
#[inline]
pub fn preempt_is_enabled() -> bool {
    // SAFETY: Calling this FFI function safely queries the current thread's preemption state.
    unsafe { cpp_thread_preempt_is_enabled() }
}

/// Zero-sized capability token proving that preemption is disabled on the current thread.
///
/// Corresponds to C++ `preempt_disabled_token`. This token is `!Send` and `!Sync` because
/// preemption state is CPU- and thread-local.
#[derive(Debug)]
pub struct PreemptDisabledToken {
    // Safety invariant: Preemption is disabled on the current thread (`!preempt_is_enabled()`) for
    // the lifetime of this token, and it is `!Send` and `!Sync`.
    _private: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<PreemptDisabledToken>() == 0);
zr::static_assert!(align_of::<PreemptDisabledToken>() == 1);

impl PreemptDisabledToken {
    /// Creates a new capability token proving that preemption is disabled on the current thread.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that preemption is currently disabled on the current thread
    /// (`!preempt_is_enabled()`) and remains disabled for the lifetime of the returned token.
    #[inline]
    pub const unsafe fn new() -> Self {
        Self { _private: PhantomData }
    }

    /// Asserts in debug builds that preemption is currently disabled on the current thread.
    #[inline]
    pub fn assert_held(&self) {
        debug_assert!(!preempt_is_enabled());
    }
}

/// Sets a timeslice extension if one is not already set.
///
/// This function should only be called in normal thread context.
///
/// Returns `false` if a timeslice extension was already present or if the supplied duration is
/// `<= 0`.
///
/// Note: It is OK to call this from a context where preemption is (hard) disabled. If preemption
/// is requested while the preempt disable count is non-zero and a timeslice extension is in place,
/// the extension will be activated, but preemption will not occur until the count has dropped to
/// zero and the extension has expired or has been cleared.
pub fn preempt_set_timeslice_extension(duration: DurationMono) -> bool {
    // SAFETY: Calling this FFI function safely sets the timeslice extension on the current thread's
    // preemption state.
    unsafe { cpp_thread_preempt_set_timeslice_extension(duration) }
}

/// Unconditionally clears any timeslice extension.
///
/// This function must be called in normal thread context because it may trigger local preemption.
pub fn preempt_clear_timeslice_extension() {
    // SAFETY: Calling this FFI function safely clears the timeslice extension on the current
    // thread's preemption state.
    unsafe { cpp_thread_preempt_clear_timeslice_extension() }
}

pub use super::auto_preempt_disabler::{
    AnnotatedAutoEagerReschedDisabler, AnnotatedAutoPreemptDisabler, AutoEagerReschedDisabler,
    AutoExpiringPreemptDisabler, AutoPreemptDisabler,
};

/// RAII helper to enforce that a block of code does not allocate memory.
///
/// See `Thread::Current::memory_allocation_state()`.
pub struct ScopedMemoryAllocationDisabled;

impl ScopedMemoryAllocationDisabled {
    pub fn new() -> Self {
        // SAFETY: Disables memory allocations on the current thread.
        unsafe { cpp_thread_current_memory_allocation_state_disable() };
        Self
    }
}

impl Drop for ScopedMemoryAllocationDisabled {
    fn drop(&mut self) {
        // SAFETY: Re-enables memory allocations on the current thread when the guard drops.
        unsafe { cpp_thread_current_memory_allocation_state_enable() };
    }
}

pub use crate::kernel::wait_queue::Interruptible;

/// Puts the current thread to sleep until the specified `deadline` has occurred.
///
/// Note that this function could continue to sleep after the specified deadline if other threads
/// are running. When the deadline occurs, this thread will be placed at the head of the run queue.
///
/// If `interruptible` is [`Interruptible::YES`], this routine may return early with
/// `Status::INTERNAL_INTR_KILLED` if the thread is signaled for kill, or
/// `Status::INTERNAL_INTR_RETRY` if signaled for suspend.
pub fn sleep_etc(
    deadline: &crate::kernel::deadline::Deadline,
    interruptible: Interruptible,
    now: zx_instant_mono_t,
) -> Result<(), Status> {
    // SAFETY: `deadline` points to a valid `Deadline`.
    let status = unsafe { cpp_thread_current_sleep_etc(deadline as *const _, interruptible, now) };
    Status::ok(status)
}

/// Non-interruptible version of [`sleep_etc`].
pub fn sleep(deadline: InstantMono) -> Result<(), Status> {
    // SAFETY: FFI function has no special safety requirements in thread context.
    let status = unsafe { cpp_thread_current_sleep(deadline) };
    Status::ok(status)
}

/// Non-interruptible relative delay version of [`sleep`].
pub fn sleep_relative(duration: DurationMono) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_sleep_relative is safe to call at any time in thread context.
    let status = unsafe { cpp_thread_current_sleep_relative(duration) };
    Status::ok(status)
}

/// Interruptible version of [`sleep`].
pub fn sleep_interruptible(deadline: InstantMono) -> Result<(), Status> {
    // SAFETY: FFI function has no special safety requirements in thread context.
    let status = unsafe { cpp_thread_current_sleep_interruptible(deadline) };
    Status::ok(status)
}

/// Handles a soft fault on the address space containing `va` for the current thread.
///
/// If there is no address space that contains `va`, or the thread does not have access to it,
/// `Status::NOT_FOUND` is returned.
///
/// Calling this method on a pure kernel thread (i.e. one without an associated `ThreadDispatcher`)
/// is a programming error. May block on page requests and must be called without locks held.
pub fn soft_fault(va: usize, flags: u32) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_soft_fault is safe to call from thread context.
    let status = unsafe { cpp_thread_current_soft_fault(va, flags) };
    Status::ok(status)
}

/// Returns the raw pointer to the current thread.
pub fn current_get() -> *mut Thread {
    unsafe { cpp_thread_current_get() }
}

/// Preempts the current thread from an interrupt.
///
/// This function places the current thread at the head of the run queue and then yields the CPU to
/// another thread.
pub fn preempt() {
    unsafe { cpp_thread_preempt() }
}

/// Logs the relevant stack memory addresses of the current thread at the `CRITICAL` debug level.
///
/// This is useful during a thread dump.
pub fn dump_current_stack() {
    unsafe { cpp_thread_dump_current_stack() }
}

/// Processes any pending thread signals on the current thread using the given `frame`.
///
/// This function may never return if the thread has a pending kill signal.
///
/// Interrupt state: This function modifies interrupt state. It is critical that this function be
/// called with interrupts disabled to eliminate a "lost wakeup" race condition. While interrupts
/// must be disabled prior to calling this function, it may re-enable them during the processing of
/// certain signals. This function guarantees that if it does return, it will do so with interrupts
/// disabled.
///
/// # Safety
///
/// Caller must ensure `frame` points to a valid architectural `iframe_t` and that interrupts are
/// disabled.
pub unsafe fn process_pending_signals(frame: *mut c_void) {
    // SAFETY: Forwarded to C++ Thread::Current::ProcessPendingSignals with caller-verified frame.
    unsafe { cpp_thread_process_pending_signals(frame) }
}

/// Returns a pointer to the architecture-specific state (`arch_thread`) of `thread`.
///
/// The returned pointer is derived by offset only, so this is safe to call before
/// `thread` is fully constructed; dereferencing the result is the caller's problem.
///
/// # Safety
/// Caller must ensure `thread` points to a C++ `Thread` instance.
pub unsafe fn get_arch(thread: *mut Thread) -> *mut c_void {
    // SAFETY: Forwarded to C++ Thread::arch() with caller-verified pointer.
    unsafe { cpp_thread_get_arch(thread) }
}

/// Returns the top of the stack for `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn get_stack_top(thread: *mut Thread) -> usize {
    // SAFETY: Forwarded to C++ Thread::stack().top() with caller-verified pointer.
    unsafe { cpp_thread_get_stack_top(thread) }
}

/// Returns the shadow call stack base for `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn get_shadow_call_base(thread: *mut Thread) -> usize {
    // SAFETY: Forwarded to C++ Thread::stack().shadow_call_base() with caller-verified pointer.
    unsafe { cpp_thread_get_shadow_call_base(thread) }
}

/// Returns `true` if `thread`'s user state has been saved.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance and that the caller holds
/// `thread`'s lock.
pub unsafe fn is_user_state_saved_locked(thread: *mut Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::IsUserStateSavedLocked() with caller-verified pointer.
    unsafe { cpp_thread_is_user_state_saved_locked(thread) }
}

/// Checks whether `thread` is currently running.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn is_running(thread: *const Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::state() with caller-verified pointer.
    unsafe { cpp_thread_is_running(thread) }
}

/// Returns the name of `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn name(thread: *const Thread) -> *const c_char {
    // SAFETY: Forwarded to C++ Thread::name() with caller-verified pointer.
    unsafe { cpp_thread_name(thread) }
}

/// Checks whether `thread` is executing in restricted mode.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn in_restricted(thread: *mut Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::in_restricted() with caller-verified pointer.
    unsafe { cpp_thread_in_restricted(thread) }
}

/// Checks whether `thread` is a user thread, i.e. has an associated `ThreadDispatcher`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn is_user_thread(thread: *const Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::user_thread() with caller-verified pointer.
    unsafe { cpp_thread_is_user_thread(thread) }
}

/// Returns the currently active address space of `thread`, which is the address space currently
/// hosting page tables for the thread.
///
/// The active address space should be used only when context switching. It should not be used for
/// resolving faults, as it may be a unified aspace that does not keep track of its own mappings.
///
/// Kernel-only thread -- This will return null, unless a caller has explicitly set the aspace
/// using `switch_aspace`, which is done by a few kernel unittests.
///
/// User thread -- If the thread is in Restricted Mode, this will return the restricted aspace.
/// Otherwise, it will return the process's normal aspace.
///
/// Note, the normal aspace is, by definition, the aspace that's active when a thread is in Normal
/// Mode. All threads not in Restricted Mode are said to be in Normal Mode. See
/// `ProcessDispatcher::normal_aspace()` for more information.
///
/// Only the thread itself, or the context switch path for the two threads it is switching
/// between, may read a thread's active aspace without holding a reference to it; see the rules for
/// `Thread::aspace_` in `kernel/thread.h`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance that is either the current
/// thread or one of the two threads of a context switch in progress.
pub unsafe fn active_aspace(thread: *mut Thread) -> *mut VmAspace {
    // SAFETY: Forwarded to C++ Thread::active_aspace() with caller-verified pointer.
    unsafe { cpp_thread_active_aspace(thread) }
}

/// Returns the current thread's restricted mode state pointer.
pub fn current_restricted_state() -> *mut RestrictedState {
    // SAFETY: Foreign function wrapper for Thread::Current::restricted_state().
    unsafe { cpp_thread_current_restricted_state() }
}

/// Returns the currently active address space, which is the address space currently hosting page
/// tables for the current thread.
///
/// The active address space should be used only when context switching. It should not be used for
/// resolving faults, as it may be a unified aspace that does not keep track of its own mappings.
///
/// Kernel-only thread -- This will return `None`, unless a caller has explicitly set `aspace_`
/// using `switch_aspace`, which is done by a few kernel unittests.
///
/// User thread -- If the thread is in Restricted Mode, this will return the restricted aspace.
/// Otherwise, it will return the process's normal aspace.
///
/// Note, the normal aspace is, by definition, the aspace that's active when a thread is in Normal
/// Mode. All threads not in Restricted Mode are said to be in Normal Mode. See
/// `ProcessDispatcher::normal_aspace()` for more information.
///
/// # Safety
///
/// The caller must ensure that the returned address space reference remains valid for lifetime
/// `'a`.
pub unsafe fn current_active_aspace<'a>() -> Option<&'a VmAspace> {
    // SAFETY: `cpp_thread_current_active_aspace` returns the active `VmAspace*` pointer, which
    // is guaranteed by the caller to remain valid for `'a`.
    unsafe { cpp_thread_current_active_aspace().as_ref() }
}

/// Sets the current thread's restricted mode state pointer.
///
/// # Safety
/// Caller must pass a valid `RestrictedState` raw pointer or null pointer.
pub unsafe fn current_set_restricted_state(raw_rs: *mut RestrictedState) {
    // SAFETY: Forwarded to C++ Thread::Current::Get()->set_restricted_state.
    unsafe { cpp_thread_current_set_restricted_state(raw_rs) }
}

/// Returns `true` if the current thread has been signaled.
pub fn current_is_signaled() -> bool {
    // SAFETY: Foreign function wrapper for Thread::Current::Get()->IsSignaled().
    unsafe { cpp_thread_current_is_signaled() }
}

/// If a restricted kick is pending on the current thread, clears it and returns `true`.
/// Otherwise returns `false`.
///
/// Must be called with interrupts disabled.
pub fn current_check_for_restricted_kick() -> bool {
    // SAFETY: Foreign function wrapper for Thread::Current::CheckForRestrictedKick().
    unsafe { cpp_thread_current_check_for_restricted_kick() }
}

/// Returns `true` if memory allocation is allowed on the current thread.
pub fn current_memory_allocation_state_is_enabled() -> bool {
    // SAFETY: Foreign function wrapper for Thread::Current::memory_allocation_state().IsEnabled().
    unsafe { cpp_thread_current_memory_allocation_state_is_enabled() }
}

/// Signals an exception on the current thread, to be handled when the current syscall exits.
///
/// Unlike other signals, this is synchronous, in the sense that a thread signals itself. This
/// exists primarily so that we can unwind the stack in order to get the state of userland's
/// callee-saved registers at the point where userland invoked the syscall.
///
/// `policy_exception_code` should be a `ZX_EXCP_POLICY_CODE_*` value.
pub fn signal_policy_exception(policy_exception_code: u32, policy_exception_data: u32) {
    // SAFETY: Foreign function wrapper for Thread::Current::SignalPolicyException.
    unsafe {
        cpp_thread_current_signal_policy_exception(policy_exception_code, policy_exception_data)
    }
}

/// Migrates the current thread to the CPU identified by `target_cpu`, and hard-pins it there.
///
/// # Safety
///
/// The caller must ensure that:
/// - `target_cpu` is a valid, currently active CPU.
/// - This function is called from normal thread context where blocking/rescheduling is allowed,
///   with no spinlocks or active `ChainLockTransaction` held on the current CPU.
pub unsafe fn migrate_to_cpu(target_cpu: cpu_num_t) {
    // SAFETY: The caller guarantees `target_cpu` is active and that the calling context is a
    // normal thread context with no active `ChainLockTransaction` or spinlocks held.
    unsafe { cpp_thread_current_migrate_to_cpu(target_cpu) }
}

/// Unit tests for Thread Rust bindings.
#[cfg(ktest)]
#[unittest::suite(name = "thread_rust")]
mod tests {
    use super::{
        AnnotatedAutoEagerReschedDisabler, DEFAULT_PRIORITY, LOW_PRIORITY,
        ScopedMemoryAllocationDisabled, Thread, ThreadLockGuard, ThreadPtr,
        cpp_thread_current_memory_allocation_state_disable,
        cpp_thread_current_memory_allocation_state_enable, create_with_priority, current_get,
        current_memory_allocation_state_is_enabled, eager_resched_disable_count, sleep_relative,
    };
    use crate::arch_rs::curr_cpu_num;
    use crate::kconcurrent::{
        ChainLockTransaction, DONE, IRQ_SAVE_OPTION, OwnerGuard, SingleChainLockGuard, clt_tag,
        single_chain_lock_guard_owned,
    };
    use crate::kernel::mp::{INVALID_CPU, cpu_num_to_mask, get_online_mask};
    use crate::kernel::types::{cpu_mask_t, cpu_num_t, highest_cpu_set};
    use crate::platform_rs::timer::{DurationMono, InstantMono};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
    use kprint::kprintln;
    use unittest::{
        assert_eq, assert_ne, assert_true, expect_eq, expect_false, expect_true, unwrap_ok,
    };
    use zx_status::Status;

    // Wait for condition |cond| to become true, polling with slow exponential backoff
    // to avoid pegging the CPU.
    fn wait_for_cond(mut cond: impl FnMut() -> bool) {
        let mut wait_duration = DurationMono::from_micros(1);
        while !cond() {
            let _ = sleep_relative(wait_duration);
            // Increase wait_duration time by ~10%.
            wait_duration = DurationMono(wait_duration.0 + (wait_duration.0 / 10 + 1));
        }
    }

    /// Verifies AnnotatedAutoEagerReschedDisabler and ThreadPtr accessors on the current thread.
    #[test]
    fn eager_resched_disabler_and_thread_accessors() {
        let base_count = eager_resched_disable_count();
        {
            let mut disabler = AnnotatedAutoEagerReschedDisabler::new();
            assert_eq!(eager_resched_disable_count(), base_count + 1);
            disabler.enable();
            assert_eq!(eager_resched_disable_count(), base_count);
            // A second `enable` is a no-op.
            disabler.enable();
            assert_eq!(eager_resched_disable_count(), base_count);

            let _default_disabler = AnnotatedAutoEagerReschedDisabler::default();
            assert_eq!(eager_resched_disable_count(), base_count + 1);
        }
        assert_eq!(eager_resched_disable_count(), base_count);

        // SAFETY: Called in a running kernel test thread context.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` points to the valid current thread.
        let _tid = unsafe { current.tid() };
        // SAFETY: `current` points to the valid current thread.
        let _user_thread: *mut crate::object::ThreadDispatcher =
            unsafe { current.user_thread_raw() };

        // SAFETY: `current` points to the valid current thread.
        let flow1 = unsafe { current.take_next_lock_flow_id() };
        // SAFETY: `current` points to the valid current thread.
        let flow_obs = unsafe { current.lock_flow_id() };
        assert_eq!(flow1, flow_obs);

        // SAFETY: `current` points to the valid current thread.
        let flow2 = unsafe { current.take_next_lock_flow_id() };
        assert_ne!(flow1, flow2);

        {
            // SAFETY: `current` points to the valid current thread, which outlives `lock`.
            let lock = unsafe { current.get_lock() };
            let held = SingleChainLockGuard::scope(
                IRQ_SAVE_OPTION,
                lock,
                clt_tag!("thread_rust_test"),
                |guard| {
                    // SAFETY: `current` is valid and its chain lock is held by `guard`'s
                    // transaction.
                    let _status =
                        unsafe { current.wait_queue_state_blocked_status_locked(guard.token()) };
                    lock.is_held(guard.token())
                },
            );
            assert_true!(held);
            assert_true!(lock.is_unlocked());
        }
    }

    /// Verifies `ThreadLockGuard::proof` and `OwnerGuard::<Thread>` on the current `Thread`.
    #[test]
    fn thread_lock_proof_and_owner_guard() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };
        expect_true!(current.lock().is_unlocked());

        {
            // SAFETY: `current_ptr` is non-null and valid for the guard's lifetime, no transaction
            // is active, and `guard` is stack-pinned and dropped in this scope without blocking.
            ksync::lock!(
                let mut guard = unsafe {
                    ThreadLockGuard::lock(current_ptr, clt_tag!("thread_lock_guard_proof"))
                }
            );
            expect_eq!(guard.thread(), current_ptr);
            expect_false!(current.lock().is_unlocked());
            let proof = guard.as_mut().proof();
            expect_true!(core::ptr::eq(proof.owner(), current));
        }
        expect_true!(current.lock().is_unlocked());

        {
            single_chain_lock_guard_owned!(
                scope,
                current,
                IRQ_SAVE_OPTION,
                clt_tag!("thread_owned_scope")
            );
            expect_true!(current.lock().is_held(scope.token()));
            let proof = scope.proof();
            expect_true!(core::ptr::eq(proof.owner(), current));
        }
        expect_true!(current.lock().is_unlocked());

        let mut held = false;
        let mut owner_matches = false;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("thread_owner_guard"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                held = guard.is_held() && current.lock().is_held(token);
                let proof = guard.proof();
                owner_matches = core::ptr::eq(proof.owner(), current);
                guard.release();
                DONE
            },
        );
        expect_true!(held);
        expect_true!(owner_matches);
        expect_true!(current.lock().is_unlocked());
    }

    /// Tests setting CPU affinity on the current thread across online CPUs.
    #[test]
    fn set_affinity_self_test() {
        // Our worker thread will attempt to schedule itself onto each core, one at
        // a time, and ensure it landed in the right location.
        let mut online_cpus: cpu_mask_t = get_online_mask();
        assert_ne!(online_cpus, 0u32, "Expected at least one CPU to be online.");
        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `online_cpus` on the caller's stack, which remains valid
            // until the caller joins this thread.
            let online_cpus = unsafe { *(arg as *const cpu_mask_t) };
            // SAFETY: Called in a running kernel worker thread context.
            let this = unsafe { ThreadPtr::current() };

            for c in 0..=highest_cpu_set(online_cpus) {
                // Skip offline CPUs.
                if (cpu_num_to_mask(c) & online_cpus) == 0 {
                    continue;
                }

                // Set affinity to the given core.
                // SAFETY: `this` points to the valid current thread.
                let _ = unsafe { this.set_cpu_affinity(cpu_num_to_mask(c)) };

                // Ensure we are on the correct CPU.
                let current_cpu: cpu_num_t = curr_cpu_num();
                if current_cpu != c {
                    kprintln!(
                        "Expected to be running on CPU {}, but actually running on {}.",
                        c,
                        current_cpu
                    );
                    return Status::INTERNAL.into_raw();
                }
            }

            Status::result_into_raw(Ok(()))
        }
        // SAFETY: `worker_body` is a valid thread entry point and `online_cpus` outlives the
        // worker thread (joined below).
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_affinity_self_test_worker".as_ptr(),
                    worker_body,
                    (&raw mut online_cpus).cast(),
                    DEFAULT_PRIORITY,
                )
            },
            "thread_create failed."
        );
        // SAFETY: `worker` has just been created and not yet resumed or joined.
        unsafe { worker.resume() };

        // Wait for the worker thread to test itself.
        // SAFETY: `worker` is valid and has not been joined yet.
        let worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
        expect_eq!(worker_retcode, Status::result_into_raw(Ok(())), "Worker thread failed.");
    }

    /// Tests migrating another running worker thread across online CPUs via `set_cpu_affinity`.
    #[test]
    fn set_affinity_other_test() {
        struct WorkerState {
            current_cpu: AtomicU32,
            should_stop: AtomicI32,
        }
        let mut state = WorkerState {
            current_cpu: AtomicU32::new(INVALID_CPU),
            should_stop: AtomicI32::new(0),
        };

        // Start a worker, which reports the CPU it is running on.
        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `state` on the caller's stack, which remains valid until the
            // caller joins this thread.
            let state = unsafe { &*(arg as *const WorkerState) };
            while state.should_stop.load(Ordering::SeqCst) != 1 {
                state.current_cpu.store(curr_cpu_num(), Ordering::SeqCst);
            }
            0
        }
        // SAFETY: `worker_body` is a valid thread entry point and `state` outlives the worker
        // thread (joined below).
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_affinity_other_test_worker".as_ptr(),
                    worker_body,
                    (&raw mut state).cast(),
                    LOW_PRIORITY,
                )
            },
            "thread_create failed."
        );
        // SAFETY: `worker` has just been created and not yet resumed or joined.
        unsafe { worker.resume() };

        // Migrate the worker task amongst different threads.
        let online_cpus: cpu_mask_t = get_online_mask();
        assert_ne!(online_cpus, 0u32, "Expected at least one CPU to be online.");
        for c in 0..=highest_cpu_set(online_cpus) {
            // Skip offline CPUs.
            if (cpu_num_to_mask(c) & online_cpus) == 0 {
                continue;
            }

            // Set affinity to the given core.
            // SAFETY: `worker` is valid and has not been joined yet.
            let _ = unsafe { worker.set_cpu_affinity(cpu_num_to_mask(c)) };

            // Wait for it to land on the correct CPU.
            wait_for_cond(|| state.current_cpu.load(Ordering::SeqCst) == c);
        }

        // Done.
        state.should_stop.store(1, Ordering::SeqCst);
        // SAFETY: `worker` is valid and has not been joined yet.
        let _worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
    }

    /// Tests enabling and disabling thread memory allocation state and nested guards.
    #[test]
    fn scoped_allocation_disabled_test() {
        expect_true!(current_memory_allocation_state_is_enabled());

        // SAFETY: Disables memory allocation on the current thread and re-enables it immediately
        // below.
        unsafe { cpp_thread_current_memory_allocation_state_disable() };
        expect_false!(current_memory_allocation_state_is_enabled());
        // SAFETY: Balances the preceding `cpp_thread_current_memory_allocation_state_disable` call.
        unsafe { cpp_thread_current_memory_allocation_state_enable() };
        expect_true!(current_memory_allocation_state_is_enabled());

        {
            expect_true!(current_memory_allocation_state_is_enabled());
            {
                let _smad = ScopedMemoryAllocationDisabled::new();
                expect_false!(current_memory_allocation_state_is_enabled());
                {
                    let _nested_smad = ScopedMemoryAllocationDisabled::new();
                    expect_false!(current_memory_allocation_state_is_enabled());
                }
                expect_false!(current_memory_allocation_state_is_enabled());
            }
            expect_true!(current_memory_allocation_state_is_enabled());
        }
    }
}

/// Intensive scheduler and thread synchronization unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "intensive_thread_rust")]
mod intensive_thread_tests {
    // These tests take much longer than, and are more intensive on the scheduler than,
    // the //zircon/kernel/kernel/thread.cc tests.
    use super::{
        DEFAULT_PRIORITY, Interruptible, LOW_PRIORITY, ThreadPtr, create_with_priority,
        current_get, sleep_interruptible, sleep_relative, r#yield,
    };
    use crate::kernel::event::{AutounsignalEvent, Event};
    use crate::platform_rs::timer::{DurationMono, InstantMono};
    use core::ffi::{c_int, c_void};
    use core::ptr;
    use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use kprint::kprintln;
    use pin_init::{pin_init, stack_pin_init};
    use unittest::unwrap_ok;
    use zx_status::Status;
    use zx_types::ZX_ERR_INTERNAL_INTR_KILLED;

    const INTERNAL_INTR_KILLED: Status = Status::err_from_raw(ZX_ERR_INTERNAL_INTR_KILLED);

    unsafe extern "C" {
        fn rand() -> c_int;
    }

    #[ksync::guarded]
    struct IntensiveMutexHolder {
        #[mutex]
        m: ksync::KMutex<ksync::RawMutex>,
    }

    extern "C" fn mutex_thread(arg: *mut c_void) -> i32 {
        const ITERATIONS: i32 = 100000;
        static SHARED: AtomicUsize = AtomicUsize::new(0);

        // SAFETY: `arg` points to `m` on `mutex_test`'s stack, which outlives all worker threads.
        let holder = unsafe { &*(arg as *const IntensiveMutexHolder) };

        for _i in 0..ITERATIONS {
            {
                ksync::lock!(let _guard = holder.lock_m());

                if SHARED.load(Ordering::Relaxed) != 0 {
                    panic!("someone else has messed with the shared data\n");
                }

                SHARED.store(current_get() as usize, Ordering::Relaxed);
                // SAFETY: `rand` is a thread-safe C library PRNG function in the kernel.
                if (unsafe { rand() } % 5) == 0 {
                    r#yield();
                }

                SHARED.store(0, Ordering::Relaxed);
            }
            // SAFETY: `rand` is a thread-safe C library PRNG function in the kernel.
            if (unsafe { rand() } % 5) == 0 {
                r#yield();
            }
        }

        0
    }

    /// Stress-tests mutex acquisition and release across 5 concurrent threads.
    #[test]
    fn mutex_test() {
        stack_pin_init!(let m = pin_init!(IntensiveMutexHolder {
            m <- ksync::KMutex::init(),
        }));
        let m_ptr: *mut c_void = ptr::from_ref(&*m).cast_mut().cast();

        let threads: [ThreadPtr; 5] = core::array::from_fn(|_| {
            // SAFETY: `mutex_thread` is a valid thread entry point and `m` outlives all threads
            // joined below.
            let thread = unsafe {
                create_with_priority(
                    c"mutex tester".as_ptr(),
                    mutex_thread,
                    m_ptr,
                    DEFAULT_PRIORITY,
                )
            }
            .unwrap();
            // SAFETY: `thread` was just created and has not been resumed or joined yet.
            unsafe { thread.resume() };
            thread
        });

        for thread in threads {
            // SAFETY: `thread` is valid and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }

        let _ = sleep_relative(DurationMono::from_millis(100));
    }

    extern "C" fn event_signaler(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` on `event_test`'s stack, which outlives this
        // thread.
        let event = unsafe { &*(arg as *const Event) };

        // event signaler pausing
        let _ = sleep_relative(DurationMono::from_seconds(1));

        event.signal();
        r#yield();

        0
    }

    #[derive(Clone, Copy)]
    struct WaiterArgs {
        event: *const Event,
        count: usize,
    }

    #[allow(clippy::if_same_then_else)]
    extern "C" fn event_waiter(arg: *mut c_void) -> i32 {
        // Copy our arguments here so we can mutate the count.
        // SAFETY: `arg` points to a valid `WaiterArgs` on `event_test`'s stack, which outlives
        // this thread.
        let mut args: WaiterArgs = unsafe { *(arg as *const WaiterArgs) };

        while args.count > 0 {
            // SAFETY: `args.event` points to a valid `Event` on `event_test`'s stack, which
            // outlives this thread.
            let status =
                unsafe { (*args.event).wait_deadline(InstantMono::INFINITE, Interruptible::Yes) };
            if status == Err(INTERNAL_INTR_KILLED) {
                return -1;
            } else if status.is_err() {
                return -1;
            }
            r#yield();
            args.count -= 1;
        }

        0
    }

    /// Tests `Event` (broadcast wakeup) and `AutounsignalEvent` (single-waiter wakeup).
    #[test]
    fn event_test() {
        {
            // make sure signaling the event wakes up all the threads and stays signaled
            stack_pin_init!(let event = Event::init_unsignaled());
            let event_ptr: *const Event = &*event;
            let mut args = WaiterArgs { event: event_ptr, count: 2 };
            let args_ptr: *mut c_void = (&raw mut args).cast();

            // SAFETY: `event_signaler` and `event_waiter` are valid entry points, and `event` and
            // `args` outlive all 5 threads (joined before the end of this block).
            let threads = unsafe {
                [
                    unwrap_ok!(create_with_priority(
                        c"event signaler".as_ptr(),
                        event_signaler,
                        event_ptr.cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 0".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 1".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 2".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 3".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                ]
            };

            for thread in threads {
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe { thread.resume() };
            }

            for thread in threads {
                // SAFETY: `thread` is valid and has not been joined yet.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }

            let _ = sleep_relative(DurationMono::from_seconds(2));
            // destroying event by going out of scope
        }

        {
            stack_pin_init!(let event = AutounsignalEvent::init_unsignaled());
            let event_ptr: *const Event = event.as_event();
            let mut args = WaiterArgs { event: event_ptr, count: 99 };
            let args_ptr: *mut c_void = (&raw mut args).cast();
            // make sure signaling the event wakes up precisely one thread
            // SAFETY: `event_signaler` and `event_waiter` are valid entry points, and `event` and
            // `args` outlive all 5 threads (joined before the end of this block).
            let threads = unsafe {
                [
                    unwrap_ok!(create_with_priority(
                        c"event signaler".as_ptr(),
                        event_signaler,
                        event_ptr.cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 0".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 1".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 2".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                    unwrap_ok!(create_with_priority(
                        c"event waiter 3".as_ptr(),
                        event_waiter,
                        args_ptr,
                        DEFAULT_PRIORITY,
                    )),
                ]
            };

            for thread in threads {
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe { thread.resume() };
            }

            let _ = sleep_relative(DurationMono::from_seconds(2));

            for thread in threads {
                // SAFETY: `thread` is valid and has not been joined yet.
                unsafe {
                    thread.kill();
                    let _ = thread.join(InstantMono::INFINITE);
                }
            }
        }
    }

    static ATOMIC_VAR: AtomicI32 = AtomicI32::new(0);
    static ATOMIC_COUNT: AtomicI32 = AtomicI32::new(0);

    extern "C" fn atomic_tester(arg: *mut c_void) -> i32 {
        let add = arg as isize as i32;
        const ITER: i32 = 10000000;

        for _i in 0..ITER {
            let _ = ATOMIC_VAR.fetch_add(add, Ordering::SeqCst);
        }

        let _ = ATOMIC_COUNT.fetch_sub(1, Ordering::SeqCst);

        0
    }

    /// Stress-tests concurrent atomic `fetch_add` and `fetch_sub` across 8 threads.
    #[test]
    fn atomic_test() {
        ATOMIC_VAR.store(0, Ordering::SeqCst);
        ATOMIC_COUNT.store(8, Ordering::SeqCst);

        #[allow(clippy::manual_dangling_ptr)]
        let plus_one = 1usize as *mut c_void;
        let minus_one = (-1isize as usize) as *mut c_void;
        // SAFETY: `atomic_tester` is a valid thread entry point and interprets its `arg` as an
        // integer delta without dereferencing it.
        let threads = unsafe {
            [
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    plus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    plus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    plus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 1".as_ptr(),
                    atomic_tester,
                    plus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    minus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    minus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    minus_one,
                    LOW_PRIORITY,
                )),
                unwrap_ok!(create_with_priority(
                    c"atomic tester 2".as_ptr(),
                    atomic_tester,
                    minus_one,
                    LOW_PRIORITY,
                )),
            ]
        };

        // start all the threads
        for thread in threads {
            // SAFETY: `thread` was just created and has not been resumed or joined yet.
            unsafe { thread.resume() };
        }

        // wait for them to all stop
        for thread in threads {
            // SAFETY: `thread` is valid and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }

    extern "C" fn sleeper_kill_thread_infinite_wait(_arg: *mut c_void) -> i32 {
        let _ = sleep_relative(DurationMono::from_millis(100));

        kprintln!("sleeper_kill_thread_infinite_wait: waiting until killed");
        let err = sleep_interruptible(InstantMono::INFINITE);
        assert_eq!(err, Err(INTERNAL_INTR_KILLED));

        0
    }

    extern "C" fn waiter_kill_thread_infinite_wait(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` on `kill_test`'s stack, which outlives this
        // thread.
        let e = unsafe { &*(arg as *const Event) };

        let _ = sleep_relative(DurationMono::from_millis(100));

        kprintln!("waiter_kill_thread_infinite_wait: waiting until killed");
        let err = e.wait_deadline(InstantMono::INFINITE, Interruptible::Yes);
        assert_eq!(err, Err(INTERNAL_INTR_KILLED));

        0
    }

    /// Tests killing threads blocked in interruptible sleep or event wait across lifecycle stages.
    #[test]
    fn kill_test() {
        // Starting sleeper thread, then killing it while it sleeps.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.resume();
            let _ = sleep_relative(DurationMono::from_millis(200));
            t.kill();
            let _ = t.join(InstantMono::INFINITE);
        }

        // Starting sleeper thread, then killing it before it wakes up.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.resume();
            t.kill();
            let _ = t.join(InstantMono::INFINITE);
        }

        // Starting sleeper thread, then killing it before it is unsuspended.
        // SAFETY: `sleeper_kill_thread_infinite_wait` is a valid thread entry point that ignores
        // its argument.
        let t = unwrap_ok!(unsafe {
            create_with_priority(
                c"sleeper".as_ptr(),
                sleeper_kill_thread_infinite_wait,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        });
        // SAFETY: `t` was just created and is joined before the end of this section.
        unsafe {
            t.kill(); // kill it before it is resumed
            t.resume();
            let _ = t.join(InstantMono::INFINITE);
        }

        {
            // Starting waiter thread that waits forever, then killing it while it blocks.
            stack_pin_init!(let e = Event::init_unsignaled());
            let e_ptr: *mut c_void = ptr::from_ref(&*e).cast_mut().cast();
            // SAFETY: `waiter_kill_thread_infinite_wait` is a valid entry point and `e` outlives
            // `t` (joined below).
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"waiter".as_ptr(),
                    waiter_kill_thread_infinite_wait,
                    e_ptr,
                    LOW_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and is joined before `e` goes out of scope.
            unsafe {
                t.resume();
                let _ = sleep_relative(DurationMono::from_millis(200));
                t.kill();
                let _ = t.join(InstantMono::INFINITE);
            }
        }

        {
            // Starting waiter thread that waits forever, then killing it before it wakes up.
            stack_pin_init!(let e = Event::init_unsignaled());
            let e_ptr: *mut c_void = ptr::from_ref(&*e).cast_mut().cast();
            // SAFETY: `waiter_kill_thread_infinite_wait` is a valid entry point and `e` outlives
            // `t` (joined below).
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"waiter".as_ptr(),
                    waiter_kill_thread_infinite_wait,
                    e_ptr,
                    LOW_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and is joined before `e` goes out of scope.
            unsafe {
                t.resume();
                t.kill();
                let _ = t.join(InstantMono::INFINITE);
            }
        }
    }
}

/// Preemption disable and timeslice extension RAII guard unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "preempt_disable_tests_rust")]
mod preempt_disable_tests {
    use super::{
        AutoExpiringPreemptDisabler, AutoPreemptDisabler, preempt_disable_count, preempt_is_enabled,
    };
    use crate::platform_rs::timer::DurationMono;
    use unittest::{assert_eq, assert_false, assert_true};

    /// Tests `AutoPreemptDisabler` deferred and immediate modes, nesting, and toggling.
    #[test]
    fn test_auto_preempt_disabler() {
        // Make sure that nothing funny is going on with our preempt disable count as
        // it stands now.
        assert_eq!(0u32, preempt_disable_count());

        {
            // Create a disabler inside of a scope, but do not have it immediately
            // request that preemption be disabled.  Our count should still be zero.
            let mut ap_disabler = AutoPreemptDisabler::new_deferred();
            assert_eq!(0u32, preempt_disable_count());

            // Now explicitly disable.  Our count should go to 1.
            ap_disabler.disable();
            assert_eq!(1u32, preempt_disable_count());

            // Do it again, our count should remain at 1.
            ap_disabler.disable();
            assert_eq!(1u32, preempt_disable_count());

            {
                // Make another inside of a new scope.  Our count should remain at 1 until
                // we explicitly use the new instance to disable preemption.
                let mut ap_disabler2 = AutoPreemptDisabler::new_deferred();
                assert_eq!(1u32, preempt_disable_count());

                ap_disabler2.disable();
                assert_eq!(2u32, preempt_disable_count());
            } // Let it go out of scope, we should drop down to a count of 1.

            assert_eq!(1u32, preempt_disable_count());
        } // Allow the original to go out of scope.  This should get us back down to a count of 0.

        assert_eq!(0u32, preempt_disable_count());

        // Next, do a similar test, but this time with the version which automatically
        // begins life with preemption disabled.
        {
            let mut ap_disabler = AutoPreemptDisabler::new();
            assert_eq!(1u32, preempt_disable_count());

            // Attempting to call disable should do nothing.
            ap_disabler.disable();
            assert_eq!(1u32, preempt_disable_count());

            {
                // Add a second.  Watch the count go up as it comes into scope, and back
                // down again when it goes out.
                let _ap_disabler2 = AutoPreemptDisabler::new();
                assert_eq!(2u32, preempt_disable_count());
            }

            assert_eq!(1u32, preempt_disable_count());
        } // Allow the original to go out of scope.  This should get us back down to a count of 0.

        // Test an explicit Enable
        {
            let mut ap_disabler = AutoPreemptDisabler::new();
            assert_eq!(1u32, preempt_disable_count());

            {
                // Create a defered disabler, and test that Enabling it before its disabled does
                // nothing.
                let mut ap_disabler2 = AutoPreemptDisabler::new_deferred();
                assert_eq!(1u32, preempt_disable_count());

                ap_disabler2.enable();
                assert_eq!(1u32, preempt_disable_count());
            }
            assert_eq!(1u32, preempt_disable_count());

            // Should be able to toggle enable and disable
            ap_disabler.enable();
            assert_eq!(0u32, preempt_disable_count());
            ap_disabler.disable();
            assert_eq!(1u32, preempt_disable_count());
            // Ending on Enable should result in no change after the disabler goes out of scope.
            ap_disabler.enable();
            assert_eq!(0u32, preempt_disable_count());
        }

        assert_eq!(0u32, preempt_disable_count());
    }

    /// Tests basic and nested `AutoExpiringPreemptDisabler` timeslice extensions.
    #[test]
    fn test_auto_timeslice_extension() {
        // Basic.
        {
            assert_true!(preempt_is_enabled());
            {
                let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
                assert_false!(preempt_is_enabled());
            }
            assert_true!(preempt_is_enabled());
        }

        // Nested.  Only the outermost guard matters.
        {
            assert_true!(preempt_is_enabled());
            {
                let _guard1 = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
                assert_false!(preempt_is_enabled());
                {
                    let _guard2 = AutoExpiringPreemptDisabler::new(DurationMono::ZERO);
                    // Even though _guard2's duration is 0, preemption should still be disabled
                    // because of _guard1's extension.
                    assert_false!(preempt_is_enabled());
                }
                assert_false!(preempt_is_enabled());
            }
            assert_true!(preempt_is_enabled());
        }
    }
}
