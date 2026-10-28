// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::enable_ints;
pub use crate::backtrace::Backtrace;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use fbl::{
    DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    SinglyLinkedListContainable, SinglyLinkedListNode,
};
use ksync::KCell;
use pin_init::{PinInit, pin_data, pinned_drop};
use thread_layout_bindings as layout;
use thread_lock_guard_bindings as lock_guard_bindings;
use zr::{Opaque, OpaqueBytes, static_assert};
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_instant_mono_t, zx_koid_t, zx_status_t};

use crate::kconcurrent::ChainLock;
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::relaxed_atomic::{RelaxedAtomicI64, RelaxedAtomicU32, RelaxedAtomicU64};
use crate::kernel::restricted_state::RestrictedState;
use crate::kernel::scheduler;
use crate::kernel::scheduler_state::{SchedulerState, SchedulerStateBaseProfile, ThreadStateKind};
use crate::kernel::types::{cpu_mask_t, cpu_num_t};
use crate::kernel::wait_queue::{ResourceOwnership, ThreadState, WaitQueue, WaitQueueBase};
use crate::ktrace_rs::InternedString;
use crate::vm::vm_aspace::VmAspace;

pub use crate::kconcurrent::CallsiteInfo as CltTag;
use crate::kconcurrent::{
    ChainLockProof, ChainLockTransaction, ChainLockTransactionToken, ChainLocked, ClassedChainLock,
    OwnerGuard,
};

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
    fn cpp_thread_detach_and_resume(thread: *mut Thread) -> zx_status_t;
    fn cpp_thread_join(
        thread: *mut Thread,
        out_retcode: *mut i32,
        deadline: zx_instant_mono_t,
    ) -> i32;
    fn cpp_thread_forget(thread: *mut Thread);
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
    fn cpp_thread_current_switch_aspace(aspace: *mut VmAspace) -> *mut VmAspace;
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
    fn cpp_thread_is_running(thread: *const Thread) -> bool;
    fn cpp_thread_process_pending_signals(frame: *mut c_void);
    fn cpp_thread_in_restricted(thread: *mut Thread) -> bool;
    fn cpp_thread_is_user_thread(thread: *const Thread) -> bool;
    fn cpp_thread_active_aspace(thread: *mut Thread) -> *mut VmAspace;
    fn cpp_thread_current_restricted_state() -> *mut RestrictedState;
    fn cpp_thread_current_set_restricted_state(raw_rs: *mut RestrictedState);
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
    fn cpp_thread_get_cpu_affinity(thread: *const Thread) -> cpu_mask_t;
    fn cpp_thread_set_cpu_affinity(thread: *mut Thread, affinity: cpu_mask_t) -> cpu_mask_t;
    fn cpp_thread_get_soft_cpu_affinity(thread: *const Thread) -> cpu_mask_t;
    fn cpp_thread_set_soft_cpu_affinity(thread: *mut Thread, affinity: cpu_mask_t) -> cpu_mask_t;
    fn cpp_thread_last_cpu(thread: *const Thread) -> cpu_num_t;
    fn cpp_thread_current_migrate_to_cpu(target_cpu: cpu_num_t);
    fn cpp_thread_user_thread(thread: *mut Thread) -> *mut crate::object::ThreadDispatcher;
    fn cpp_thread_lock_flow_id(thread: *const Thread) -> u64;
    fn cpp_thread_take_next_lock_flow_id(thread: *mut Thread) -> u64;
    fn cpp_thread_get_list_lock() -> *mut ksync::KSpinlock<ThreadListLockClass>;
    fn cpp_thread_id_to_thread_slow(tid: zx_koid_t) -> *mut Thread;
    fn cpp_thread_get_backtrace(thread: *mut Thread, out_bt: *mut Backtrace);
    fn cpp_thread_current_get_backtrace(out_bt: *mut Backtrace);
    fn cpp_thread_dump(thread: *const Thread, full: bool);
    fn cpp_thread_runtime(thread: *const Thread) -> DurationMono;
    fn cpp_thread_dump_tid(tid: zx_koid_t, full: bool);
    fn cpp_thread_dump_tid_during_panic(tid: zx_koid_t, full: bool);
    fn cpp_thread_dump_all(match_owner: *const c_char, full: bool);
    fn cpp_thread_dump_all_during_panic(match_owner: *const c_char, full: bool);
    fn cpp_thread_current_reschedule();
    fn cpp_scoped_thread_exception_context_init(
        storage: *mut core::mem::MaybeUninit<CppScopedThreadExceptionContext>,
        context: *const crate::arch_rs::ArchExceptionContext,
    );
    fn cpp_scoped_thread_exception_context_destroy(context: *mut CppScopedThreadExceptionContext);
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
    // SAFETY: The caller guarantees `thread` is a valid, live `Thread` for `'a` and holds its lock
    // for `'a`, which excludes concurrent mutation of `scheduler_state`.
    unsafe { (*thread).scheduler_state_unchecked() }
}

// LINT.IfChange(thread_flags_and_signals)
/// Thread is detached and will not be joined.
pub const THREAD_FLAG_DETACHED: u32 = 1 << 0;
/// `Thread` struct is heap-allocated and should be freed on exit.
pub const THREAD_FLAG_FREE_STRUCT: u32 = 1 << 1;
/// Thread is a per-CPU idle/power thread.
pub const THREAD_FLAG_IDLE: u32 = 1 << 2;
/// Thread runs a guest VCPU.
pub const THREAD_FLAG_VCPU: u32 = 1 << 3;
/// A restricted-mode kick is pending on this thread.
pub const THREAD_FLAG_RESTRICTED_KICK_PENDING: u32 = 1 << 4;

/// Thread has been signaled to terminate.
pub const THREAD_SIGNAL_KILL: u32 = 1 << 0;
/// Thread has been signaled to suspend.
pub const THREAD_SIGNAL_SUSPEND: u32 = 1 << 1;
/// Thread has a pending job policy exception to deliver on syscall exit.
pub const THREAD_SIGNAL_POLICY_EXCEPTION: u32 = 1 << 2;
/// Thread has been kicked out of restricted mode.
pub const THREAD_SIGNAL_RESTRICTED_KICK: u32 = 1 << 3;
/// Thread has been signaled to sample its user stack.
pub const THREAD_SIGNAL_SAMPLE_STACK: u32 = 1 << 4;
/// Thread has been signaled to check its restartable sequence before returning to usermode.
pub const THREAD_SIGNAL_CHECK_RSEQ: u32 = 1 << 5;
/// Thread is being killed due to a fatal job policy violation.
pub const THREAD_SIGNAL_POLICY_KILL: u32 = 1 << 6;
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:thread_flags_and_signals,
//   //zircon/kernel/kernel/thread_ffi.cc:thread_flags_and_signals,
// )

/// `ZX_ERR_INTERNAL_INTR_KILLED`; `zx_status::Status` has no named constant for it.
const ZX_ERR_INTERNAL_INTR_KILLED: Status =
    Status::err_from_raw(zx_types::ZX_ERR_INTERNAL_INTR_KILLED);
/// `ZX_ERR_INTERNAL_INTR_RETRY`, an alias of `ZX_ERR_INTERRUPTED_RETRY`.
const ZX_ERR_INTERNAL_INTR_RETRY: Status = Status::INTERRUPTED_RETRY;

/// Process and thread koids for a tracing thread reference.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FxtRef {
    pub pid: u64,
    pub tid: u64,
}

/// Snapshot of a thread's name buffer.
pub struct ThreadName {
    buf: [u8; ZX_MAX_NAME_LEN],
    len: usize,
}

impl ThreadName {
    /// Returns an empty [`ThreadName`].
    #[inline]
    pub const fn empty() -> Self {
        Self { buf: [0u8; ZX_MAX_NAME_LEN], len: 0 }
    }

    /// Returns the name bytes before the first NUL byte.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Returns the name as a UTF-8 string slice, or `""` if it contains invalid UTF-8.
    #[inline]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl core::fmt::Display for ThreadName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&zr::from_utf8_lossy(self.as_bytes()), f)
    }
}

/// Tag type for a [`Thread`]'s membership in the global migrate list (`migrate_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrateListTag;

/// Tag type for a [`Thread`]'s membership in the global thread list (`thread_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ThreadListTag;

/// Tag type for a [`Thread`]'s membership in a temporary batch unblock list (`unblock_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnblockListTag;

/// Tag type for a [`Thread`]'s membership in a save state list (`save_state_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SaveStateListTag;

/// The global list of threads with migrate functions.
pub type MigrateList = DoublyLinkedList<NonNull<Thread>, MigrateListTag>;
/// The global thread list.
pub type ThreadList = DoublyLinkedList<NonNull<Thread>, ThreadListTag>;
/// A temporary batch unblock list.
pub type UnblockList = DoublyLinkedList<NonNull<Thread>, UnblockListTag>;
/// A save state list.
pub type SaveStateList = fbl::SinglyLinkedList<NonNull<Thread>, SaveStateListTag>;
/// The list of `OwnedWaitQueue`s a [`Thread`] owns.
pub type OwnedWaitQueueList = DoublyLinkedList<NonNull<OwnedWaitQueue>>;

// `Thread::owned_wait_queues_` is a `fbl::DoublyLinkedList<OwnedWaitQueue*>`, which consists of a
// single head pointer.
static_assert!(size_of::<OwnedWaitQueueList>() == size_of::<*mut c_void>());
static_assert!(align_of::<OwnedWaitQueueList>() == align_of::<*mut c_void>());

/// Per-thread preemption state.
#[repr(C)]
pub struct PreemptionState {
    /// `state` contains three fields:
    ///
    ///  * a 15-bit preempt disable counter (bits 0-14)
    ///  * a 15-bit eager resched disable counter (bits 15-29)
    ///  * a 2-bit for TimesliceExtensionFlags (bits 30-31)
    ///
    /// This is a single field so that both counters and the flags can be compared
    /// against zero with a single memory access and comparison.
    ///
    /// `state`'s counts are modified by interrupt handlers, but the counts are
    /// always restored to their original value before the interrupt handler
    /// returns, so modifications are not visible to the interrupted thread.
    state: RelaxedAtomicU32,

    /// `preempts_pending` tracks pending reschedules to both local and remote CPUs
    /// due to activity in the context of the current thread.
    ///
    /// This value can be changed asynchronously by an interrupt handler.
    ///
    /// `preempts_pending` should only be non-zero:
    ///  * if `preempt_disable_count()` or `eager_resched_disable_count()` are non-zero, or
    ///  * after `preempt_disable_count()` or `eager_resched_disable_count()` have been
    ///    decremented, while `preempts_pending` is being checked.
    preempts_pending: RelaxedAtomicU32,

    /// The maximum duration of the thread's timeslice extension.
    ///
    /// This field is only valid when `state`'s
    /// `TIMESLICE_EXTENSION_PRESENT` flag is set.
    ///
    /// This field may only be accessed by its owning thread or in an interrupt
    /// context of the owning thread.  When reading this field, be sure to issue a
    /// `compiler_fence` (Rust's `atomic_signal_fence`; a compiler barrier) with acquire semantics
    /// after observing the Present flag.  Likewise, when writing this field, use a
    /// `compiler_fence` with release semantics prior to setting the Present
    /// flag.  By using these fences, we ensure the flag and field value remain in
    /// sync.
    timeslice_extension: RelaxedAtomicI64,

    /// The deadline at which the thread timeslice extension expires.
    ///
    /// This field is only valid when `TIMESLICE_EXTENSION_ACTIVE` flag is
    /// set.
    ///
    /// This field may only be accessed by its owning thread or in an interrupt
    /// context of the owning thread.  When reading this field, be sure to issue a
    /// `compiler_fence` (Rust's `atomic_signal_fence`; a compiler barrier) with acquire semantics
    /// after observing the Active flag.  Likewise, when writing this field, use a
    /// `compiler_fence` with release semantics prior to setting the Active
    /// flag.  By using these fences, we ensure the flag and field value remain in
    /// sync.
    timeslice_extension_deadline: RelaxedAtomicI64,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Flush: u32 {
        const LOCAL = 0x1;
        const REMOTE = 0x2;
        const ALL = Self::LOCAL.bits() | Self::REMOTE.bits();
    }
}

// counts the number of times a thread's timeslice extension was activated (see
// `PreemptionState::set_timeslice_extension`).
crate::counters::define_kcounter!(THREAD_TIMESLICE_EXTENDED, "thread.timeslice_extended", Sum);

impl PreemptionState {
    /// Counters contained in state_ are limited to 15 bits.
    pub const MAX_COUNT_VALUE: u32 = 0x7fff;
    /// The preempt disable count is in the lowest 15 bits.
    pub const PREEMPT_DISABLE_MASK: u32 = Self::MAX_COUNT_VALUE;
    /// The eager resched disable count is in the next highest 15 bits.
    pub const EAGER_RESCHED_DISABLE_SHIFT: u32 = 15;
    /// Mask selecting the eager resched disable count.
    pub const EAGER_RESCHED_DISABLE_MASK: u32 =
        Self::MAX_COUNT_VALUE << Self::EAGER_RESCHED_DISABLE_SHIFT;
    /// Finally, the timeslice extension flags are the highest 2 bits.
    pub const TIMESLICE_EXTENSION_FLAGS_SHIFT: u32 = 30;
    /// Mask selecting the timeslice extension flags.
    pub const TIMESLICE_EXTENSION_FLAGS_MASK: u32 =
        !(Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK);
    /// Thread has a timeslice extension that may or may not be Active.
    pub const TIMESLICE_EXTENSION_PRESENT: u32 = 0b01 << Self::TIMESLICE_EXTENSION_FLAGS_SHIFT;
    /// Thread has an Active (in use) timeslice extension.
    pub const TIMESLICE_EXTENSION_ACTIVE: u32 = 0b10 << Self::TIMESLICE_EXTENSION_FLAGS_SHIFT;

    /// Returns true if preemption is enabled for this thread: both counts are zero and there is
    /// no runtime extension.
    #[inline]
    pub fn preempt_is_enabled(&self) -> bool {
        let state = self.state.load();
        state == 0
    }

    /// Returns the thread's preempt disable count.
    #[inline]
    pub fn preempt_disable_count(&self) -> u32 {
        Self::preempt_disable_count_of(self.state.load())
    }

    /// Returns the thread's eager resched disable count.
    #[inline]
    pub fn eager_resched_disable_count(&self) -> u32 {
        Self::eager_resched_disable_count_of(self.state.load())
    }

    /// Returns the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending(&self) -> cpu_mask_t {
        self.preempts_pending.load()
    }

    /// Clears the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending_clear(&self) {
        self.preempts_pending.store(0);
    }

    /// Adds `mask` to the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending_add(&self, mask: cpu_mask_t) {
        self.preempts_pending.fetch_or(mask);
    }

    /// PreemptDisable() increments the preempt disable counter for the current
    /// thread. While preempt disable is non-zero, preemption of the thread is
    /// disabled, including preemption from interrupt handlers. During this time,
    /// any call to Reschedule() will only record that a reschedule is pending, and
    /// won't do a context switch.
    ///
    /// Note that this does not disallow blocking operations (e.g.
    /// mutex.Acquire()). Disabling preemption does not prevent switching away from
    /// the current thread if it blocks.
    ///
    /// A call to `preempt_disable()` must be matched by a later call to
    /// `preempt_reenable()` to decrement the preempt disable counter.
    #[inline]
    pub fn preempt_disable(&self) {
        let old_state = self.state.fetch_add(1);
        if Self::preempt_disable_count_of(old_state) >= Self::MAX_COUNT_VALUE {
            Self::count_overflow("preempt disable", old_state);
        }
    }

    /// Panics for a saturated disable counter. Kept out of line and cold so that the message
    /// formatting does not bloat the hot `*_disable` entry points (which are inlined, including
    /// across the C++ FFI boundary).
    #[cold]
    #[inline(never)]
    #[track_caller]
    fn count_overflow(what: &str, old_state: u32) -> ! {
        panic!("{what} count overflow: old_state: {old_state:#x}");
    }

    /// Panics for a disable counter that was decremented from zero. Kept out of line and cold
    /// so that the message formatting does not bloat the `*_reenable_slow` paths (formatting
    /// `old_state` in place would force a SafeStack frame and defeat the tail call into
    /// [`Self::flush_pending_continued`]).
    #[cold]
    #[inline(never)]
    #[track_caller]
    fn count_underflow(what: &str, old_state: u32) -> ! {
        panic!("{what} count underflow: unmasked preemption state: {old_state:#x}");
    }

    /// PreemptReenable() decrements the preempt disable counter and flushes any
    /// pending local preemption operation.  Callers must ensure that they are
    /// calling from a context where blocking is allowed, as the call may result in
    /// the immediate preemption of the calling thread.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the flush path, as in C++).
    ///
    /// # Safety
    ///
    /// - `self` must be the *current* thread's preemption state: the flush may reschedule the
    ///   current thread, and `&self` survives that only because the current thread outlives its
    ///   own run.
    /// - The call must pair with a prior, still unmatched [`Self::preempt_disable`] on the
    ///   current thread, and it must not drop preemption back to enabled while any
    ///   [`PreemptDisabledToken`] is live.
    #[inline]
    pub unsafe fn preempt_reenable(&self) {
        let old_state = self.state.fetch_sub(1);
        // Fast path: count drops to zero, no eager resched count, no extension, nothing pending.
        if old_state == 1 && self.preempts_pending.load() == 0 {
            return;
        }
        // Are either of the counters non-zero?  Only take the early return when the preempt
        // disable count was at least one: the underflow case (count already zero) must fall
        // through to the slow path, which asserts on it like the C++ `ASSERT_MSG` did.
        if Self::preempt_disable_count_of(old_state) != 0
            && (old_state & (Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK)) > 1
        {
            // We've got a non-zero count in one of the counters.
            return;
        }
        // SAFETY: Forwarded from the caller.
        unsafe { self.preempt_reenable_slow(old_state) }
    }

    /// The uncommon remainder of [`Self::preempt_reenable`], after the inline fast-path checks;
    /// `old_state` is the `state` value before the decrement. Kept out of line so that the hot
    /// entry point stays small enough to inline (including across the C++ FFI boundary).
    ///
    /// # Safety
    ///
    /// As [`Self::preempt_reenable`], and the caller has already decremented the preempt disable
    /// count once, observing `old_state`, and returned early unless the eager resched disable
    /// count of `old_state` is zero and its preempt disable count is at most one.
    #[cold]
    #[inline(never)]
    unsafe fn preempt_reenable_slow(&self, old_state: u32) {
        if Self::preempt_disable_count_of(old_state) == 0 {
            Self::count_underflow("preempt disable", old_state);
        }

        // First, check for the expected situation of dropping the preempt count to zero
        // with a zero eager resched disable count and no timeslice extension.  The inline
        // wrapper only gets here in that case when a preemption is pending, so flush directly.
        if old_state == 1 {
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending_continued(Flush::LOCAL) };
            return;
        }

        // Things must be more complicated.  The inline wrapper already returned when either
        // counter was still non-zero, so the counters are both zero.  At this point, we must
        // have a timeslice extension installed.  This extension may be inactive, active and
        // not-yet-expired, or active and expired.

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // It has.  We can flush.
                debug_assert!(self.preempt_is_enabled());
                // SAFETY: Forwarded from the caller.
                unsafe { self.flush_pending(Flush::LOCAL) };
            }
        }

        // We have an extension that's either inactive or active+unexpired.
    }

    /// PreemptReenableDelayFlush() decrements the preempt disable counter, but
    /// deliberately does _not_ flush any pending local preemption operation.
    /// Instead, if local preemption has become enabled again after the count
    /// drops, and the local pending bit is set, the method will clear the bit and
    /// return true.  Otherwise, it will return false.
    ///
    /// This method may only be called when interrupts are disabled and blocking is
    /// not allowed.
    ///
    /// Callers of this method are "taking" ownership of the responsibility to
    /// ensure that preemption on the local CPU takes place in the near future
    /// after the call if the method returns true.
    ///
    /// Use of this method is strongly discouraged outside of top-level interrupt
    /// glue and early threading setup.
    ///
    /// TODO(johngro): Consider replacing the bool return type with a move-only
    /// RAII type which wraps the bool, and ensures that preemption event _must_
    /// happen, either by having the user call a method on the object to manually
    /// force the preemption event, or when the object destructs.
    ///
    /// # Safety
    ///
    /// The call must pair with a prior, still unmatched [`Self::preempt_disable`] on the current
    /// thread, and it must not drop preemption back to enabled while any [`PreemptDisabledToken`]
    /// is live.
    #[must_use]
    #[inline]
    pub unsafe fn preempt_reenable_delay_flush(&self) -> bool {
        debug_assert!(crate::arch_rs::ints_disabled());
        debug_assert!(crate::arch_rs::blocking_disallowed());

        let old_state = self.state.fetch_sub(1);

        // First, check for the expected situation of dropping the preempt count to zero
        // with a zero eager resched disable count and no timeslice extension.
        if old_state == 1 {
            let local_mask = crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
            let prev_mask = self.preempts_pending.fetch_and(!local_mask);
            return (local_mask & prev_mask) != 0;
        }
        // Are either of the counters non-zero?  Only take the early return when the preempt
        // disable count was at least one: the underflow case (count already zero) must fall
        // through to the slow path, which asserts on it like the C++ `ASSERT` did.
        if Self::preempt_disable_count_of(old_state) != 0
            && (old_state & (Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK)) > 1
        {
            // We've got a non-zero count in one of the counters.
            return false;
        }
        self.preempt_reenable_delay_flush_slow(old_state)
    }

    /// The uncommon remainder of [`Self::preempt_reenable_delay_flush`], after the inline
    /// fast-path checks; `old_state` is the `state` value before the decrement. Kept out of line
    /// so that the hot entry point stays small enough to inline (including across the C++ FFI
    /// boundary). Only reached when the eager resched disable count of `old_state` is zero and
    /// its preempt disable count is at most one.
    #[must_use]
    #[cold]
    #[inline(never)]
    fn preempt_reenable_delay_flush_slow(&self, old_state: u32) -> bool {
        if Self::preempt_disable_count_of(old_state) == 0 {
            Self::count_underflow("preempt disable", old_state);
        }

        // The inline wrapper already returned when either counter was still non-zero, so the
        // counters are both zero.  At this point, we must have a timeslice
        // extension installed.  This extension may be inactive, active and
        // not-yet-expired, or active and expired.

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // It has.
                debug_assert!(self.preempt_is_enabled());
                let local_mask =
                    crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
                let prev_mask = self.preempts_pending.fetch_and(!local_mask);
                return (local_mask & prev_mask) != 0;
            }
        }

        // We have an extension that's either inactive or active+unexpired.
        false
    }

    /// EagerReschedDisable() increments the eager resched disable counter for the
    /// current thread. When early resched disable is non-zero, issuing local and
    /// remote preemptions is disabled, including from interrupt handlers. During
    /// this time, any call to Reschedule() or other scheduler entry points that
    /// imply a reschedule will only record the pending reschedule for the affected
    /// CPU, but will not perform reschedule IPIs or a local context switch.
    ///
    /// As with PreemptDisable, blocking operations are still allowed while
    /// eager resched disable is non-zero.
    ///
    /// A call to `eager_resched_disable()` must be matched by a later call to
    /// `eager_resched_reenable()` to decrement the eager resched disable counter.
    #[inline]
    pub fn eager_resched_disable(&self) {
        let old_state = self.state.fetch_add(1 << Self::EAGER_RESCHED_DISABLE_SHIFT);
        if Self::eager_resched_disable_count_of(old_state) >= Self::MAX_COUNT_VALUE {
            Self::count_overflow("eager resched disable", old_state);
        }
    }

    /// EagerReschedReenable() decrements the eager resched disable counter and
    /// flushes pending local and/or remote preemptions if enabled, respectively.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the local flush path, as in C++).
    ///
    /// # Safety
    ///
    /// - `self` must be the *current* thread's preemption state (see [`Self::preempt_reenable`]).
    /// - The call must pair with a prior, still unmatched [`Self::eager_resched_disable`] on the
    ///   current thread, and it must not drop preemption back to enabled while any
    ///   [`PreemptDisabledToken`] is live.
    #[inline]
    pub unsafe fn eager_resched_reenable(&self) {
        let old_state = self.state.fetch_sub(1 << Self::EAGER_RESCHED_DISABLE_SHIFT);
        // Fast path: count drops to zero, no preempt disable count, no extension, nothing pending.
        if old_state == 1 << Self::EAGER_RESCHED_DISABLE_SHIFT && self.preempts_pending.load() == 0
        {
            return;
        }
        if Self::eager_resched_disable_count_of(old_state) > 1 {
            // Nothing to do since eager resched disable implies preempt disable.
            return;
        }
        // SAFETY: Forwarded from the caller.
        unsafe { self.eager_resched_reenable_slow(old_state) }
    }

    /// The uncommon remainder of [`Self::eager_resched_reenable`], after the inline fast-path
    /// checks; `old_state` is the `state` value before the decrement. Kept out of line so that
    /// the hot entry point stays small enough to inline (including across the C++ FFI boundary).
    ///
    /// # Safety
    ///
    /// As [`Self::eager_resched_reenable`], and the caller has already decremented the eager
    /// resched disable count once, observing `old_state`, and returned early unless the eager
    /// resched disable count of `old_state` is at most one.
    #[cold]
    #[inline(never)]
    unsafe fn eager_resched_reenable_slow(&self, old_state: u32) {
        if Self::eager_resched_disable_count_of(old_state) == 0 {
            Self::count_underflow("eager resched disable", old_state);
        }

        // First check the expected case.
        if old_state == 1 << Self::EAGER_RESCHED_DISABLE_SHIFT {
            // Counts are both zero and there's no timeslice extension.  The inline wrapper only
            // gets here in that case when a preemption is pending, so flush directly.
            //
            // Flushing all might reschedule this CPU, make sure it's OK to block.
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending_continued(Flush::ALL) };
            return;
        }

        // The inline wrapper already returned when the eager resched disable count was still
        // non-zero (nothing to do since eager resched disable implies preempt disable).

        // We know we can at least flush remote.  Can we also flush local?
        if Self::preempt_disable_count_of(old_state) > 0 {
            // Nope, we've got a non-zero preempt disable count.
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending(Flush::REMOTE) };
            return;
        }

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // Yes, preempt disable count is zero and the active extension has
                // expired.  We can flush all.
                debug_assert!(self.preempt_is_enabled());
                // SAFETY: Forwarded from the caller.
                unsafe { self.flush_pending(Flush::ALL) };
                return;
            }
            // Extension is active, can't flush local.
        }

        // We have an inactive extension or an unexpired active extension.  Either
        // way, we can flush remote, but not local.
        // SAFETY: Forwarded from the caller.
        unsafe { self.flush_pending(Flush::REMOTE) };
    }

    /// Sets a timeslice extension if one is not already set.
    ///
    /// This method should only be called in normal thread context.
    ///
    /// Returns false if a timeslice extension was already present or if the
    /// supplied duration is <= 0.
    ///
    /// Note: It OK to call this from a context where preemption is (hard)
    /// disabled.  If preemption is requested while the preempt disable count is
    /// non-zero and a timeslice extension is in place, the extension will be
    /// activated, but preemption will not occur until the count has dropped to
    /// zero and the extension has expired or has been clear.
    pub fn set_timeslice_extension(
        &self,
        extension_duration: zx_types::zx_duration_mono_t,
    ) -> bool {
        if extension_duration <= 0 {
            return false;
        }

        let state = self.state.load();
        if Self::has_timeslice_extension(state) {
            return false;
        }
        self.timeslice_extension.store(extension_duration);
        // Make sure that the timeslice extension value becomes visible to an
        // interrupt handler in this thread prior to the state flag becoming
        // visible.  See comment at |timeslice_extension|.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.state.fetch_or(Self::TIMESLICE_EXTENSION_PRESENT);
        true
    }

    /// Unconditionally clears any timeslice extension.
    ///
    /// This method must be called in normal thread context because it may trigger
    /// local preemption.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the flush path, as in C++).
    ///
    /// # Safety
    ///
    /// `self` must be the *current* thread's preemption state (see [`Self::preempt_reenable`]).
    pub unsafe fn clear_timeslice_extension(&self) {
        // Clear any present timeslice extension.
        let old_state = self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
        // Are the counters both zero?
        if (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0 {
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending(Flush::LOCAL) };
        }
    }

    /// PreemptSetPending() marks a pending preemption for the given CPUs.
    ///
    /// This is similar to Reschedule(), except that it may only be used inside an
    /// interrupt handler while interrupts and preemption are disabled, between
    /// PreemptDisable() and PreemptReenable(). It is similar to Reschedule(),
    /// except that it does not need to be called with thread's lock held.
    pub fn preempt_set_pending(&self, reschedule_mask: cpu_mask_t) {
        debug_assert!(crate::arch_rs::ints_disabled());
        debug_assert!(crate::arch_rs::blocking_disallowed());
        debug_assert!(!self.preempt_is_enabled());

        // Latch the local CPU's mask before the atomic RMW below (interrupts are disabled, so the
        // CPU cannot change) so that the compiler can share it with the caller's copy.
        let local_cpu_mask = crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());

        self.preempts_pending.fetch_or(reschedule_mask);

        // Are we pending for the local CPU?
        let local_cpu_in_resched_mask = (reschedule_mask & local_cpu_mask) != 0;
        if !local_cpu_in_resched_mask {
            // Nope.
            return;
        }

        self.evaluate_timeslice_extension();
    }

    /// Evaluate the thread's timeslice extension (if present), activating or
    /// expiring it as necessary.
    ///
    /// Returns whether preemption is enabled.
    #[inline]
    pub fn evaluate_timeslice_extension(&self) -> bool {
        let old_state = self.state.load();
        if old_state == 0 {
            // No counts, no extension.  The common case.
            return true;
        }
        self.evaluate_timeslice_extension_slow(old_state)
    }

    /// The uncommon remainder of [`Self::evaluate_timeslice_extension`], after the inline
    /// fast-path check; `old_state` is the `state` value just loaded. Kept out of line so that the
    /// hot entry point stays small enough to inline (including across the C++ FFI boundary).
    #[cold]
    #[inline(never)]
    fn evaluate_timeslice_extension_slow(&self, old_state: u32) -> bool {
        if !Self::has_timeslice_extension(old_state) {
            // No extension, but we have a non-zero count.
            return false;
        }

        if Self::has_active_timeslice_extension(old_state) {
            if !self.clear_active_timeslice_extension_if_expired() {
                return false;
            }
            // The active extension has expired.  If the counts are both zero, then
            // we're ready for preemption.
            return (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0;
        }

        // We have a not-yet-active extension.  Time to activate it.
        //
        // See comment at |timeslice_extension| for why the signal fence is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        let extension_duration = self.timeslice_extension.load();
        if extension_duration <= 0 {
            // Already expired.
            self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
            return (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0;
        }
        let deadline = (crate::platform_rs::timer::current_mono_time()
            + crate::platform_rs::timer::DurationMono(extension_duration))
        .0;
        self.timeslice_extension_deadline.store(deadline);
        // See comment at |timeslice_extension_deadline| for why the signal fence
        // is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.state.fetch_or(Self::TIMESLICE_EXTENSION_ACTIVE);
        Self::set_preemption_timer_for_extension(deadline);
        false
    }

    /// Resets the preemption state. This should only be called when reviving an
    /// idle/power thread that halted while taking a CPU offline.
    ///
    /// # Safety
    ///
    /// The owning thread must be halted (not running on any CPU) and no [`PreemptDisabledToken`]
    /// for it may be live; `Reset` exists only for `ReviveIdlePowerThread`.
    pub unsafe fn reset(&self) {
        self.state.store(0);
        self.preempts_pending.store(0);
        self.timeslice_extension.store(0);
        self.timeslice_extension_deadline.store(0);
    }

    const fn eager_resched_disable_count_of(state: u32) -> u32 {
        (state & Self::EAGER_RESCHED_DISABLE_MASK) >> Self::EAGER_RESCHED_DISABLE_SHIFT
    }

    const fn preempt_disable_count_of(state: u32) -> u32 {
        state & Self::PREEMPT_DISABLE_MASK
    }

    const fn has_timeslice_extension(state: u32) -> bool {
        (state & Self::TIMESLICE_EXTENSION_PRESENT) != 0
    }

    const fn has_active_timeslice_extension(state: u32) -> bool {
        (state & Self::TIMESLICE_EXTENSION_ACTIVE) != 0
    }

    /// Sets the preemption timer when a timeslice has been extended.
    fn set_preemption_timer_for_extension(deadline: zx_instant_mono_t) {
        // Interrupts must be disabled when calling PreemptReset.
        let _interrupt_disable = crate::arch_rs::InterruptDisableGuard::new();
        // SAFETY: Interrupts are disabled for the call, as `TimerQueue::PreemptReset` requires,
        // and with interrupts disabled `curr_cpu_num()` names the current CPU, so this is the
        // current CPU's timer queue.
        unsafe {
            crate::kernel::percpu::PerCpu::get(crate::arch_rs::curr_cpu_num())
                .timer_queue
                .preempt_reset(deadline)
        };
        THREAD_TIMESLICE_EXTENDED.add(1);
    }

    /// Checks whether the active timeslice extension has expired and if so, clears
    /// it and returns true.
    ///
    /// Should only be called when there is an active timeslice extension.
    fn clear_active_timeslice_extension_if_expired(&self) -> bool {
        // Has the extension expired?
        //
        // See comment at |timeslice_extension_deadline| for why the signal fence is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        if crate::platform_rs::timer::current_mono_time()
            >= crate::platform_rs::timer::InstantMono(self.timeslice_extension_deadline.load())
        {
            self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
            return true;
        }
        false
    }

    /// Flushes local, remote, or all pending preemptions.
    ///
    /// This method is kept split from [`Self::flush_pending_continued`] so that the early-out
    /// case of no pending preemptions stays small and inlinable.
    ///
    /// # Safety
    ///
    /// `self` must be the *current* thread's preemption state: flushing a local preemption
    /// reschedules the current thread, and `&self` stays valid across that only because the
    /// current thread outlives its own run.
    #[inline]
    unsafe fn flush_pending(&self, flush: Flush) {
        // Early out to avoid unnecessarily taking the thread lock. This check races
        // any potential flush due to context switch, however, the context switch can
        // only clear bits that would have been flushed below, no new pending
        // preemptions are possible in the mask bits indicated by |flush|.
        if self.preempts_pending.load() == 0 {
            return;
        }
        core::hint::cold_path();
        // SAFETY: Forwarded from the caller.
        unsafe { self.flush_pending_continued(flush) }
    }

    /// The out-of-line part of [`Self::flush_pending`], entered only when a preemption is
    /// pending.
    ///
    /// # Safety
    ///
    /// As [`Self::flush_pending`].
    #[cold]
    #[inline(never)]
    unsafe fn flush_pending_continued(&self, flush: Flush) {
        // If we're flushing the local CPU, make sure OK to block since flushing
        // local may trigger a reschedule.
        debug_assert!(!flush.contains(Flush::LOCAL) || !crate::arch_rs::blocking_disallowed());

        let _interrupt_disable = crate::arch_rs::InterruptDisableGuard::new();
        // Recheck, pending preemptions could have been flushed by a context switch
        // before interrupts were disabled.
        let pending_mask = self.preempts_pending.load();

        // If there is a pending local preemption the scheduler will take care of
        // flushing all pending reschedules.
        let current_cpu_mask =
            crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
        if (pending_mask & current_cpu_mask) != 0 && flush.contains(Flush::LOCAL) {
            // Clear the local preempt pending flag before calling preempt.  Failure
            // to do this can cause recursion during Scheduler::Preempt if any code
            // (such as debug tracing code) attempts to disable and re-enable
            // preemption during the scheduling operation.
            self.preempts_pending.fetch_and(!current_cpu_mask);

            // TODO(johngro): We cannot be holding the current thread's lock while we do
            // this.  Is there a good way to enforce this requirement via either static
            // annotation or dynamic checks?
            //
            // `scheduler::preempt` requires that no `ChainLockTransaction` is active on this CPU
            // (`TA_EXCL(chainlock_transaction_token)` in C++).  Like the C++ `DEBUG_ASSERT` via
            // `ChainLockTransaction::AssertNoTransactionActive()`, this is a debug-only check:
            // `scheduler::preempt` enters `single_chain_lock_guard!`, whose
            // `ChainLockTransaction::register` asserts the same condition in release builds.
            crate::kconcurrent::debug_assert_no_transaction_active();
            // SAFETY: No `ChainLockTransaction` is active on the current CPU (debug-asserted
            // above, and asserted unconditionally by `ChainLockTransaction::register` inside
            // `scheduler::preempt`'s `single_chain_lock_guard!`), which is `scheduler::preempt`'s
            // only precondition. `self` is the *current* thread's preemption state (the caller's
            // contract), and the current thread outlives its own run, so `&self` stays valid
            // across the resulting context switch; `PreemptionState` is all relaxed atomics, so
            // the scheduler's and interrupt handlers' writes through it meanwhile are sound.
            unsafe {
                crate::kernel::scheduler::preempt(
                    crate::kernel::scheduler::PreemptType::FlushPending,
                );
            }
        } else if flush.contains(Flush::REMOTE) {
            // The current cpu is ignored by mp_reschedule if present in the mask.
            crate::kernel::mp::reschedule(pending_mask, 0);
            self.preempts_pending.fetch_and(current_cpu_mask);
        }
    }
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<PreemptionState>() == size_of::<layout::PreemptionStateStorage>());
static_assert!(align_of::<PreemptionState>() == align_of::<layout::PreemptionStateStorage>());
static_assert!(offset_of!(PreemptionState, state) == layout::PreemptionStateLayout_kStateOffset);
static_assert!(
    offset_of!(PreemptionState, preempts_pending)
        == layout::PreemptionStateLayout_kPreemptsPendingOffset
);
static_assert!(
    offset_of!(PreemptionState, timeslice_extension)
        == layout::PreemptionStateLayout_kTimesliceExtensionOffset
);
static_assert!(
    offset_of!(PreemptionState, timeslice_extension_deadline)
        == layout::PreemptionStateLayout_kTimesliceExtensionDeadlineOffset
);

/// TaskState is responsible for running the task defined by
/// |entry(arg)|, and reporting its value to any joining threads.
///
/// TODO: the detached state in Thread::flags_ probably belongs here.
#[repr(C)]
pub struct TaskState {
    // Safety invariant: written once by `rust_task_state_init` before the owning `Thread` is
    // published; never mutated afterwards.
    /// The Thread's entry point, and its argument.
    entry: Option<extern "C" fn(*mut c_void) -> i32>,
    arg: *mut c_void,

    // Safety invariant: written by the exiting thread under its own lock (`Thread::Current::Exit`)
    // and read by a joiner under the same lock after observing `THREAD_DEATH` (`Thread::Join`), so
    // the thread's lock orders the two; see `retcode`.
    /// Storage for the return code.
    ///
    /// TODO: This is logically protected by our thread's lock.  What is a
    /// good way to represent this with a static annotation?
    retcode: UnsafeCell<i32>,

    /// Other threads waiting to join this Thread.
    retcode_wait_queue: WaitQueue,
}

impl TaskState {
    /// The thread's entry point; `None` for threads driven by an alternate trampoline, which
    /// `CreateEtc` allows to be created without one.
    #[inline]
    pub(crate) fn entry(&self) -> Option<extern "C" fn(*mut c_void) -> i32> {
        self.entry
    }

    /// The argument passed to the entry point.
    #[inline]
    pub(crate) fn arg(&self) -> *mut c_void {
        self.arg
    }

    /// The thread's return code.
    ///
    /// # Safety
    ///
    /// The caller must exclude a concurrent write by the exiting thread (`Thread::Current::Exit`):
    /// either it holds the owning thread's lock (the return code is written under that lock, and
    /// `Thread::Join` reads it under the same lock after observing `THREAD_DEATH`), or the thread
    /// has not started running yet.
    #[inline]
    pub unsafe fn retcode(&self) -> i32 {
        // SAFETY: The caller excludes concurrent writers; no reference is formed.
        unsafe { self.retcode.get().read() }
    }

    /// Records the thread's entry point and argument (`TaskState::Init`).
    ///
    /// Called from `CreateEtc` on a fully constructed but not yet published `Thread`, so the
    /// fields are written through the raw pointer and no `&TaskState` is formed.
    ///
    /// # Safety
    ///
    /// `this` points to a valid `TaskState` not yet visible to any other thread, with no live Rust
    /// reference to it.
    #[inline]
    pub(crate) unsafe fn init(
        this: *mut Self,
        entry: Option<extern "C" fn(*mut c_void) -> i32>,
        arg: *mut c_void,
    ) {
        // SAFETY: The caller guarantees the pointer is valid for writes with no live Rust
        // reference; projecting the fields through the raw pointer forms none.
        unsafe {
            (&raw mut (*this).entry).write(entry);
            (&raw mut (*this).arg).write(arg);
        }
    }

    /// Blocks `current_thread` in this thread's `retcode_wait_queue` until that thread exits or
    /// `deadline` passes.
    ///
    /// The queue's lock is released by this call because the C++ is `TA_REL(get_lock())`;
    /// `current_thread`'s lock stays held on return (`TA_REQ`).
    ///
    /// There is deliberately no `&self`: the target thread (and so its `TaskState`) may be torn
    /// down while the joiner is blocked (`Detach` wakes joiners with `BAD_STATE` and a detached
    /// target then exits and frees itself, possibly before the woken joiner runs), so no reference
    /// into it may be live across `scheduler::block`. The `&WaitQueue` and its [`OwnerGuard`] are
    /// scoped to [`WaitQueue::block_etc_preamble`], which releases the queue lock before the
    /// thread blocks, and [`WaitQueueBase::block_etc_postamble`] dereferences the returned
    /// `base_ptr` only once, before `scheduler::block`.
    ///
    /// # Safety
    ///
    /// - `this` points to a valid `TaskState` at the time of the call, and its
    ///   `retcode_wait_queue` remains valid while `current_thread` remains blocked in it (and at
    ///   least until both the queue lock and `current_thread`'s lock are released during the block
    ///   operation).
    /// - `current_thread.owner()` is the currently executing thread in `THREAD_RUNNING`, not queued
    ///   in any wait queue, in a context where blocking is permitted (no spinlocks held, blocking
    ///   not disallowed).
    /// - The active transaction is finalized with `(*this).retcode_wait_queue`'s lock and
    ///   `current_thread`'s lock as the only held chain locks, with no other guard or proof for
    ///   the queue lock live; this call takes over releasing the queue lock.
    pub unsafe fn join(
        this: *const Self,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: InstantMono,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        let deadline = Deadline::no_slack(deadline.into());
        // The `&WaitQueue` and its guard live only inside this block: `block_etc_preamble`
        // releases the queue lock before the thread blocks, and only the raw `base_ptr` is passed
        // to `block_etc_postamble`.
        let base_ptr = {
            // SAFETY: `this` is valid at the time of the call per the caller; projecting the field
            // forms no reference to the `TaskState` itself, and the `&WaitQueue` does not outlive
            // this block.
            let queue = unsafe { &(*this).retcode_wait_queue };
            // SAFETY: The queue's lock is held by the active transaction with no other guard or
            // proof (precondition), and this guard takes over releasing it.
            let guard = unsafe { OwnerGuard::adopt_unchecked(queue, clt_token) };
            // SAFETY: `current_thread` is the running thread, not queued, in a context where
            // blocking is permitted; `deadline` is monotonic; the active transaction is finalized
            // with `guard`'s and `current_thread`'s locks as the only held chain locks; and the
            // queue remains valid while `current_thread` is blocked in it.
            unsafe {
                WaitQueue::block_etc_preamble(
                    guard,
                    current_thread,
                    &deadline,
                    0,
                    ResourceOwnership::Normal,
                    Interruptible::No,
                    clt_token,
                )?
            }
        };
        // SAFETY: `block_etc_preamble` returned `Ok(base_ptr)` with `current_thread` inserted in
        // `base_ptr` and the queue lock released; `base_ptr` remains valid while `current_thread`
        // is blocked in it; and `current_thread`'s lock is the only held chain lock in the
        // finalized transaction.
        unsafe {
            WaitQueueBase::block_etc_postamble(
                base_ptr,
                current_thread.reborrow(),
                &deadline,
                clt_token,
            )
        }
    }

    /// Attempt to wake all of our joiners.  This operation might fail because of a
    /// conflict while attempting to obtain the required locks, in which case the
    /// caller needs to back off (drop all currently held chain locks) and try
    /// again.
    ///
    /// `Thread::Current::Exit` restarts its finalized transaction before calling this in a loop
    /// with its own lock held. On `true`, `wake_all_locked` finalizes the transaction and
    /// `guard.release()` drops the queue lock (`ChainLock::release` does not require an
    /// unfinalized transaction); on `false`, either `acquire_or_backoff` or `wake_all_locked`
    /// backed off without finalizing.
    ///
    /// # Panics
    ///
    /// Debug-asserts (via `acquire_or_backoff` and `wake_all_locked`) that the active
    /// transaction is not already finalized on entry.
    pub fn try_wake_joiners(
        &self,
        status: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> bool {
        // Attempt to lock the queue first.
        let Some(mut guard) = OwnerGuard::acquire_or_backoff(&self.retcode_wait_queue, clt_token)
        else {
            core::hint::cold_path();
            return false;
        };

        // Now, try to lock and wake all of our waiting threads.
        let result = guard.proof().wake_all_locked(status, clt_token, preempt_disabled_token);

        // No matter what just happened, we need to drop the queue's lock.
        guard.release();

        // If we got a result, then we succeeded in waking the threads we wanted to
        // wake.
        result.is_some()
    }
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<TaskState>() == size_of::<layout::TaskStateStorage>());
static_assert!(align_of::<TaskState>() == align_of::<layout::TaskStateStorage>());
static_assert!(offset_of!(TaskState, entry) == layout::TaskStateLayout_kEntryOffset);
static_assert!(offset_of!(TaskState, arg) == layout::TaskStateLayout_kArgOffset);
static_assert!(offset_of!(TaskState, retcode) == layout::TaskStateLayout_kRetcodeOffset);
static_assert!(
    offset_of!(TaskState, retcode_wait_queue) == layout::TaskStateLayout_kRetcodeWaitQueueOffset
);

/// Keeps track of whether a thread is allowed to allocate memory.
///
/// A thread's |MemoryAllocationState| should only be accessed by that thread itself or interrupt
/// handlers running in the thread's context.
#[repr(C)]
pub struct MemoryAllocationState {
    /// Notice that we aren't using atomic operations to access the field.  We don't need atomic
    /// operations here as long as...
    ///
    /// 1. We use atomic_signal_fence to prevent compiler reordering.
    ///
    /// 2. We use volatile to ensure the compiler actually generates loads and stores for the value
    ///    (so the interrupt handler can see what the thread see, and vice versa).
    ///
    /// 3. Upon completion, an interrupt handler that modified the field restores it to the value
    ///    it held at the start of the interrupt.
    // Accessed only by the owning thread and its interrupt handlers.
    disable_count: UnsafeCell<u32>,
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(
    size_of::<MemoryAllocationState>() == size_of::<layout::MemoryAllocationStateStorage>()
);
static_assert!(
    align_of::<MemoryAllocationState>() == align_of::<layout::MemoryAllocationStateStorage>()
);
static_assert!(
    offset_of!(MemoryAllocationState, disable_count)
        == layout::MemoryAllocationStateLayout_kDisableCountOffset
);

/// A Zircon kernel thread.
///
/// Its layout is shared with `<kernel/thread.h>` and checked against `<kernel/thread_layout.h>`.
/// Rust never constructs, moves, or drops a `Thread` by value, and only ever holds `&Thread` or
/// raw pointers to one. Members Rust does not access directly are `zr::Opaque` storage of the
/// corresponding size and alignment.
///
/// `KCell<_, ThreadLockClass>` marks members guarded by the thread's `ChainLock`; they are
/// reached only through accessors on `ChainLockProof<'_, Thread>`. Comments on the remaining
/// interior-mutable members state their synchronization rules.
///
/// # Safety Invariant
///
/// Any `&Thread` or `&mut Thread` reference must point to a valid, initialized `Thread`.
// LINT.IfChange(ThreadLayout)
#[repr(C)]
#[derive(DoublyLinkedListContainable, SinglyLinkedListContainable)]
pub struct Thread {
    // The `ChainLockable` base: the lock guarding the `KCell<_, ThreadLockClass>` members.
    // `ClassedChainLock` is `repr(transparent)` over `ChainLock`, so a plain `ChainLock` sits at
    // offset 0.
    lock: ClassedChainLock<ThreadLockClass>,

    canary: fbl::Canary<{ fbl::magic(b"thrd") }>,

    // These fields are among the most active in the thread. They are grouped
    // together near the front to improve cache locality.
    //
    // `scheduler_variable_lock_` (a zero-sized `fbl::NullLock`) has no storage and is omitted.
    //
    // Relaxed atomic. Each flag is an independent bit: the owning thread mutates
    // `THREAD_FLAG_RESTRICTED_KICK_PENDING` lock-free, `THREAD_FLAG_IDLE`/`THREAD_FLAG_VCPU` are
    // set during setup, and `THREAD_FLAG_DETACHED`/`THREAD_FLAG_FREE_STRUCT` are set at creation
    // or under `get_lock()`.
    flags: RelaxedAtomicU32,
    signals: RelaxedAtomicU32,
    // Guard: split (see `scheduler::SchedulerState`). `state`, `base_profile`,
    // `inherited_profile_values` and the `effective_profile` dirty flags are written under
    // `get_lock()`; the queue-related members (`start_time`, `finish_time`, time slices,
    // `activation_count`, the run-queue node, `subtree_invariants`) under the owning scheduler's
    // `queue_lock_`; `curr_cpu`/`last_cpu` are read lock-free. Read through
    // `ChainLockProof::<Thread>::scheduler_state`.
    scheduler_state: Opaque<layout::ThreadSchedulerStateStorage>,
    // Guard: `scheduler_variable_lock_` (a `fbl::NullLock`), i.e. the scheduler's own discipline.
    scheduler_queue_state: Opaque<layout::ThreadSchedulerQueueStateStorage>,
    // Guard: `get_lock()`; each member of `ThreadState` carries its own cell type.
    pub(super) wait_queue_state: ThreadState,
    owned_wait_queues: KCell<OwnedWaitQueueList, ThreadLockClass>,
    task_state: TaskState,
    preemption_state: PreemptionState,
    memory_allocation_state: MemoryAllocationState,

    /// Must only be accessed by "this" thread.
    // Owned `RestrictedState` pointer; reached through `current_restricted_state`.
    restricted_state: UnsafeCell<*mut c_void>,

    /// state for runtime lock validation when in thread context
    #[cfg(with_lock_dep)]
    lock_state: Opaque<layout::ThreadLockStateStorage>,

    /// The current address space this thread is associated with.
    /// This can be null if this is a kernel thread.
    /// See the comments near active_aspace() for more details.
    aspace: AtomicPtr<VmAspace>,

    // Guard: the owning thread (set and cleared around a ktrace test).
    #[cfg(ktest)]
    rust_ktrace_override: UnsafeCell<*mut c_void>,

    /// Saved by SignalPolicyException() to store the type of policy error, and
    /// passed to exception dispatch in ProcessPendingSignals().
    extra_policy_exception_code: KCell<u32, ThreadLockClass>,
    extra_policy_exception_data: KCell<u32, ThreadLockClass>,

    /// Is this thread allowed to own wait queues?  Set to false by a thread as
    /// it exits, but before it reaches the DEAD state.
    can_own_wait_queues: KCell<bool, ThreadLockClass>,

    /// Strong reference to user thread if one exists for this thread.
    /// In the common case freeing Thread will also free ThreadDispatcher when this
    /// reference is dropped.
    // `RefPtr<ThreadDispatcher>` storage; set once during user-thread setup and read lock-free
    // afterwards.
    user_thread: UnsafeCell<*mut c_void>,

    /// When user_thread_ is set, these values are copied from ThreadDispatcher and
    /// its parent ProcessDispatcher. Kernel threads maintain an independent tid.
    // Written during thread setup (or `SetUserThread` before the thread runs) and read lock-free
    // with relaxed ordering.
    tid: RelaxedAtomicU64,
    pid: RelaxedAtomicU64,

    /// Architecture-specific stuff.
    arch: Opaque<layout::ThreadArchStorage>,

    stack: Opaque<layout::ThreadStackStorage>,

    /// This is used by dispatcher.cc:SafeDeleter.
    // Guard: the owning thread.
    recursive_object_deletion_list: UnsafeCell<*mut c_void>,

    /// This always includes the trailing NUL.
    // NUL-terminated name buffer, byte-wise relaxed atomics: `set_name` (and C++
    // `Thread::Current::SetName`, which forwards to it) writes it without a lock and readers
    // (`name_bytes`, `name_bytes_ptr`, C++ `name()`) copy or read it lock-free, tolerating a torn
    // name. Same layout as `char[ZX_MAX_NAME_LEN]`.
    name: [AtomicU8; ZX_MAX_NAME_LEN],

    /// Buffering for Debuglog output.
    linebuffer: Opaque<layout::ThreadLinebufferStorage>,

    /// The flow id allocated before blocking on the last lock.
    lock_flow_id: RelaxedAtomicU64,

    /// Indicates whether user register state (debug, vector, fp regs, etc.) has been saved to the
    /// arch_thread_t as part of thread suspension / exception handling.
    ///
    /// When a user thread is suspended or generates an exception (synthetic or architectural) that
    /// might be observed by another process, we save user register state to the thread's
    /// arch_thread_t so that it may be accessed by a debugger.  Upon leaving a suspended or
    /// exception state, we restore user register state.
    user_state_saved: KCell<bool, ThreadLockClass>,

    /// For threads with migration functions, indicates whether a migration is in progress. When
    /// true, the migrate function has been called with Before but not yet with After.
    ///
    /// TODO: What to do about this?  Should it be protected by the
    /// thread's lock?  Does the thread's lock need to be held exclusively when a
    /// thread's migrate function is called?
    migrate_pending: KCell<bool, ThreadLockClass>,

    /// Provides a way to execute custom logic when a thread must be migrated between CPUs.
    // Guard: `get_lock()`.
    migrate_fn: Opaque<layout::ThreadMigrateFnStorage>,

    /// Provides a way to execute custom logic when a thread is context switched to or away from.
    context_switch_fn: Opaque<layout::ThreadContextSwitchFnStorage>,

    /// An Accessor for the thread's restartable sequence structure, if any.
    ///
    /// This is used to detect when a thread is preempted or enters a waiting state
    /// while in a restartable sequence.  When this happens, the restartable sequence
    /// is interrupted, and the thread will restart the sequence when it resumes.
    ///
    /// Kernel threads do not use restartable sequences, so this will always be !IsValid() for
    /// kernel threads.
    ///
    /// Must be accessed only by the current thread.
    rseq_accessor: Opaque<layout::ThreadRseqAccessorStorage>,

    /// The sampling session this thread is associated with.
    sampling_session: RelaxedAtomicU64,

    /// Used to track threads that have set `migrate_fn`. This is used to migrate
    /// threads before a CPU is taken offline.
    // Guard: `Thread::list_lock_`.
    #[dll_node(tag = MigrateListTag)]
    migrate_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the global thread list.
    // Guard: `Thread::list_lock_`.
    #[dll_node(tag = ThreadListTag)]
    thread_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the temporary batch unblock list.
    // Guard: `get_lock()`; the node's own `UnsafeCell`s stand in for a `KCell`, which the list
    // traits cannot see through.
    #[dll_node(tag = UnblockListTag)]
    unblock_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the save state list.
    // Guard: `get_lock()`; as for `unblock_list_node`.
    #[sll_node(tag = SaveStateListTag)]
    save_state_list_node: SinglyLinkedListNode<Thread>,

    /// State of the thread with respect to memory stalls.
    ///
    /// It can only be accessed by the thread's current CPU. Hence, preemption must
    /// be disabled while accessing it.
    // Guard: `preempt_disabled_token`; see `memory_stall_state` / `set_memory_stall_state` above.
    memory_stall_state: UnsafeCell<ThreadStallState>,
}
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:ThreadLayout,
//   //zircon/kernel/include/kernel/thread_layout.h:ThreadLayout,
// )

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<Thread>() == size_of::<layout::ThreadStorage>());
static_assert!(align_of::<Thread>() == align_of::<layout::ThreadStorage>());
static_assert!(offset_of!(Thread, lock) == 0);
static_assert!(offset_of!(Thread, lock) == layout::ThreadLayout_kLockOffset);
static_assert!(offset_of!(Thread, canary) == layout::ThreadLayout_kCanaryOffset);
static_assert!(offset_of!(Thread, flags) == layout::ThreadLayout_kFlagsOffset);
static_assert!(offset_of!(Thread, signals) == layout::ThreadLayout_kSignalsOffset);
static_assert!(offset_of!(Thread, scheduler_state) == layout::ThreadLayout_kSchedulerStateOffset);
static_assert!(
    size_of::<scheduler::SchedulerState>() == size_of::<layout::ThreadSchedulerStateStorage>()
);
static_assert!(
    align_of::<scheduler::SchedulerState>() == align_of::<layout::ThreadSchedulerStateStorage>()
);
static_assert!(
    offset_of!(Thread, scheduler_queue_state) == layout::ThreadLayout_kSchedulerQueueStateOffset
);
static_assert!(offset_of!(Thread, wait_queue_state) == layout::ThreadLayout_kWaitQueueStateOffset);
static_assert!(
    offset_of!(Thread, owned_wait_queues) == layout::ThreadLayout_kOwnedWaitQueuesOffset
);
static_assert!(offset_of!(Thread, task_state) == layout::ThreadLayout_kTaskStateOffset);
static_assert!(offset_of!(Thread, preemption_state) == layout::ThreadLayout_kPreemptionStateOffset);
static_assert!(
    offset_of!(Thread, memory_allocation_state)
        == layout::ThreadLayout_kMemoryAllocationStateOffset
);
static_assert!(offset_of!(Thread, restricted_state) == layout::ThreadLayout_kRestrictedStateOffset);
#[cfg(with_lock_dep)]
static_assert!(offset_of!(Thread, lock_state) == layout::ThreadLayout_kLockStateOffset);
static_assert!(offset_of!(Thread, aspace) == layout::ThreadLayout_kAspaceOffset);
#[cfg(ktest)]
static_assert!(
    offset_of!(Thread, rust_ktrace_override) == layout::ThreadLayout_kRustKtraceOverrideOffset
);
static_assert!(
    offset_of!(Thread, extra_policy_exception_code)
        == layout::ThreadLayout_kExtraPolicyExceptionCodeOffset
);
static_assert!(
    offset_of!(Thread, extra_policy_exception_data)
        == layout::ThreadLayout_kExtraPolicyExceptionDataOffset
);
static_assert!(
    offset_of!(Thread, can_own_wait_queues) == layout::ThreadLayout_kCanOwnWaitQueuesOffset
);
static_assert!(offset_of!(Thread, user_thread) == layout::ThreadLayout_kUserThreadOffset);
static_assert!(offset_of!(Thread, tid) == layout::ThreadLayout_kTidOffset);
static_assert!(offset_of!(Thread, pid) == layout::ThreadLayout_kPidOffset);
static_assert!(offset_of!(Thread, arch) == layout::ThreadLayout_kArchOffset);
static_assert!(offset_of!(Thread, stack) == layout::ThreadLayout_kStackOffset);
static_assert!(
    offset_of!(Thread, recursive_object_deletion_list)
        == layout::ThreadLayout_kRecursiveObjectDeletionListOffset
);
static_assert!(offset_of!(Thread, name) == layout::ThreadLayout_kNameOffset);
static_assert!(size_of::<[AtomicU8; ZX_MAX_NAME_LEN]>() == size_of::<[c_char; ZX_MAX_NAME_LEN]>());
static_assert!(
    align_of::<[AtomicU8; ZX_MAX_NAME_LEN]>() == align_of::<[c_char; ZX_MAX_NAME_LEN]>()
);
static_assert!(offset_of!(Thread, linebuffer) == layout::ThreadLayout_kLinebufferOffset);
static_assert!(offset_of!(Thread, lock_flow_id) == layout::ThreadLayout_kLockFlowIdOffset);
static_assert!(offset_of!(Thread, user_state_saved) == layout::ThreadLayout_kUserStateSavedOffset);
static_assert!(offset_of!(Thread, migrate_pending) == layout::ThreadLayout_kMigratePendingOffset);
static_assert!(offset_of!(Thread, migrate_fn) == layout::ThreadLayout_kMigrateFnOffset);
static_assert!(
    offset_of!(Thread, context_switch_fn) == layout::ThreadLayout_kContextSwitchFnOffset
);
static_assert!(offset_of!(Thread, rseq_accessor) == layout::ThreadLayout_kRseqAccessorOffset);
static_assert!(offset_of!(Thread, sampling_session) == layout::ThreadLayout_kSamplingSessionOffset);
static_assert!(
    offset_of!(Thread, migrate_list_node) == layout::ThreadLayout_kMigrateListNodeOffset
);
static_assert!(offset_of!(Thread, thread_list_node) == layout::ThreadLayout_kThreadListNodeOffset);
static_assert!(
    offset_of!(Thread, unblock_list_node) == layout::ThreadLayout_kUnblockListNodeOffset
);
static_assert!(
    offset_of!(Thread, save_state_list_node) == layout::ThreadLayout_kSaveStateListNodeOffset
);
static_assert!(
    offset_of!(Thread, memory_stall_state) == layout::ThreadLayout_kMemoryStallStateOffset
);

// SAFETY: Every interior mutation Rust performs through `&Thread` is either on an atomic, on a
// `KCell<_, ThreadLockClass>` reached through a `ChainLockProof<'_, Thread>` (which proves `lock`
// is held), or on `wait_queue_state`'s tree bookkeeping from WAVL tree hooks that run under the
// containing wait queue's lock. The remaining `UnsafeCell` and `Opaque` members are accessed only
// under the synchronization rules on each field.
unsafe impl Sync for Thread {}

// SAFETY: `lock` guards every `KCell<_, ThreadLockClass>` member of `Thread` (including those of
// `wait_queue_state`), and is a plain field so the same instance is returned every time.
unsafe impl ChainLocked for Thread {
    type Class = ThreadLockClass;

    /// Returns the lock guarding this thread.
    fn lock(&self) -> &ClassedChainLock<ThreadLockClass> {
        &self.lock
    }
}

/// Lock class for `Thread::list_lock_`.
///
/// `Thread::list_lock_` is constructed in C++ and only accessed via a `&KSpinlock` facade, so
/// `LockClass::ID` is never used to construct a Rust spinlock.
pub enum ThreadListLockClass {}

impl ksync::LockClass for ThreadListLockClass {
    const ID: *mut c_void = core::ptr::null_mut();
}

impl Thread {
    /// The sampling session value of a thread not associated with any session.
    pub const INVALID_SAMPLING_SESSION: u64 = u64::MAX;

    /// Returns a reference to the thread's `TaskState`.
    #[inline]
    pub(crate) fn task_state(&self) -> &TaskState {
        &self.task_state
    }

    /// Returns a reference to the thread's `PreemptionState`.
    #[inline]
    pub(crate) fn preemption_state(&self) -> &PreemptionState {
        &self.preemption_state
    }

    /// Domain-specific conversion: returns raw pointer for [`Thread`].
    #[inline]
    pub fn as_raw(&self) -> *mut Self {
        core::ptr::from_ref(self).cast_mut()
    }

    /// Raw place of the [`ClassedChainLock`] embedded in `this`, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn lock_ptr(this: *const Self) -> *const ClassedChainLock<ThreadLockClass> {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `lock` field place without forming a reference to the whole `Thread`.
        unsafe { &raw const (*this).lock }
    }

    /// Asserts that `self`'s canary magic value is valid.
    #[inline]
    pub fn assert_canary(&self) {
        self.canary.assert();
    }

    /// Like [`Self::assert_canary`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn assert_canary_ptr(this: *const Self) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `canary` field place without forming a reference to the whole `Thread`.
        let canary = unsafe { &raw const (*this).canary };
        // SAFETY: `canary` is the live `Thread`'s canary field (see above).
        unsafe { (*canary).assert() };
    }

    /// Like [`Self::is_idle`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn is_idle_ptr(this: *const Self) -> bool {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `flags` field place without forming a reference to the whole `Thread`.
        let flags = unsafe { &raw const (*this).flags };
        // SAFETY: `flags` is the live `Thread`'s flags field (see above).
        unsafe { (*flags).load() & THREAD_FLAG_IDLE != 0 }
    }

    /// Like [`Self::preemption_state`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn preemption_state_ptr(this: *const Self) -> *const PreemptionState {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `preemption_state` field place without forming a reference to the whole `Thread`.
        unsafe { &raw const (*this).preemption_state }
    }

    /// Returns the koid of the associated `ThreadDispatcher` for user threads or an independent
    /// koid for kernel threads.
    #[inline]
    pub fn tid(&self) -> zx_koid_t {
        self.tid.load()
    }

    /// Like [`Self::tid`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(crate) unsafe fn tid_ptr(this: *const Self) -> zx_koid_t {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `tid` field place without forming a reference to the whole `Thread`.
        let tid = unsafe { &raw const (*this).tid };
        // SAFETY: `tid` is the live `Thread`'s `tid` field (see above).
        unsafe { (*tid).load() }
    }

    /// Returns the koid of the associated `ProcessDispatcher` for user threads or
    /// `ZX_KOID_INVALID` for kernel threads.
    #[inline]
    pub fn pid(&self) -> zx_koid_t {
        self.pid.load()
    }

    /// Like [`Self::pid`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(crate) unsafe fn pid_ptr(this: *const Self) -> zx_koid_t {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `pid` field place without forming a reference to the whole `Thread`.
        let pid = unsafe { &raw const (*this).pid };
        // SAFETY: `pid` is the live `Thread`'s `pid` field (see above).
        unsafe { (*pid).load() }
    }

    /// Checks whether the kill or suspend signal has been raised.
    ///
    /// Returns `Err(ZX_ERR_INTERNAL_INTR_KILLED)` if kill has been raised,
    /// `Err(ZX_ERR_INTERNAL_INTR_RETRY)` if suspend has been raised, and `Ok(())` otherwise.
    #[inline]
    pub fn check_kill_or_suspend_signal(&self) -> Result<(), Status> {
        let current_signals = self.signals();
        if current_signals & THREAD_SIGNAL_KILL != 0 {
            core::hint::cold_path();
            return Err(ZX_ERR_INTERNAL_INTR_KILLED);
        }
        if current_signals & THREAD_SIGNAL_SUSPEND != 0 {
            core::hint::cold_path();
            return Err(ZX_ERR_INTERNAL_INTR_RETRY);
        }
        Ok(())
    }

    // Accessors for specific flags bits.
    /// Whether the thread is detached.
    #[inline]
    pub fn detached(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_DETACHED) != 0
    }
    /// Sets or clears the detached flag.
    ///
    /// # Safety
    ///
    /// Callers must preserve the detached-vs-joinable lifecycle invariant: a joined or being-joined
    /// thread must not be marked detached, and a detached thread's exit path assumes it will not be
    /// joined.
    #[inline]
    pub(crate) unsafe fn set_detached(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_DETACHED);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_DETACHED);
        }
    }
    /// Whether the `Thread` struct is freed when the thread exits.
    #[inline]
    pub fn free_struct(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_FREE_STRUCT) != 0
    }
    /// Sets or clears the free-struct flag.
    ///
    /// # Safety
    ///
    /// Callers must ensure this matches how the `Thread` storage was allocated (`new` vs
    /// caller-owned storage) and is only changed before the thread's exit path inspects it.
    #[inline]
    pub(crate) unsafe fn set_free_struct(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_FREE_STRUCT);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_FREE_STRUCT);
        }
    }
    /// Whether this is an idle thread; read lock-free by the scheduler.
    #[inline]
    pub fn is_idle(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_IDLE) != 0
    }
    /// Sets or clears the idle flag.
    ///
    /// # Safety
    ///
    /// Callers must only set this on a CPU's dedicated per-CPU idle thread during initialization,
    /// as the scheduler relies on `is_idle()` to identify the idle thread.
    #[inline]
    pub(crate) unsafe fn set_idle(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_IDLE);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_IDLE);
        }
    }
    /// Whether this thread runs a VCPU.
    #[inline]
    pub fn vcpu(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_VCPU) != 0
    }
    /// Sets or clears the VCPU flag.
    #[inline]
    pub(crate) fn set_vcpu(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_VCPU);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_VCPU);
        }
    }

    /// Access to the entire flags value, for diagnostics.
    #[inline]
    pub fn flags(&self) -> u32 {
        self.flags.load()
    }

    // TODO(https://fxbug.dev/42077109): Write down memory order requirements for accessing
    // signals.
    /// Returns the thread's pending signals.
    ///
    /// Uses `Relaxed` ordering: signal bitmask reads are polled opportunistically or re-checked
    /// under the thread's lock, and do not synchronize other memory accesses.
    #[inline]
    pub fn signals(&self) -> u32 {
        self.signals.load()
    }
    /// Raises `bits` in the thread's pending signals.
    #[inline]
    pub(crate) fn set_signals(&self, bits: u32) {
        self.signals.fetch_or(bits);
    }
    /// Clears `bits` from the thread's pending signals.
    #[inline]
    pub(crate) fn clear_signals(&self, bits: u32) {
        self.signals.fetch_and(!bits);
    }

    /// Copies the name buffer (always NUL-terminated) into `out`.
    #[inline]
    pub fn name_bytes(&self, out: &mut [u8; ZX_MAX_NAME_LEN]) {
        // Like the C++ lock-free readers (ktrace, `DumpLocked`), the copy may observe a
        // concurrent `set_name` of another thread renaming itself; byte-wise relaxed loads make
        // that a torn name rather than a data race.
        for i in 0..ZX_MAX_NAME_LEN {
            out[i] = self.name[i].load(Ordering::Relaxed);
        }
    }

    /// Like [`Self::name_bytes`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn name_bytes_ptr(this: *const Self, out: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `name` field place without forming a reference to the whole `Thread`.
        let name = unsafe { &raw const (*this).name };
        for (i, byte) in out.iter_mut().enumerate() {
            // SAFETY: `name` is the live `Thread`'s `[AtomicU8; ZX_MAX_NAME_LEN]` field (see
            // above).
            *byte = unsafe { (*name)[i].load(Ordering::Relaxed) };
        }
    }

    /// Returns a snapshot of the thread's name.
    #[inline]
    pub fn name(&self) -> ThreadName {
        // SAFETY: `self` is a valid, live `Thread`.
        unsafe { Self::name_copy_ptr(core::ptr::from_ref(self)) }
    }

    /// Like [`Self::name`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn name_copy_ptr(this: *const Self) -> ThreadName {
        let mut res = ThreadName::empty();
        // SAFETY: `this` points to a valid, live `Thread` per the caller.
        unsafe { Self::name_bytes_ptr(this, &mut res.buf) };
        let mut len = 0;
        while len < ZX_MAX_NAME_LEN && res.buf[len] != 0 {
            len += 1;
        }
        res.len = len;
        res
    }

    /// Writes `name` into `dst`, truncated so that it (including a trailing NUL byte) fits in
    /// `ZX_MAX_NAME_LEN` bytes, and NUL-fills the remainder.
    fn write_name_bytes(dst: &[AtomicU8; ZX_MAX_NAME_LEN], name: &[u8]) {
        let len = core::cmp::min(name.len(), ZX_MAX_NAME_LEN - 1);
        for i in 0..ZX_MAX_NAME_LEN {
            let byte = if i < len { name[i] } else { 0 };
            dst[i].store(byte, Ordering::Relaxed);
        }
    }

    /// Sets the thread's name to `name`.
    ///
    /// This may truncate `name`, so that it (including a trailing NUL byte) fits in
    /// `ZX_MAX_NAME_LEN` bytes. Writes to `name` are unsynchronized: concurrent `set_name` calls
    /// may interleave, and readers (`name_bytes`, C++ `name()`) tolerate a torn name.
    pub fn set_name(&self, name: &[u8]) {
        Self::write_name_bytes(&self.name, name);
    }

    /// Sets the thread's name without creating a `&Thread` reference, so that it can be used on
    /// a `Thread` that is still under construction.
    ///
    /// # Safety
    ///
    /// `thread` must point to allocated, suitably aligned `Thread` storage whose `name` bytes are
    /// initialized (it need not be a fully constructed `Thread`), with no concurrent non-atomic
    /// access to `name`.
    unsafe fn set_name_in_place(thread: *mut Self, name: &[u8]) {
        // SAFETY: `&raw const` projects the `name` place without forming a reference to the
        // surrounding (possibly uninitialized) `Thread`; the caller guarantees `name` is
        // initialized and only accessed atomically.
        let dst = unsafe { &raw const (*thread).name };
        // SAFETY: `dst` points to the initialized `[AtomicU8; ZX_MAX_NAME_LEN]` field (see above).
        Self::write_name_bytes(unsafe { &*dst }, name);
    }

    /// Raw read of the thread's scheduling state without its lock.
    ///
    /// # Safety
    ///
    /// The thread must currently be a member of a wait queue whose lock the caller holds: a queued
    /// thread's state is set under both locks when it enqueues and only changes again after it
    /// has been removed under the queue's lock, so the queue lock alone makes this read race-free.
    #[inline]
    pub(super) unsafe fn state_in_wait_queue(&self) -> ThreadStateKind {
        let p = self.scheduler_state.get().cast::<scheduler::SchedulerState>();
        // SAFETY: `p` points at the live storage of the thread's `scheduler::SchedulerState`;
        // only the `state` place is read (the caller excludes concurrent writers of it) and no
        // reference to the surrounding struct is formed.
        unsafe { (&raw const (*p).state).read() }
    }

    /// Returns the [`SchedulerState`] facade over this thread's `scheduler_state` member.
    ///
    /// # Safety
    ///
    /// The caller must hold this thread's lock for the lifetime of the returned reference, or
    /// otherwise exclude concurrent mutation of `scheduler_state` (e.g. the WAVL tree hooks of a
    /// wait queue, which run under that queue's lock on threads blocked in it).
    #[inline]
    pub(super) unsafe fn scheduler_state_unchecked(&self) -> &SchedulerState {
        // SAFETY: `scheduler_state` is the storage of the thread's `SchedulerState`, which is
        // live for as long as `self` is, and `SchedulerState` is a zero-sized facade over it. The
        // caller excludes concurrent mutation.
        unsafe { &*self.scheduler_state.get().cast::<SchedulerState>() }
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
    pub fn dump_tid(tid: zx_koid_t, full: bool) {
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
    pub unsafe fn dump_tid_during_panic(tid: zx_koid_t, full: bool) {
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
pub unsafe fn thread_id_to_thread_slow(tid: zx_koid_t) -> Option<ThreadPtr> {
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
    /// Returns a shared reference to this thread's [`SchedulerState`].
    #[inline]
    pub fn scheduler_state(&self) -> &SchedulerState {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `scheduler_state`.
        unsafe { self.owner().scheduler_state_unchecked() }
    }

    /// Raw pointer to the [`scheduler::SchedulerState`] storage of this thread.
    ///
    /// Never dereferenced into a reference to the whole `scheduler::SchedulerState`: some of its
    /// fields (`start_time`, `finish_time`, `time_slice_*`, `activation_count`, and
    /// `subtree_invariants`) are guarded by the owning scheduler's `queue_lock_` rather than by
    /// the thread's lock, and `curr_cpu`/`last_cpu` are read lock-free. Use the field-projecting
    /// accessors below to reach the fields guarded by the thread's lock.
    #[inline]
    fn scheduler_state_ptr(&self) -> *mut scheduler::SchedulerState {
        self.owner().scheduler_state.get().cast::<scheduler::SchedulerState>()
    }

    /// Raw place of the `state` member of this thread's [`scheduler::SchedulerState`].
    ///
    /// Only the single field is projected: a reference covering the whole struct would also
    /// cover members guarded by the scheduler's `queue_lock_` (`start_time`, `finish_time`,
    /// `activation_count`, `subtree_invariants`, the run-queue node) and members read lock-free
    /// (`curr_cpu`/`last_cpu`), none of which the thread lock alone may touch.
    #[inline]
    fn state_ptr(&self) -> *mut ThreadStateKind {
        let p = self.scheduler_state_ptr();
        // SAFETY: `p` points at the live storage of the thread's `scheduler::SchedulerState`;
        // `&raw mut` projects the field place without forming a reference to the surrounding
        // struct.
        unsafe { &raw mut (*p).state }
    }

    /// Returns the scheduling state of this thread.
    #[inline]
    pub fn state(&self) -> ThreadStateKind {
        // SAFETY: `self` proves the thread lock is held, which is the only lock `state` is
        // written under, and no reference to the surrounding struct is formed.
        unsafe { *self.state_ptr() }
    }

    /// Sets the thread's state to `ThreadStateKind::Blocked`.
    #[inline]
    pub fn set_blocked(&mut self) {
        // SAFETY: As in `state`; the unique borrow of `self` excludes other writers under the
        // thread lock.
        unsafe { *self.state_ptr() = ThreadStateKind::Blocked };
    }

    /// Sets the thread's state to `ThreadStateKind::BlockedReadLock`.
    #[inline]
    pub fn set_blocked_read_lock(&mut self) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = ThreadStateKind::BlockedReadLock };
    }

    /// Sets the thread's scheduling state (for unit tests).
    #[cfg(ktest)]
    #[inline]
    pub(crate) fn set_state(&mut self, state: ThreadStateKind) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = state };
    }

    /// Returns this thread's base profile.
    #[inline]
    pub(crate) fn base_profile(&self) -> &scheduler::BaseProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held, which guards `base_profile`, and
        // `&(*p).base_profile` projects only the field place without forming a reference to the
        // whole `scheduler::SchedulerState`.
        unsafe { &(*p).base_profile }
    }

    /// Returns this thread's base profile mutably.
    #[inline]
    pub(crate) fn base_profile_mut(&mut self) -> &mut scheduler::BaseProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: As in `base_profile`; the unique borrow of `self` ensures no other reference to
        // `base_profile` is live.
        unsafe { &mut (*p).base_profile }
    }

    /// Returns this thread's inherited profile values.
    #[inline]
    pub(crate) fn inherited_profile_values(&self) -> &scheduler::InheritedProfileValues {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held, which guards
        // `inherited_profile_values`, and `&(*p).inherited_profile_values` projects only that
        // field.
        unsafe { &(*p).inherited_profile_values }
    }

    /// Returns this thread's inherited profile values mutably.
    #[inline]
    pub(crate) fn inherited_profile_values_mut(
        &mut self,
    ) -> &mut scheduler::InheritedProfileValues {
        let p = self.scheduler_state_ptr();
        // SAFETY: As in `inherited_profile_values`; the unique borrow of `self` ensures no other
        // reference to `inherited_profile_values` is live.
        unsafe { &mut (*p).inherited_profile_values }
    }

    /// Returns this thread's effective profile mutably (for example, to mark it dirty).
    #[inline]
    pub(crate) fn effective_profile_mut(&mut self) -> &mut scheduler::EffectiveProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively, which guards writes to
        // `effective_profile`, `&mut (*p).effective_profile` projects only that field, and the
        // unique borrow of `self` ensures no other reference to it is live.
        unsafe { &mut (*p).effective_profile }
    }

    /// Recomputes this thread's effective profile from its `base_profile` and
    /// `inherited_profile_values` without forming a reference to the whole
    /// [`scheduler::SchedulerState`].
    #[inline]
    pub(crate) fn recompute_effective_profile(&mut self) {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively, which guards
        // `base_profile`, `inherited_profile_values`, and `effective_profile`. Each field place is
        // projected individually from the raw pointer without forming a reference to the
        // surrounding `scheduler::SchedulerState`, and the three fields are disjoint.
        unsafe {
            let bp = &(*p).base_profile;
            let ipv = &(*p).inherited_profile_values;
            let ep = &mut (*p).effective_profile;
            ep.recompute(bp, ipv);
        }
    }

    /// Returns this thread's dynamic scheduling parameters, as
    /// `(start_time, finish_time, time_slice_ns, time_slice_used_ns)`.
    ///
    /// These fields are guarded by the owning scheduler's `queue_lock_`, so the values are only
    /// stable if the thread is not in a run queue (for example, because it is blocked, or because
    /// it is the current thread and preemption is disabled).
    #[inline]
    pub(crate) fn dynamic_parameters(
        &self,
    ) -> (
        scheduler::SchedTime,
        scheduler::SchedTime,
        scheduler::SchedDuration,
        scheduler::SchedDuration,
    ) {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`. The fields are read by
        // value through the raw pointer without forming a reference to the whole struct, matching
        // the C++ reads made while holding only the thread's lock.
        unsafe {
            ((*ss).start_time, (*ss).finish_time, (*ss).time_slice_ns, (*ss).time_slice_used_ns)
        }
    }

    /// Returns a reference to this thread's [`ThreadState`].
    #[inline]
    pub fn wait_queue_state(&self) -> &'p ThreadState {
        // `ThreadState`'s own members are `KCell`s and tree bookkeeping with their own protocols,
        // so handing out the reference needs no lock proof beyond the thread outliving `'p`.
        &self.owner().wait_queue_state
    }

    /// Returns the list of `OwnedWaitQueue`s this thread owns.
    pub(crate) fn owned_wait_queues(&self) -> &OwnedWaitQueueList {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `owned_wait_queues`.
        unsafe { self.owner().owned_wait_queues.get(self.token()) }
    }

    /// Returns the list of `OwnedWaitQueue`s this thread owns mutably.
    pub(crate) fn owned_wait_queues_mut(&mut self) -> Pin<&mut OwnedWaitQueueList> {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `owned_wait_queues`; the unique borrow of `self` ensures no other
        // reference to `owned_wait_queues` is live. `Pin::new_unchecked`: every `Thread` is pinned
        // in memory for its whole life, so the embedded list never moves.
        unsafe { Pin::new_unchecked(thread.owned_wait_queues.get_mut(token)) }
    }

    /// Returns the policy exception code saved by `SignalPolicyException()`.
    pub(crate) fn extra_policy_exception_code(&self) -> u32 {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `extra_policy_exception_code`.
        *unsafe { self.owner().extra_policy_exception_code.get(self.token()) }
    }

    /// Returns the policy exception code mutably.
    pub(crate) fn extra_policy_exception_code_mut(&mut self) -> &mut u32 {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `extra_policy_exception_code`; the unique borrow of `self` ensures no
        // other reference is live.
        unsafe { thread.extra_policy_exception_code.get_mut(token) }
    }

    /// Returns the policy exception data saved by `SignalPolicyException()`.
    pub(crate) fn extra_policy_exception_data(&self) -> u32 {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `extra_policy_exception_data`.
        *unsafe { self.owner().extra_policy_exception_data.get(self.token()) }
    }

    /// Returns the policy exception data mutably.
    pub(crate) fn extra_policy_exception_data_mut(&mut self) -> &mut u32 {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `extra_policy_exception_data`; the unique borrow of `self` ensures no
        // other reference is live.
        unsafe { thread.extra_policy_exception_data.get_mut(token) }
    }

    /// Returns whether this thread is allowed to own wait queues.
    pub(crate) fn can_own_wait_queues(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `can_own_wait_queues`.
        *unsafe { self.owner().can_own_wait_queues.get(self.token()) }
    }

    /// Returns whether this thread is allowed to own wait queues mutably.
    pub(crate) fn can_own_wait_queues_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `can_own_wait_queues`; the unique borrow of `self` ensures no other
        // reference is live.
        unsafe { thread.can_own_wait_queues.get_mut(token) }
    }

    /// Returns whether user register state has been saved to the thread's `arch_thread`.
    pub(crate) fn user_state_saved(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `user_state_saved`.
        *unsafe { self.owner().user_state_saved.get(self.token()) }
    }

    /// Returns whether user register state has been saved mutably.
    pub(crate) fn user_state_saved_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `user_state_saved`; the unique borrow of `self` ensures no other
        // reference is live.
        unsafe { thread.user_state_saved.get_mut(token) }
    }

    /// Returns whether a migration is in progress for this thread.
    pub(crate) fn migrate_pending(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `migrate_pending`.
        *unsafe { self.owner().migrate_pending.get(self.token()) }
    }

    /// Returns whether a migration is in progress mutably.
    pub(crate) fn migrate_pending_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `migrate_pending`; the unique borrow of `self` ensures no other reference
        // is live.
        unsafe { thread.migrate_pending.get_mut(token) }
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

    /// Detaches and resumes a newly created thread (`Thread::DetachAndResume`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread has not been joined, detached, or destroyed, and
    /// that `self` is not used after detaching once the thread may have exited.
    pub unsafe fn detach_and_resume(self) -> Result<(), Status> {
        // SAFETY: Caller guarantees `self.as_raw()` points to a valid thread that has not been
        // joined, detached, or destroyed.
        Status::ok(unsafe { cpp_thread_detach_and_resume(self.as_raw()) })
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

    /// Drops the kernel thread without joining or waiting for it (`Thread::Forget`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` in a state where
    /// `Thread::Forget` is permitted (e.g. initialized but never started) and is not used after
    /// this call.
    #[inline]
    pub unsafe fn forget(self) {
        // SAFETY: The caller guarantees `self` points to a valid `Thread` that can be forgotten.
        unsafe { cpp_thread_forget(self.as_raw()) }
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
    #[inline]
    pub unsafe fn fxt_ref(self) -> FxtRef {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { FxtRef { pid: self.pid(), tid: self.tid() } }
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

    /// Returns the current soft CPU affinity mask of the thread (`GetSoftCpuAffinity`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn get_soft_cpu_affinity(self) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_get_soft_cpu_affinity(self.as_ptr()) }
    }

    /// Sets the soft CPU affinity mask of the thread and returns its previous soft affinity mask
    /// (`SetSoftCpuAffinity`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn set_soft_cpu_affinity(self, affinity: cpu_mask_t) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_set_soft_cpu_affinity(self.as_raw(), affinity) }
    }

    /// Returns the total time the thread has spent running on a CPU (`Runtime`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn runtime(self) -> DurationMono {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_runtime(self.as_ptr()) }
    }

    /// Returns the CPU the thread was last scheduled on (`LastCpu`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn last_cpu(self) -> cpu_num_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { cpp_thread_last_cpu(self.as_ptr()) }
    }

    /// Returns a reference to this thread's [`ChainLock`].
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `self` points to a valid `Thread` which remains alive for as
    ///   long as the returned reference is used.
    /// - The caller must not use the returned [`ChainLock`] to release the lock or to create
    ///   overlapping guards (`ChainLockGuard::adopt`/`unguard`) while any
    ///   `ChainLockProof<'_, Thread>` for this thread is live (`ThreadLockGuard::proof`,
    ///   `OwnerGuard::proof`, `single_chain_lock_guard_owned!`).
    #[inline]
    pub unsafe fn get_lock(&self) -> &ChainLock {
        // SAFETY: `self` points to a valid `Thread` by the caller's guarantee.
        // `as_chain_lock`'s requirements about overlapping guards are forwarded to the caller.
        unsafe { self.as_ref().lock().as_chain_lock() }
    }

    /// Returns the associated `ThreadDispatcher`, or null if this is a kernel-only thread.
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
    /// koid for kernel threads.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn tid(self) -> zx_koid_t {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::tid_ptr(self.as_ptr()) }
    }

    /// Returns the koid of the associated `ProcessDispatcher` for user threads or
    /// `ZX_KOID_INVALID` for kernel threads.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn pid(self) -> zx_koid_t {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::pid_ptr(self.as_ptr()) }
    }

    /// Returns the last flow id allocated by [`ThreadPtr::take_next_lock_flow_id`] for this thread.
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
        // SAFETY: `self.thread` is a live `Thread` (`lock`'s contract) whose lock `raw` acquired
        // and holds until drop; the temporary proof does not escape this call.
        unsafe { ChainLockProof::from_acquiring_guard(self.thread.as_ref()) }.user_state_saved()
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

/// The current thread's [`PreemptionState`] (`Thread::Current::preemption_state()`), as a pointer
/// to the field alone so that no `&Thread` is formed.
fn current_preemption_state() -> *const PreemptionState {
    let current = current_get();
    // SAFETY: `current` is the valid, live current thread; projecting the field through the raw
    // pointer forms no reference.
    unsafe { &raw const (*current).preemption_state }
}

/// Increments the preempt disable counter for the current thread.
///
/// While preempt disable is non-zero, preemption of the thread is disabled, including preemption
/// from interrupt handlers. During this time, any call to `Reschedule()` will only record that a
/// reschedule is pending, and won't do a context switch.
///
/// Note: [`preempt_disable`] does not preclude a thread from blocking (e.g., waiting on a lock).
/// Disabling preemption does not prevent switching away from the current thread if it blocks.
///
/// A call to [`preempt_disable`] must be matched by a later call to [`preempt_reenable`] to
/// decrement the preempt disable counter.
#[inline]
pub fn preempt_disable() {
    // SAFETY: The current thread is alive for as long as code runs on it, and `PreemptionState`
    // is all relaxed atomics, so a shared reference to the field is sound while C++ and interrupt
    // handlers mutate it.
    unsafe { (*current_preemption_state()).preempt_disable() }
}

/// Decrements the preempt disable counter and flushes any pending local preemption operation.
///
/// Callers must ensure that they are calling from a context where blocking is allowed, as the call
/// may result in the immediate preemption of the calling thread.
///
/// # Safety
///
/// The call must pair with a prior, still unmatched [`preempt_disable`] on the current thread,
/// and it must not drop preemption back to enabled while any [`PreemptDisabledToken`] is live.
#[inline]
pub unsafe fn preempt_reenable() {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` is by construction the
    // current thread's preemption state, and the pairing and token obligations are forwarded
    // from the caller.
    unsafe { (*current_preemption_state()).preempt_reenable() }
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
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).eager_resched_disable() }
}

/// Decrements the eager resched disable counter and flushes pending local and/or remote
/// preemptions if enabled, respectively.
///
/// # Safety
///
/// The call must pair with a prior, still unmatched [`eager_resched_disable`] on the current
/// thread, and it must not drop preemption back to enabled while any [`PreemptDisabledToken`] is
/// live.
#[inline]
pub unsafe fn eager_resched_reenable() {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` is by construction the
    // current thread's preemption state, and the pairing and token obligations are forwarded
    // from the caller.
    unsafe { (*current_preemption_state()).eager_resched_reenable() }
}

/// Returns the current thread's preemption disable count.
#[inline]
pub fn preempt_disable_count() -> u32 {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).preempt_disable_count() }
}

/// Returns the current thread's eager resched disable count.
#[inline]
pub fn eager_resched_disable_count() -> u32 {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).eager_resched_disable_count() }
}

/// Marks a pending preemption for the current CPU on the current thread's preemption state.
///
/// This is similar to `Reschedule()`, except that it may only be used inside an interrupt handler
/// while interrupts and preemption are disabled, between [`preempt_disable`] and
/// [`preempt_reenable`]. Unlike `Reschedule()`, it does not need to be called with the thread's
/// lock held.
///
/// Supplies the C++ default argument `cpu_num_to_mask(arch_curr_cpu_num())`.
#[inline]
pub fn preempt_set_pending() {
    // SAFETY: As in `preempt_disable`.
    unsafe {
        (*current_preemption_state()).preempt_set_pending(crate::kernel::types::cpu_num_to_mask(
            crate::arch_rs::curr_cpu_num(),
        ))
    }
}

/// Returns `true` if preemption is enabled for the current thread.
#[inline]
pub fn preempt_is_enabled() -> bool {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).preempt_is_enabled() }
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
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).set_timeslice_extension(duration.0) }
}

/// Unconditionally clears any timeslice extension.
///
/// This function must be called in normal thread context because it may trigger local preemption.
pub fn preempt_clear_timeslice_extension() {
    // SAFETY: As in `preempt_disable`, and `current_preemption_state()` is by construction the
    // current thread's preemption state.
    unsafe { (*current_preemption_state()).clear_timeslice_extension() }
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

/// Opaque inline backing storage matching C++ `ScopedThreadExceptionContext`.
#[repr(C, align(8))]
struct CppScopedThreadExceptionContext(OpaqueBytes<24>);

/// Private RAII wrapper around C++ `ScopedThreadExceptionContext`.
///
/// Kept private behind [`with_exception_context`] so safe code cannot call `core::mem::forget` on
/// the guard and leave `Thread::arch().suspended_general_regs` dangling into popped stack memory.
#[repr(C)]
struct ScopedThreadExceptionContext {
    storage: core::mem::MaybeUninit<CppScopedThreadExceptionContext>,
    _not_send_sync: PhantomData<*mut ()>,
}

zr::static_assert!(
    core::mem::size_of::<ScopedThreadExceptionContext>() == 3 * core::mem::size_of::<usize>()
);
zr::static_assert!(
    core::mem::align_of::<ScopedThreadExceptionContext>() == core::mem::align_of::<usize>()
);

impl Drop for ScopedThreadExceptionContext {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self.storage` was initialized by `cpp_scoped_thread_exception_context_init` in
        // `with_exception_context` and has not yet been destroyed, and the caller of
        // `with_exception_context` guarantees neither the current thread's lock nor an active
        // `ChainLockTransaction` is held when the scope exits.
        unsafe {
            cpp_scoped_thread_exception_context_destroy(self.storage.as_mut_ptr());
        }
    }
}

/// Installs `context` on the current user thread for the duration of `f`, making its user register
/// state available to debuggers and exception handlers while the thread is in an exception.
///
/// # Safety
///
/// * `context` must point to a valid, initialized `arch_exception_context_t` for the current
///   thread that outlives this call.
/// * If `(*context).frame` is non-null, it must point to a valid, initialized, writable `iframe_t`
///   that outlives this call with no conflicting Rust references while installed.
/// * The current thread must be a user thread (`user_thread_ != nullptr`).
/// * Neither the current thread's lock nor an active `ChainLockTransaction` may be held on the
///   current CPU when entering `with_exception_context` or when `f` returns (or unwinds).
#[inline]
pub unsafe fn with_exception_context<R>(
    context: *const crate::arch_rs::ArchExceptionContext,
    f: impl FnOnce() -> R,
) -> R {
    let mut guard = ScopedThreadExceptionContext {
        storage: core::mem::MaybeUninit::uninit(),
        _not_send_sync: PhantomData,
    };
    // SAFETY: `guard.storage` has the verified size and alignment of C++
    // `ScopedThreadExceptionContext` (which is non-self-referential), and the caller guarantees
    // `context` (and any non-null `(*context).frame`) is valid for the current user thread and
    // outlives this call with neither the thread lock nor an active `ChainLockTransaction` held.
    unsafe {
        cpp_scoped_thread_exception_context_init(&raw mut guard.storage, context);
    }
    let result = f();
    drop(guard);
    result
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

/// Returns the total runtime of the current thread.
///
/// # Safety
///
/// The caller must not hold the current thread's lock or have an active chain lock transaction on
/// the current CPU. `Thread::Runtime` starts its own chain lock transaction, and nesting it inside
/// an active transaction is only checked by debug assertions.
#[inline]
pub unsafe fn current_runtime() -> DurationMono {
    // SAFETY: `cpp_thread_current_get` returns a valid pointer to the running thread, and the
    // caller upholds the locking preconditions of `cpp_thread_runtime`.
    unsafe { cpp_thread_runtime(cpp_thread_current_get()) }
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
/// Caller must ensure `thread` points to a valid `Thread` instance and that the caller holds
/// `thread`'s lock for the duration of this call, with no proof for it live.
pub unsafe fn is_user_state_saved_locked(thread: *const Thread) -> bool {
    // SAFETY: `thread` is a valid `Thread` whose lock the caller holds with no proof live
    // (caller's contract); the temporary proof does not escape this call.
    unsafe { ChainLockProof::assume_held_unchecked(&*thread) }.user_state_saved()
}

/// Checks whether `thread` is currently running.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn is_running(thread: *const Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::state() with caller-verified pointer.
    unsafe { cpp_thread_is_running(thread) }
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

/// Switches the current thread's address space pointer without performing a hardware context
/// switch.
///
/// # Safety
///
/// The caller must ensure `aspace` remains valid while attached to the current thread.
pub unsafe fn current_switch_aspace(aspace: &VmAspace) -> *mut VmAspace {
    let ptr = core::ptr::from_ref(aspace).cast_mut();
    // SAFETY: Caller guarantees `aspace` remains valid while attached to the current thread.
    unsafe { cpp_thread_current_switch_aspace(ptr) }
}

/// Clears the current thread's address space pointer without performing a hardware context
/// switch.
///
/// Note: This helper is only used by test and kernel debugging code (such as `k vmm` console
/// commands) to detach a test address space from a kernel thread.
///
/// # Safety
///
/// The caller must ensure the current thread can safely run without an attached user address
/// space.
pub unsafe fn current_clear_aspace() -> *mut VmAspace {
    // SAFETY: Caller guarantees clearing the current thread's address space pointer is valid.
    unsafe { cpp_thread_current_switch_aspace(core::ptr::null_mut()) }
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
#[inline]
pub fn current_is_signaled() -> bool {
    // SAFETY: `current_get()` returns the current `Thread`, and `&raw const` projects the `signals`
    // field place without forming a reference to the whole `Thread`.
    let signals = unsafe { &raw const (*current_get()).signals };
    // SAFETY: `signals` is the live current `Thread`'s `signals` field (see above).
    unsafe { (*signals).load() != 0 }
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

/// Generate a backtrace for the calling thread.
///
/// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot
/// be obtained, it will be left empty.
#[inline]
pub fn current_get_backtrace(out_bt: &mut Backtrace) {
    // SAFETY: `out_bt` is a valid, writable `Backtrace` on the current stack.
    unsafe { cpp_thread_current_get_backtrace(out_bt) };
}

/// Forwards C++ `Thread::set_signals`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_set_signals(thread: &Thread, bits: u32) {
    thread.set_signals(bits);
}

/// Forwards C++ `Thread::clear_signals`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_clear_signals(thread: &Thread, bits: u32) {
    thread.clear_signals(bits);
}

/// Forwards C++ `Thread::flags`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_flags(thread: &Thread) -> u32 {
    thread.flags()
}

/// Forwards C++ `Thread::detached`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_detached(thread: &Thread) -> bool {
    thread.detached()
}

/// Forwards C++ `Thread::set_detached`.
///
/// # Safety
///
/// Callers must uphold the [`Thread::set_detached`] lifecycle invariants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_detached(thread: &Thread, value: bool) {
    // SAFETY: Forwarded from C++ `Thread::set_detached` caller.
    unsafe { thread.set_detached(value) };
}

/// Forwards C++ `Thread::free_struct`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_free_struct(thread: &Thread) -> bool {
    thread.free_struct()
}

/// Forwards C++ `Thread::set_free_struct`.
///
/// # Safety
///
/// Callers must uphold the [`Thread::set_free_struct`] storage-ownership invariants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_free_struct(thread: &Thread, value: bool) {
    // SAFETY: Forwarded from C++ `Thread::set_free_struct` caller.
    unsafe { thread.set_free_struct(value) };
}

/// Forwards C++ `Thread::idle`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_idle(thread: &Thread) -> bool {
    thread.is_idle()
}

/// Forwards C++ `Thread::set_idle`.
///
/// # Safety
///
/// Callers must uphold the [`Thread::set_idle`] idle-thread invariants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_idle(thread: &Thread, value: bool) {
    // SAFETY: Forwarded from C++ `Thread::set_idle` caller.
    unsafe { thread.set_idle(value) };
}

/// Forwards C++ `Thread::vcpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_vcpu(thread: &Thread) -> bool {
    thread.vcpu()
}

/// Forwards C++ `Thread::set_vcpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_set_vcpu(thread: &Thread, value: bool) {
    thread.set_vcpu(value);
}

/// Forwards C++ `Thread::name`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_name(thread: &Thread) -> *const c_char {
    thread.name.as_ptr().cast::<c_char>()
}

/// Forwards C++ `Thread::set_name`. Takes `*mut Thread` rather than `&Thread` because C++ calls
/// this from the `Thread::Thread` constructor before the `Thread` is fully constructed.
///
/// # Safety
///
/// - `thread` must point to allocated, suitably aligned `Thread` storage whose `name` bytes are
///   initialized, with no concurrent non-atomic access to `name`.
/// - When `len > 0`, `name` must be valid for reads of `len` initialized bytes (the
///   [`zr::slice_from_raw_parts`] preconditions), and `[name, name + len)` must not overlap
///   `thread`'s `name` buffer or be concurrently mutated for the duration of this call. When
///   `len == 0`, `name` is not dereferenced and may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_name(
    thread: *mut Thread,
    name: *const c_char,
    len: usize,
) {
    // SAFETY: Caller guarantees `name` is valid for `len` non-overlapping, unmutated bytes when
    // `len > 0`, and `zr::slice_from_raw_parts` handles `len == 0` without dereferencing `name`.
    let bytes = unsafe { zr::slice_from_raw_parts(name.cast::<u8>(), len) };
    // SAFETY: Caller guarantees `thread` has valid storage for `name`.
    unsafe { Thread::set_name_in_place(thread, bytes) };
}

/// Forwards C++ `Thread::CheckKillOrSuspendSignal`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_check_kill_or_suspend_signal(thread: &Thread) -> zx_status_t {
    Status::result_into_raw(thread.check_kill_or_suspend_signal())
}

/// Forwards C++ `TaskState::Init`.
///
/// # Safety
///
/// `task_state` is a valid `TaskState` not yet visible to any other thread, with no live Rust
/// reference to it (see [`TaskState::init`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_task_state_init(
    task_state: *mut TaskState,
    entry: Option<extern "C" fn(*mut c_void) -> i32>,
    arg: *mut c_void,
) {
    // SAFETY: Forwarded from the C++ `TaskState::Init` caller, which upholds `init`'s contract.
    unsafe { TaskState::init(task_state, entry, arg) };
}

/// Forwards C++ `TaskState::Join`.
///
/// # Safety
///
/// - A `ChainLockTransaction` must be active and finalized on this CPU (`assert_active` is
///   debug-only; calling with no active transaction is UB in release).
/// - `current_thread` must be the valid, currently executing `Thread` in `THREAD_RUNNING`, not
///   queued in any wait queue, in a context where blocking is permitted (no spinlocks held,
///   blocking not disallowed), with no Rust guard or proof for it live.
/// - `*task_state` must be valid at the time of the call with no Rust guard or proof for its
///   `retcode_wait_queue` live, and the queue must remain valid while `current_thread` remains
///   blocked in it (and at least until both the queue lock and `current_thread`'s lock are
///   released during the block operation); see [`TaskState::join`].
/// - `task_state`'s queue lock and `current_thread`'s lock must be held by the active
///   transaction's *current* token as the only held chain locks (both are debug-asserted).
/// - The queue lock is released by this call (`TA_REL(get_lock())`) and must not be released
///   again by the caller; `current_thread`'s lock is still held on return (`TA_REQ`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_task_state_join(
    task_state: *mut TaskState,
    current_thread: *mut Thread,
    deadline: zx_instant_mono_t,
) -> zx_status_t {
    debug_assert!(!task_state.is_null());
    debug_assert!(!current_thread.is_null());
    // SAFETY: A transaction is active and finalized by precondition.
    let token = unsafe { ChainLockTransaction::assert_active() };
    // SAFETY:
    // - `current_thread` is the valid current thread per caller contract, and it outlives the
    //   call, so a `&Thread` may live across the block. Its lock is held by the active transaction
    //   (`assert_held`, debug-only) with no other proof live, and a proof never releases the lock,
    //   so the caller keeps that responsibility.
    // - `task_state` is passed as a raw pointer: `TaskState::join` scopes the `&WaitQueue` and its
    //   adopted guard to the preamble so that nothing into the (possibly freed) target is live
    //   across `WaitQueueBase::block_etc_postamble`. All other blocking preconditions of
    //   `TaskState::join` are upheld by the caller.
    let res = unsafe {
        let thread: &Thread = &*current_thread;
        thread.lock().assert_held(&token);
        let mut thread_proof = ChainLockProof::assume_held_unchecked(thread);
        TaskState::join(task_state, &mut thread_proof, InstantMono(deadline), &token)
    };
    Status::result_into_raw(res)
}

/// Forwards C++ `TaskState::TryWakeJoiners`.
///
/// # Safety
///
/// - A `ChainLockTransaction` must be active and not finalized on this CPU (`assert_active` is
///   debug-only; calling with no active transaction is UB in release).
/// - Preemption must be disabled (`TA_REQ(preempt_disabled_token)`).
/// - `task_state`'s queue lock must not already be held by this transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_task_state_try_wake_joiners(
    task_state: &TaskState,
    status: zx_status_t,
) -> bool {
    // SAFETY: A transaction is active by precondition.
    let token = unsafe { ChainLockTransaction::assert_active() };
    // SAFETY: Preemption is disabled by precondition (`TA_REQ(preempt_disabled_token)`).
    let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
    task_state.try_wake_joiners(Status::ok(status), &token, &preempt_disabled_token)
}

/// The default trampoline used when running the Thread. This can be
/// replaced by the `alt_trampoline` parameter to `CreateEtc()`.
///
/// `Thread::CreateEtc` and `Thread::ReviveIdlePowerThread` install this symbol directly as the
/// thread's initial PC (`arch_thread_initialize`), so there is no intermediate frame.
///
/// Thread trampolines are always called with:
///
/// 1. The thread's lock held
/// 2. interrupts disabled.
///
/// Implementers of trampolines must always remember to unconditionally release
/// the current thread's lock, and *then* enable interrupts, before jumping to
/// the thread's entry point.
///
/// Sadly, there does not seem to be a good way to statically annotate this.
///
/// # Safety
///
/// Must only be entered as the initial PC of a newly context-switched thread, with interrupts
/// disabled and the `ChainLockTransaction` registered by `Scheduler::reschedule_common` still
/// active and holding both the previous thread's lock and the current thread's lock under its
/// reserved scheduler token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_trampoline() -> ! {
    // Handle the special case of starting a new thread for the first time,
    // releasing the previous thread's lock as well as the current thread's lock
    // in the process, before restoring interrupts and dropping into the thread's
    // entry point.
    //
    // SAFETY: The caller guarantees this is the initial trampoline entry with interrupts disabled
    // and the `reschedule_common` transaction active holding both locks with the reserved scheduler
    // token.
    unsafe { scheduler::trampoline_lock_handoff() };
    enable_ints();

    // Call into the thread's entry point, and call exit when finished.
    let current = current_get();
    // SAFETY: `current` is the valid, live current thread; projecting the field forms no
    // reference, so no `&Thread` is live across the indirect call, and `entry`/`arg` were written
    // once by `TaskState::init` before this thread was published.
    let (entry, arg) = unsafe {
        let task_state = &raw const (*current).task_state;
        ((*task_state).entry, (*task_state).arg)
    };
    // `CreateEtc` installs this default trampoline only when no `alt_trampoline` is given, and
    // every `CreateEtc` caller that passes a null `entry` supplies an `alt_trampoline`, so `entry`
    // is non-null here; a null `entry` is a caller bug, so panic.
    let entry =
        entry.expect("the default trampoline is only installed for threads created with an entry");
    // TODO(https://fxbug.dev/528405969): The C++ `thread_start_routine` typedef in
    // `kernel/thread.h` is `[[clang::cfi_unchecked_callee]]`. Rust has no equivalent opt-out;
    // revisit this indirect call if kernel Rust ever enables KCFI.
    let ret = entry(arg);
    current_exit(ret)
}

/// Forwards C++ `PreemptionState::PreemptDisable`.
///
/// Every call must be matched by a later `PreemptReenable`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_preempt_disable(preemption_state: &PreemptionState) {
    preemption_state.preempt_disable();
}

/// Forwards C++ `PreemptionState::PreemptReenable`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`, the call pairs with a prior
/// unmatched `PreemptDisable` on the current thread, and it does not drop preemption back to
/// enabled while any `PreemptDisabledToken` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_preempt_reenable(
    preemption_state: &PreemptionState,
) {
    // SAFETY: The caller guarantees this is the current thread's state and the pairing and token
    // obligations.
    unsafe { preemption_state.preempt_reenable() };
}

/// Forwards C++ `PreemptionState::PreemptReenableDelayFlush`.
///
/// Interrupts must be disabled and blocking disallowed, as `debug_assert!`ed by
/// [`PreemptionState::preempt_reenable_delay_flush`].
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`, the call pairs with a prior
/// unmatched `PreemptDisable` on the current thread, and it does not drop preemption back to
/// enabled while any `PreemptDisabledToken` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_preempt_reenable_delay_flush(
    preemption_state: &PreemptionState,
) -> bool {
    // SAFETY: The caller guarantees the pairing and token obligations.
    unsafe { preemption_state.preempt_reenable_delay_flush() }
}

/// Forwards C++ `PreemptionState::EagerReschedDisable`.
///
/// Every call must be matched by a later `EagerReschedReenable`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_eager_resched_disable(preemption_state: &PreemptionState) {
    preemption_state.eager_resched_disable();
}

/// Forwards C++ `PreemptionState::EagerReschedReenable`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`, the call pairs with a prior
/// unmatched `EagerReschedDisable` on the current thread, and it does not drop preemption back to
/// enabled while any `PreemptDisabledToken` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_eager_resched_reenable(
    preemption_state: &PreemptionState,
) {
    // SAFETY: The caller guarantees this is the current thread's state and the pairing and token
    // obligations.
    unsafe { preemption_state.eager_resched_reenable() };
}

/// Forwards C++ `PreemptionState::SetTimesliceExtension`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_set_timeslice_extension(
    preemption_state: &PreemptionState,
    extension_duration: zx_types::zx_duration_mono_t,
) -> bool {
    preemption_state.set_timeslice_extension(extension_duration)
}

/// Forwards C++ `PreemptionState::ClearTimesliceExtension`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_clear_timeslice_extension(
    preemption_state: &PreemptionState,
) {
    // SAFETY: The caller guarantees this is the current thread's state.
    unsafe { preemption_state.clear_timeslice_extension() };
}

/// Forwards C++ `PreemptionState::PreemptSetPending`.
///
/// Interrupts and preemption must be disabled and blocking disallowed (interrupt-handler
/// context), as `debug_assert!`ed by [`PreemptionState::preempt_set_pending`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_preempt_set_pending(
    preemption_state: &PreemptionState,
    reschedule_mask: crate::kernel::types::cpu_mask_t,
) {
    preemption_state.preempt_set_pending(reschedule_mask);
}

/// Forwards C++ `PreemptionState::EvaluateTimesliceExtension`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_evaluate_timeslice_extension(
    preemption_state: &PreemptionState,
) -> bool {
    preemption_state.evaluate_timeslice_extension()
}

/// Forwards C++ `PreemptionState::Reset`.
///
/// # Safety
/// `preemption_state`'s owning thread is halted (not running on any CPU) and no
/// `PreemptDisabledToken` for it is live. This exists only for reviving an idle/power thread that
/// halted while taking a CPU offline.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_reset(preemption_state: &PreemptionState) {
    // SAFETY: The caller guarantees the owning thread is halted with no live token.
    unsafe { preemption_state.reset() };
}

/// Unit tests for Thread Rust bindings.
#[cfg(ktest)]
#[unittest::suite(name = "thread_rust")]
mod tests {
    use super::{
        AnnotatedAutoEagerReschedDisabler, ChainLocked, DEFAULT_PRIORITY, Interruptible,
        LOW_PRIORITY, ScopedMemoryAllocationDisabled, Thread, ThreadLockGuard, ThreadPtr,
        ThreadStateKind, cpp_thread_current_memory_allocation_state_disable,
        cpp_thread_current_memory_allocation_state_enable, create_with_priority, current_get,
        current_memory_allocation_state_is_enabled, eager_resched_disable_count, layout,
        sleep_relative,
    };
    use crate::arch_rs::curr_cpu_num;
    use crate::kconcurrent::{
        ChainLockTransaction, DONE, IRQ_SAVE_OPTION, OwnerGuard, SingleChainLockGuard, clt_tag,
        single_chain_lock_guard_owned,
    };
    use crate::kernel::event::Event;
    use crate::kernel::mp::{INVALID_CPU, cpu_num_to_mask, get_online_mask};
    use crate::kernel::types::{cpu_mask_t, cpu_num_t, highest_cpu_set};
    use crate::kernel::wait_queue::{
        BlockedThreadTreeTag, MinRelativeDeadlineTraits, PRIMARY_KEY_ZERO,
    };
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
    use core::ffi::{CStr, c_void};
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use fbl::{WavlTreeAugmentedInvariantObserverTraits, WavlTreeContainable, WavlTreeKeyable};
    use kprint::kprintln;
    use unittest::{
        assert_eq, assert_ne, assert_true, expect_eq, expect_err, expect_false, expect_ok,
        expect_true, unwrap_ok,
    };
    use zx_status::Status;
    use zx_types::{ZX_ERR_BAD_STATE, ZX_ERR_TIMED_OUT, ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_OK};

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
                |guard| lock.is_held(guard.token()),
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

    /// Verifies the `ChainLockProof<'_, Thread>` accessors on the (unblocked) running thread.
    #[test]
    fn thread_proof_guarded_fields() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };
        // The lock is the `ChainLockable` base subobject, so it lives at the thread's address.
        expect_eq!(core::ptr::from_ref(current.lock()).addr(), current_ptr.addr());
        expect_true!(current.canary.valid());
        current.assert_canary();
        expect_true!(current.check_kill_or_suspend_signal().is_ok());

        let mut running = false;
        let mut state_transitions_match = false;
        let mut not_blocked = false;
        let mut interruptible = Interruptible::Yes;
        let mut no_owned_wait_queues = false;
        let mut can_own_wait_queues = false;
        let mut user_state_saved = true;
        let mut migrate_pending = true;
        let mut policy_exception = (u32::MAX, u32::MAX);
        let mut scheduler_state_matches = false;
        let mut wait_queue_state_matches = false;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("thread_proof_guarded_fields"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                let mut proof = guard.proof();

                let orig_state = proof.state();
                running = orig_state == ThreadStateKind::Running;
                proof.set_blocked();
                let blocked_ok = proof.state() == ThreadStateKind::Blocked;
                proof.set_blocked_read_lock();
                let blocked_read_ok = proof.state() == ThreadStateKind::BlockedReadLock;
                proof.set_state(orig_state);
                state_transitions_match =
                    blocked_ok && blocked_read_ok && proof.state() == orig_state;

                // `ThreadState` members (wait_queue.rs accessors).
                let wq_state = proof.wait_queue_state();
                not_blocked = !wq_state.in_wait_queue(proof.token())
                    && wq_state.blocking_wait_queue(proof.token()).is_none();
                // `blocked_status` keeps the last wake status until the next block, so only check
                // that it reads back.
                let _ = wq_state.blocked_status(proof.token());
                interruptible = wq_state.interruptible(proof.token());

                // `Thread` members.
                no_owned_wait_queues = proof.owned_wait_queues().is_empty()
                    && proof.owned_wait_queues_mut().is_empty();
                can_own_wait_queues = proof.can_own_wait_queues();
                user_state_saved = proof.user_state_saved();
                migrate_pending = proof.migrate_pending();
                policy_exception =
                    (proof.extra_policy_exception_code(), proof.extra_policy_exception_data());

                // Writing the current values back through the `_mut` accessors leaves the thread
                // unchanged.
                *proof.extra_policy_exception_code_mut() = policy_exception.0;
                *proof.extra_policy_exception_data_mut() = policy_exception.1;
                *proof.can_own_wait_queues_mut() = can_own_wait_queues;
                *proof.user_state_saved_mut() = user_state_saved;
                *proof.migrate_pending_mut() = migrate_pending;

                // The proof's `SchedulerState` facade and `ThreadState` sit over the members at
                // the offsets `<kernel/thread_layout.h>` declares.
                scheduler_state_matches = core::ptr::from_ref(proof.scheduler_state()).addr()
                    == current_ptr.addr() + layout::ThreadLayout_kSchedulerStateOffset;
                wait_queue_state_matches = core::ptr::from_ref(proof.wait_queue_state()).addr()
                    == current_ptr.addr() + layout::ThreadLayout_kWaitQueueStateOffset;
                guard.release();
                DONE
            },
        );
        expect_true!(running);
        expect_true!(state_transitions_match);
        expect_true!(not_blocked);
        expect_eq!(interruptible, Interruptible::No);
        expect_true!(no_owned_wait_queues);
        expect_true!(can_own_wait_queues);
        expect_false!(user_state_saved);
        expect_false!(migrate_pending);
        expect_eq!(policy_exception.0, 0);
        expect_eq!(policy_exception.1, 0);
        expect_true!(scheduler_state_matches);
        expect_true!(wait_queue_state_matches);
        expect_true!(current.lock().is_unlocked());
    }

    /// Verifies WAVL tree traits and `MinRelativeDeadlineTraits` on an unqueued `Thread`.
    #[test]
    fn wavl_tree_traits_on_thread() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };

        // The running test thread is not in any wait queue.
        let node = <Thread as WavlTreeContainable<Thread, BlockedThreadTreeTag>>::get_node(current);
        expect_false!(node.in_container());

        let mut key: (InstantMono, usize) = (InstantMono::INFINITE, 0);
        let mut node_deadline = DurationMono::ZERO;
        let mut subtree_deadline = DurationMono::ZERO;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("wavl_tree_traits_on_thread"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                let _proof = guard.proof();
                // The current thread is not in any wait queue and we hold its lock, which
                // satisfies the synchronization preconditions of the WAVL tree trait hooks.
                key = <Thread as WavlTreeKeyable<(InstantMono, usize)>>::get_key(current);
                node_deadline = MinRelativeDeadlineTraits::get_node_value(current);
                subtree_deadline = MinRelativeDeadlineTraits::get_subtree_value(current);
                guard.release();
                DONE
            },
        );
        // `WaitQueueCollection::Remove` only resets the sort key in debug builds, so a thread
        // that has blocked before may carry a stale key in release builds.
        if cfg!(debug_assertions) {
            expect_eq!(key.0.0, PRIMARY_KEY_ZERO.0);
        }
        expect_eq!(key.1, current_ptr.addr());
        // The running test thread has a fair profile, so its relative deadline is `INFINITE`.
        expect_eq!(node_deadline.0, DurationMono::INFINITE.0);
        expect_eq!(subtree_deadline.0, DurationMono::INFINITE.0);
        expect_eq!(
            MinRelativeDeadlineTraits::combine_values(
                DurationMono::from_millis(10),
                DurationMono::INFINITE
            )
            .0,
            DurationMono::from_millis(10).0
        );
    }

    /// Shared with a helper thread that waits on `event` until `deadline` (monotonic) and exits
    /// with `retcode`, or with the raw status `wait` returned on failure.
    struct WaitCtx {
        event: *const Event,
        deadline: InstantMono,
        retcode: i32,
    }

    impl WaitCtx {
        /// A generous but finite deadline, so a failed test self-terminates via the wait queue's
        /// timeout instead of wedging the join.
        fn ten_seconds(event: &Event) -> Self {
            Self {
                event: core::ptr::from_ref(event),
                deadline: InstantMono(current_mono_time().0 + 10 * 1_000_000_000),
                retcode: zx_types::ZX_OK,
            }
        }
    }

    extern "C" fn wait_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `WaitCtx` the spawning test keeps alive until it joins us.
        let ctx = unsafe { &*arg.cast::<WaitCtx>() };
        // SAFETY: As above for the event the context points at.
        let event = unsafe { &*ctx.event };
        match event.wait_deadline(ctx.deadline, super::Interruptible::No) {
            Ok(()) => ctx.retcode,
            Err(status) => status.into_raw(),
        }
    }

    /// Waits for `waiter` to enter `THREAD_BLOCKED` in a wait queue.
    fn wait_until_blocked(waiter: super::ThreadPtr) -> bool {
        for _ in 0..5000 {
            // SAFETY: Callers have not joined `waiter` yet.
            if unsafe { waiter.is_blocked() } {
                return true;
            }
            let _ = super::sleep_relative(DurationMono(1_000_000)); // 1ms
        }
        false
    }

    /// A helper thread parked in `ctx`'s event queue. Its `Thread` can be inspected and poked
    /// through [`ParkedWaiter::thread`] while it sits there; it is signaled and joined exactly
    /// once, either by [`ParkedWaiter::join`] or on drop, so an early test return never leaves
    /// it live.
    struct ParkedWaiter<'a> {
        ctx: &'a WaitCtx,
        thread: Option<super::ThreadPtr>,
    }

    impl<'a> ParkedWaiter<'a> {
        /// Spawns the waiter and waits for it to block; `None` if either step fails.
        fn spawn(ctx: &'a WaitCtx) -> Option<Self> {
            // SAFETY: `ctx` outlives the thread, which `join`/`Drop` joins.
            let thread = unsafe {
                super::spawn(
                    c"thread_rust_waiter".as_ptr(),
                    wait_entry,
                    core::ptr::from_ref(ctx).cast_mut().cast(),
                )
            }
            .ok()?;
            let waiter = Self { ctx, thread: Some(thread) };
            wait_until_blocked(thread).then_some(waiter)
        }

        fn thread(&self) -> &super::Thread {
            // SAFETY: The waiter has not been joined (it is still in `self.thread`), so its
            // `Thread` is alive for as long as `self` is borrowed.
            unsafe { &*self.ptr().as_ptr() }
        }

        fn ptr(&self) -> super::ThreadPtr {
            self.thread.expect("only `join`/`joined_elsewhere` take the thread, consuming self")
        }

        /// Gives up the waiter without joining it, because some other thread already did.
        fn joined_elsewhere(mut self) {
            self.thread = None;
        }

        /// Opens the event and joins the waiter, returning its exit status.
        fn join(mut self) -> Result<i32, zx_status::Status> {
            let thread = self.thread.take().expect("only `join` takes the thread");
            // SAFETY: `self.event` outlives `self`.
            unsafe { &*self.ctx.event }.signal();
            // SAFETY: The thread was taken out of `self.thread`, so it is joined exactly once.
            unsafe { thread.join(InstantMono::INFINITE) }
        }
    }

    impl Drop for ParkedWaiter<'_> {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                // SAFETY: `self.event` outlives `self`.
                unsafe { &*self.ctx.event }.signal();
                // SAFETY: The thread was taken out of `self.thread`, so it is joined exactly once.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }
        }
    }

    /// Verifies the flag getters and setters on a parked helper thread, restoring its flags.
    // A helper is used rather than the running thread because a blocked thread's flags are not
    // concurrently modified by other systems, while the running thread's might be. Toggling
    // `FREE_STRUCT`, `IDLE`, and `DETACHED` here is safe because `waiter` is parked on `event` and
    // neither exits nor is scheduled until `waiter.join()`, after every flag is restored.
    #[test]
    fn test_thread_flags() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { deadline: InstantMono::INFINITE, ..WaitCtx::ten_seconds(&event) };
        let waiter = ParkedWaiter::spawn(&ctx);
        assert_true!(waiter.is_some(), "failed to spawn and park the waiter");
        let waiter = waiter.expect("checked above");
        let thread = waiter.thread();
        let saved = thread.flags();
        // Put every bit back however the test exits.
        let _restore = zr::defer(|| {
            // SAFETY: Restores the parked waiter's original flags before `waiter` is dropped.
            unsafe {
                thread.set_free_struct(saved & super::THREAD_FLAG_FREE_STRUCT != 0);
                thread.set_idle(saved & super::THREAD_FLAG_IDLE != 0);
                thread.set_detached(saved & super::THREAD_FLAG_DETACHED != 0);
            }
            thread.set_vcpu(saved & super::THREAD_FLAG_VCPU != 0);
        });

        expect_false!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_false!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });

        // SAFETY: `waiter` is parked in `event` and all flags are restored before it wakes/exits.
        unsafe { thread.set_free_struct(true) };
        expect_true!(thread.free_struct());
        expect_true!(thread.flags() & super::THREAD_FLAG_FREE_STRUCT != 0);
        // SAFETY: As above.
        unsafe { thread.set_free_struct(false) };
        expect_false!(thread.free_struct());
        // SAFETY: As above.
        unsafe { thread.set_free_struct(saved & super::THREAD_FLAG_FREE_STRUCT != 0) };
        expect_eq!(thread.free_struct(), saved & super::THREAD_FLAG_FREE_STRUCT != 0);

        // SAFETY: As above.
        unsafe { thread.set_idle(true) };
        expect_true!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_true!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });
        expect_true!(thread.flags() & super::THREAD_FLAG_IDLE != 0);
        // SAFETY: As above.
        unsafe { thread.set_idle(false) };
        expect_false!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_false!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });
        // SAFETY: As above.
        unsafe { thread.set_idle(saved & super::THREAD_FLAG_IDLE != 0) };
        expect_eq!(thread.is_idle(), saved & super::THREAD_FLAG_IDLE != 0);

        thread.set_vcpu(true);
        expect_true!(thread.vcpu());
        expect_true!(thread.flags() & super::THREAD_FLAG_VCPU != 0);
        thread.set_vcpu(false);
        expect_false!(thread.vcpu());
        thread.set_vcpu(saved & super::THREAD_FLAG_VCPU != 0);
        expect_eq!(thread.vcpu(), saved & super::THREAD_FLAG_VCPU != 0);

        // SAFETY: As above.
        unsafe { thread.set_detached(true) };
        expect_true!(thread.detached());
        expect_true!(thread.flags() & super::THREAD_FLAG_DETACHED != 0);
        // SAFETY: As above.
        unsafe { thread.set_detached(false) };
        expect_false!(thread.detached());
        // SAFETY: As above.
        unsafe { thread.set_detached(saved & super::THREAD_FLAG_DETACHED != 0) };
        expect_eq!(thread.detached(), saved & super::THREAD_FLAG_DETACHED != 0);

        expect_eq!(thread.flags(), saved);
        drop(_restore);
        expect_eq!(waiter.join().unwrap_or(ZX_ERR_BAD_STATE), ZX_OK);
    }

    /// Verifies the signal accessors and `check_kill_or_suspend_signal` on a parked helper thread.
    #[test]
    fn test_thread_signals() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx::ten_seconds(&event);
        let waiter = ParkedWaiter::spawn(&ctx);
        assert_true!(waiter.is_some(), "failed to spawn and park the waiter");
        let waiter = waiter.expect("checked above");
        let thread = waiter.thread();
        let initial = thread.signals();
        expect_ok!(thread.check_kill_or_suspend_signal());

        thread.set_signals(super::THREAD_SIGNAL_KILL);
        expect_true!(thread.signals() & super::THREAD_SIGNAL_KILL != 0);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_KILLED);
        thread.clear_signals(super::THREAD_SIGNAL_KILL);
        expect_true!(thread.signals() & super::THREAD_SIGNAL_KILL == 0);

        thread.set_signals(super::THREAD_SIGNAL_SUSPEND);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_RETRY);

        // When both KILL and SUSPEND are raised, KILL takes precedence.
        thread.set_signals(super::THREAD_SIGNAL_KILL);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_KILLED);
        thread.clear_signals(super::THREAD_SIGNAL_KILL | super::THREAD_SIGNAL_SUSPEND);
        expect_ok!(thread.check_kill_or_suspend_signal());
        expect_eq!(thread.signals(), initial);

        expect_eq!(waiter.join().unwrap_or(ZX_ERR_BAD_STATE), ZX_OK);
    }

    /// Verifies `set_name` truncation and zero-fill on the current thread, restoring its name.
    #[test]
    fn test_thread_name() {
        let current_ptr = super::current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &super::Thread = unsafe { &*current_ptr };
        let saved = current.name();
        let _restore = zr::defer(|| {
            current.set_name(saved.as_bytes());
        });

        current.set_name(b"short");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..5].copy_from_slice(b"short");
        let mut buf = [0u8; ZX_MAX_NAME_LEN];
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        let mut ptr_buf = [0u8; ZX_MAX_NAME_LEN];
        // SAFETY: `current_ptr` points to the live current thread.
        unsafe { Thread::name_bytes_ptr(current_ptr, &mut ptr_buf) };
        expect_true!(ptr_buf == expected);
        // SAFETY: `current_ptr` points to the live current thread.
        expect_true!(unsafe { Thread::name_copy_ptr(current_ptr) }.as_bytes() == b"short");
        expect_eq!(super::rust_thread_name(current).addr(), current.name.as_ptr().addr());
        // SAFETY: The buffer is NUL-terminated and nothing else writes the test thread's name.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes() == b"short");

        let exact = [b'a'; ZX_MAX_NAME_LEN - 1];
        current.set_name(&exact);
        let mut expected = [b'a'; ZX_MAX_NAME_LEN];
        expected[ZX_MAX_NAME_LEN - 1] = 0;
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_eq!(current.name().as_bytes().len(), ZX_MAX_NAME_LEN - 1);

        let long = [b'b'; ZX_MAX_NAME_LEN + 8];
        current.set_name(&long);
        let mut expected = [b'b'; ZX_MAX_NAME_LEN];
        expected[ZX_MAX_NAME_LEN - 1] = 0;
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_eq!(cstr.to_bytes().len(), ZX_MAX_NAME_LEN - 1);

        // Embedded NUL bytes and non-UTF-8 bytes are copied verbatim into the buffer.
        current.set_name(b"ab\0cd");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..5].copy_from_slice(b"ab\0cd");
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_true!(current.name().as_bytes() == b"ab");
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes() == b"ab");

        current.set_name(b"a\xffb");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..3].copy_from_slice(b"a\xffb");
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_true!(current.name().as_bytes() == b"a\xffb");

        current.set_name(b"");
        current.name_bytes(&mut buf);
        expect_true!(buf == [0u8; ZX_MAX_NAME_LEN]);
        expect_true!(current.name().as_bytes().is_empty());
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes().is_empty());

        // `rust_thread_set_name` accepts a null pointer when `len == 0`.
        current.set_name(b"nonempty");
        // SAFETY: `current_ptr` has valid `name` storage and `len == 0` does not dereference the
        // name pointer.
        unsafe { super::rust_thread_set_name(current_ptr, core::ptr::null(), 0) };
        current.name_bytes(&mut buf);
        expect_true!(buf == [0u8; ZX_MAX_NAME_LEN]);
    }

    /// Verifies that a kernel thread has a nonzero tid and no pid.
    #[test]
    fn test_thread_tid_pid() {
        let current_ptr = super::current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &super::Thread = unsafe { &*current_ptr };
        expect_true!(current.tid() != 0);
        // SAFETY: `current_ptr` points to the valid current thread.
        expect_eq!(current.tid(), unsafe { super::ThreadPtr::current().tid() });
        expect_eq!(current.pid(), ZX_KOID_INVALID);
        // SAFETY: `current_ptr` points to the valid current thread.
        expect_eq!(current.pid(), unsafe { super::ThreadPtr::current().pid() });
    }

    /// Shared with a helper thread that joins `target` with `deadline` (monotonic) and exits with
    /// the target's retcode, or with the raw status `join` returned on failure; `joined` records
    /// whether the helper's join succeeded (and so consumed `target`).
    struct JoinCtx {
        target: super::ThreadPtr,
        deadline: InstantMono,
        joined: AtomicBool,
    }

    extern "C" fn join_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `JoinCtx` the spawning test keeps alive until it joins us.
        let ctx = unsafe { &*arg.cast::<JoinCtx>() };
        // SAFETY: The spawning test joins `target` itself only if `joined` stays false.
        match unsafe { ctx.target.join(ctx.deadline) } {
            Ok(retcode) => {
                ctx.joined.store(true, Ordering::Release);
                retcode
            }
            Err(status) => status.into_raw(),
        }
    }

    extern "C" fn double_entry(arg: *mut c_void) -> i32 {
        2 * (arg.addr() as i32)
    }

    /// Verifies a created thread's `TaskState` before it runs and its retcode after the trampoline.
    #[test]
    fn test_thread_spawn_and_join() {
        // SAFETY: `double_entry` only uses the value of `arg`.
        let thread = unsafe {
            super::create(
                c"thread_rust_double".as_ptr(),
                double_entry,
                core::ptr::without_provenance_mut(21),
            )
        };
        assert_true!(thread.is_ok(), "failed to create the thread");
        let thread = thread.expect("checked above");
        {
            // SAFETY: The thread is created but not resumed or joined, so its `Thread` is alive
            // and nothing else touches its task state.
            let task_state = unsafe { (*thread.as_ptr()).task_state() };
            expect_true!(task_state.entry().is_some());
            expect_eq!(task_state.arg().addr(), 21);
            // SAFETY: The thread has not started running, so nothing writes the retcode.
            expect_eq!(unsafe { task_state.retcode() }, 0);
        }
        // SAFETY: The thread has not been joined or destroyed.
        unsafe { thread.resume() };
        // SAFETY: The thread is joined exactly once.
        let retcode = unsafe { thread.join(InstantMono::INFINITE) };
        expect_eq!(retcode.unwrap_or(ZX_ERR_BAD_STATE), 42);
    }

    /// Verifies that a joiner blocked in `TaskState::join` is woken with the target's retcode.
    #[test]
    fn test_thread_join_wakes_joiner() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { retcode: 7, ..WaitCtx::ten_seconds(&event) };
        let target = ParkedWaiter::spawn(&ctx);
        assert_true!(target.is_some(), "failed to spawn and park the target");
        let target = target.expect("checked above");

        let join_ctx = JoinCtx {
            target: target.ptr(),
            deadline: ctx.deadline,
            joined: AtomicBool::new(false),
        };
        // SAFETY: `join_ctx` outlives the joiner, which is joined below before returning.
        let joiner = unsafe {
            super::spawn(
                c"thread_rust_joiner".as_ptr(),
                join_entry,
                core::ptr::from_ref(&join_ctx).cast_mut().cast(),
            )
        };
        assert_true!(joiner.is_ok(), "failed to spawn the joiner");
        let joiner = joiner.expect("checked above");
        // No early returns from here until the joiner is joined.
        let joiner_blocked = wait_until_blocked(joiner);
        // Let the target exit; its `Thread::Current::Exit` wakes the joiner with its retcode.
        event.signal();
        // SAFETY: The joiner is joined exactly once.
        let joiner_retcode = unsafe { joiner.join(InstantMono::INFINITE) };
        let target_joined = join_ctx.joined.load(Ordering::Acquire);
        if target_joined {
            target.joined_elsewhere();
        }
        expect_true!(joiner_blocked);
        expect_eq!(joiner_retcode.unwrap_or(ZX_ERR_BAD_STATE), 7);
        expect_true!(target_joined);
    }

    /// Verifies that `TaskState::join` with an already-past deadline fails with `ZX_ERR_TIMED_OUT`.
    #[test]
    fn test_thread_join_timeout() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { retcode: 7, ..WaitCtx::ten_seconds(&event) };
        let target = ParkedWaiter::spawn(&ctx);
        assert_true!(target.is_some(), "failed to spawn and park the target");
        let target = target.expect("checked above");

        let join_ctx = JoinCtx {
            target: target.ptr(),
            deadline: InstantMono(current_mono_time().0 - 1),
            joined: AtomicBool::new(false),
        };
        // SAFETY: `join_ctx` outlives the joiner, which is joined below before returning.
        let joiner = unsafe {
            super::spawn(
                c"thread_rust_joiner".as_ptr(),
                join_entry,
                core::ptr::from_ref(&join_ctx).cast_mut().cast(),
            )
        };
        assert_true!(joiner.is_ok(), "failed to spawn the joiner");
        let joiner = joiner.expect("checked above");
        // SAFETY: The joiner is joined exactly once.
        let joiner_retcode = unsafe { joiner.join(InstantMono::INFINITE) };
        let target_joined = join_ctx.joined.load(Ordering::Acquire);
        // The target is normally still parked; release it and collect its retcode ourselves.
        let target_retcode = if target_joined {
            target.joined_elsewhere();
            None
        } else {
            Some(target.join().unwrap_or(ZX_ERR_BAD_STATE))
        };
        expect_eq!(joiner_retcode.unwrap_or(ZX_ERR_BAD_STATE), ZX_ERR_TIMED_OUT);
        expect_false!(target_joined);
        expect_eq!(target_retcode.unwrap_or(ZX_ERR_BAD_STATE), 7);
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

/// Preemption disable and timeslice extension unit tests (`preempt_disable_tests.cc`).
#[cfg(ktest)]
#[unittest::suite(name = "preempt_disable_tests_rust")]
mod preempt_disable_tests {
    use super::{
        AnnotatedAutoEagerReschedDisabler, AutoEagerReschedDisabler, AutoExpiringPreemptDisabler,
        AutoPreemptDisabler, DEFAULT_PRIORITY, PreemptionState, ThreadLockGuard, ThreadPtr,
        create_with_priority, current_get, current_preemption_state, preempt, reschedule,
        sleep_relative, r#yield,
    };
    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
    use crate::kconcurrent::clt_tag;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::event::Event;
    use crate::kernel::interrupt::{IntHandlerSavedState, int_handler_finish, int_handler_start};
    use crate::kernel::mp::CPU_MASK_ALL;
    use crate::kernel::scheduler;
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t, cpu_num_to_mask};
    use crate::platform_rs::timer::{DurationMono, InstantMono, zx_usec};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};
    use pin_init::{pin_init, stack_pin_init};
    use unittest::{
        assert_eq, assert_ne, assert_true, expect_eq, expect_false, expect_ne, expect_true,
        subtest, unwrap_ok,
    };
    use zx_types::zx_instant_mono_t;

    /// Mirror of C++ `PreemptDisableTestAccess::State`.
    struct State {
        preempts_pending: cpu_mask_t,
    }

    /// Mirror of C++ `PreemptDisableTestAccess::SaveState`.
    fn save_state(preemption_state: &PreemptionState) -> State {
        State { preempts_pending: preemption_state.preempts_pending() }
    }

    /// Mirror of C++ `PreemptDisableTestAccess::RestoreState`.
    fn restore_state(preemption_state: &PreemptionState, state: State) {
        preemption_state.preempts_pending.store(state.preempts_pending);
    }

    /// Mirror of C++ `PreemptDisableTestAccess::ClearPending`.
    fn clear_pending(preemption_state: &PreemptionState) {
        preemption_state.preempts_pending.store(0);
    }

    /// Test that PreemptDisable is set for timer callbacks and that, in this
    /// context, preempts_pending will get set by some functions.
    ///
    /// # Safety
    ///
    /// `arg` must point to the `Event` that `test_in_timer_callback` keeps alive until the timer
    /// has been cancelled, and this must run as a `Timer` callback (interrupt context).
    unsafe extern "C" fn timer_callback_func(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` is the pinned `Event` that `test_in_timer_callback` keeps alive until the
        // timer has been cancelled.
        let event = unsafe { &*arg.cast::<Event>() };

        // The timer should run in interrupt context.  (`assert!` panics like the C++ `ASSERT`;
        // the unittest `expect_*` macros are only usable from the test body itself.)
        assert!(crate::arch_rs::ints_disabled());
        assert!(crate::arch_rs::blocking_disallowed());

        // Entry into interrupt context should disable preemption and eager
        // reschedules.
        // SAFETY: The interrupted thread is the current thread and stays alive for the callback;
        // `PreemptionState` is all relaxed atomics.
        let preemption_state = unsafe { &*current_preemption_state() };
        assert!(preemption_state.preempt_disable_count() > 0);
        assert!(preemption_state.eager_resched_disable_count() > 0);
        let state = save_state(preemption_state);

        // Test that Scheduler::Reschedule() sets the preempt_pending flag when
        // PreemptDisable is set.
        clear_pending(preemption_state);
        assert!(preemption_state.preempts_pending() == 0);
        {
            let current_thread = current_get();
            // SAFETY: `current_thread` is the live current thread, no chain lock transaction is
            // active in this timer callback, and the guard is stack-pinned and dropped in this
            // scope without blocking.
            ksync::lock!(
                let _guard = unsafe {
                    ThreadLockGuard::lock(
                        current_thread,
                        clt_tag!("PreemptDisableTests timer_callback"),
                    )
                }
            );
            // SAFETY: `current_thread` is the current CPU's active thread, `_guard` holds its lock
            // as the sole lock of a finalized transaction with interrupts disabled, and no
            // `queue_lock_` is held in a timer callback.
            unsafe { scheduler::reschedule(current_thread) };
        }
        assert!(preemption_state.preempts_pending() != 0);

        // Test that preemption_state.PreemptSetPending() sets preempts_pending.
        clear_pending(preemption_state);
        assert!(preemption_state.preempts_pending() == 0);

        preemption_state.preempt_set_pending(cpu_num_to_mask(curr_cpu_num()));
        assert!(preemption_state.preempts_pending() != 0);

        restore_state(preemption_state, state);
        event.signal();
    }

    /// Schedule a timer callback and wait for it to complete.
    //
    // Most of the testing is done in the timer callback.
    #[test]
    fn test_in_timer_callback() {
        stack_pin_init!(let event = Event::init_unsignaled());
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));

        // SAFETY: `timer` and `event` are stack-pinned and outlive the timer, which `_cancel`
        // cancels before either goes out of scope (also on an early return from a failed
        // assertion); `timer_callback_func` is a valid timer callback.
        unsafe {
            timer.as_ref().set_oneshot(
                0,
                timer_callback_func,
                core::ptr::from_ref(&*event).cast_mut().cast(),
            );
        }
        // Make sure the timer has fully completed prior to letting it go out of scope.
        let _cancel = zr::defer(|| {
            timer.cancel();
        });
        assert_true!(event.wait_infinite().is_ok());
    }

    /// Test incrementing and decrementing the PreemptDisable and EagerReschedDisable counts.
    #[test]
    fn test_inc_dec_disable_counts() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // Test initial conditions.
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());
        // While preemption is allowed, a preemption should not be pending.
        expect_eq!(preemption_state.preempts_pending(), 0);

        // Test incrementing and decrementing of PreemptDisable.
        preemption_state.preempt_disable();
        expect_eq!(preemption_state.preempt_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: `preemption_state` is the current thread's, tests may block, this pairs with
        // the `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test incrementing and decrementing of EagerReschedDisable.
        preemption_state.eager_resched_disable();
        expect_eq!(preemption_state.eager_resched_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: `preemption_state` is the current thread's, tests may block, this pairs with
        // the `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test nesting: multiple increments and decrements.
        preemption_state.preempt_disable();
        preemption_state.preempt_disable();
        expect_eq!(preemption_state.preempt_disable_count(), 2);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above; each call pairs with one of the two `preempt_disable()`s above.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test nesting: multiple increments and decrements.
        preemption_state.eager_resched_disable();
        preemption_state.eager_resched_disable();
        expect_eq!(preemption_state.eager_resched_disable_count(), 2);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above; each call pairs with one of the two `eager_resched_disable()`s above.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Every disable above is paired with its reenable and `expect_*` never returns early, so
        // the counts are back to zero however the checks went.
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Mirror of C++ `test_decrement_clears_preempt_pending`: reenabling flushes pending bits.
    #[test]
    fn test_decrement_clears_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        assert_true!(preemption_state.preempt_is_enabled());
        assert_eq!(preemption_state.preempts_pending(), 0);

        // Test that preemption_state.preempt_reenable() clears preempt_pending.
        preemption_state.preempt_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);

        // Test that preemption_state.eager_resched_reenable() clears preempt_pending.
        preemption_state.eager_resched_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Mirror of C++ `test_blocking_clears_preempt_pending`: blocking clears pending bits.
    #[test]
    fn test_blocking_clears_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // It is OK to block while preemption is disabled. In this case, blocking
        // should clear a pending local preemption.
        preemption_state.preempt_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        {
            let _int_state = InterruptDisableGuard::new();
            let _ = sleep_relative(DurationMono::from_millis(10));
            // Read preempts_pending with interrupts disabled because otherwise an
            // interrupt handler could set it.
            expect_eq!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };

        // It is OK to block while eager rescheduling is disabled. In this case,
        // blocking should clear all pending preemptions.
        preemption_state.eager_resched_disable();
        reschedule();
        {
            let _int_state = InterruptDisableGuard::new();
            let _ = sleep_relative(DurationMono::from_millis(10));
            // Read preempts_pending with interrupts disabled because otherwise an
            // interrupt handler could set it.
            expect_eq!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
    }

    /// Mirror of C++ `test_interrupt_preserves_preempt_pending`.
    //
    // Test that preempts_pending is preserved across an interrupt handler when
    // EagerReschedDisable is set and when the interrupt handler does not cause a
    // preemption. This tests the int_handler_start()/finish() routines rather than
    // the full interrupt handler.
    #[test]
    fn test_interrupt_preserves_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        preemption_state.eager_resched_disable();
        {
            // Do this with interrupts disabled so that a real interrupt does not
            // clear preempts_pending.
            let _int_state = InterruptDisableGuard::new();
            reschedule();

            // Simulate an interrupt handler invocation.
            let mut state = IntHandlerSavedState::default();
            int_handler_start(&mut state);
            expect_eq!(preemption_state.preempt_disable_count(), 1);
            let do_preempt = int_handler_finish(&mut state);

            expect_false!(do_preempt);
            expect_ne!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Timer callback used for testing.
    ///
    /// # Safety
    ///
    /// `arg` must point to the `AtomicBool` that `test_interrupt_with_preempt_disable` keeps alive
    /// until the timer has been cancelled, and this must run as a `Timer` callback.
    unsafe extern "C" fn timer_set_preempt_pending(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` is the `AtomicBool` that `test_interrupt_with_preempt_disable` keeps
        // alive until the timer has been cancelled.
        let timer_ran = unsafe { &*arg.cast::<AtomicBool>() };

        // SAFETY: The interrupted thread is the current thread and stays alive for the callback;
        // `PreemptionState` is all relaxed atomics.
        unsafe {
            (*current_preemption_state()).preempt_set_pending(cpu_num_to_mask(curr_cpu_num()))
        };
        timer_ran.store(true, Ordering::SeqCst);
    }

    /// Mirror of C++ `test_interrupt_with_preempt_disable`.
    #[test]
    fn test_interrupt_with_preempt_disable() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // Test that interrupt handlers honor PreemptDisable.
        //
        // We test that by setting a timer callback that will set
        // preempt_pending from inside an interrupt handler.  preempt_pending
        // should remain set after the interrupt handler returns.
        //
        // This assumes that timer_set() will run the callback on the same CPU
        // that we invoked it from.  This also assumes that we don't
        // accidentally call any blocking operations that cause our thread to
        // be rescheduled to another CPU.
        preemption_state.preempt_disable();
        let timer_ran = AtomicBool::new(false);
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::after_mono(DurationMono(zx_usec(100)), TimerSlack::none());
        // SAFETY: `timer` is stack-pinned and `timer_ran` outlives it: `_cancel` cancels the timer
        // before either goes out of scope (also on an early return from a failed assertion);
        // `timer_set_preempt_pending` is a valid timer callback.
        unsafe {
            timer.as_ref().set_deadline(
                &deadline,
                timer_set_preempt_pending,
                core::ptr::from_ref(&timer_ran).cast_mut().cast(),
            );
        }
        // Make sure the timer has fully completed prior to letting it go out of scope.
        let _cancel = zr::defer(|| {
            timer.cancel();
        });
        // Spin until timer_ran is set by the interrupt handler.
        while !timer_ran.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        expect_eq!(preemption_state.preempts_pending(), cpu_num_to_mask(curr_cpu_num()));
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
    }

    /// Shared with the `waiter` thread of `test_local_preempt_pending`.
    struct Args {
        event: *const Event,
        started: AtomicBool,
    }

    extern "C" fn waiter(void_args: *mut c_void) -> i32 {
        // SAFETY: `void_args` is the `Args` that `setup_and_run_with` keeps alive (along with the
        // `Event` it points at) until it has joined this thread.
        let (args, event) = unsafe {
            let args = &*void_args.cast::<Args>();
            (args, &*args.event)
        };
        // Let the other thread know that we're up and running and then wait to be signaled.
        args.started.store(true, Ordering::SeqCst);
        let _ = event.wait_infinite();
        0
    }

    /// The common code used in all cases of `test_local_preempt_pending`.
    ///
    /// `setup_and_run_with` is used to setup test conditions and run the `func` test case.
    ///
    /// `func` receives an Event that it should signal to unblock the `waiter` thread (see above).
    fn setup_and_run_with(func: &dyn Fn(&Event) -> bool) -> bool {
        (subtest!(|func: &dyn Fn(&Event) -> bool| {
            // Make sure we restore this thread's affinity.
            // SAFETY: Threading is initialized by the time unit tests run.
            let current = unsafe { ThreadPtr::current() };
            // SAFETY: The current thread is alive for the duration of this test.
            let affinity = unsafe { current.get_cpu_affinity() };
            let _cleanup = zr::defer(move || {
                // SAFETY: The current thread is alive for the duration of this test.
                unsafe { current.set_cpu_affinity(affinity) };
            });

            stack_pin_init!(let event = Event::init_unsignaled());
            let args =
                Args { event: core::ptr::from_ref(&*event), started: AtomicBool::new(false) };

            let target_cpu = BOOT_CPU_ID;

            // Migrate the current thread to the target CPU and bind a |waiter| thread to the same
            // CPU.
            let mask = cpu_num_to_mask(target_cpu);
            // SAFETY: The current thread is alive for the duration of this test.
            unsafe { current.set_cpu_affinity(mask) };
            // SAFETY: `waiter` is a valid thread entry point, and `args` and `event` outlive the
            // thread, which `_cleanup_waiter` joins below.
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"test_local_preempt_pending".as_ptr(),
                    waiter,
                    core::ptr::from_ref(&args).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            });
            let _cleanup_waiter = zr::defer(|| {
                event.signal();
                // SAFETY: `t` is joined exactly once, here.
                let _ = unsafe { t.join(InstantMono::INFINITE) };
            });
            // SAFETY: `t` has not been joined yet.
            unsafe { t.set_cpu_affinity(mask) };

            // Start the |waiter| and spin until we know that it has started running and then
            // blocked.
            // SAFETY: `t` was just created and has not been resumed or joined yet.
            unsafe { t.resume() };
            loop {
                r#yield();
                // SAFETY: `t` has not been joined yet.  (`is_blocked` checks the scheduler state
                // under `t`'s lock, as the C++ `SingleChainLockGuard` scope does.)
                if args.started.load(Ordering::SeqCst) && unsafe { t.is_blocked() } {
                    break;
                }
            }

            // At this point we know the |waiter| is blocked on the event.
            expect_true!(func(&event));
        }))(func)
    }

    /// Unblocking a thread pinned to the current CPU marks the CPU for preemption.
    //
    // Verify that in certain contexts where preemption cannot immediately occur, unblocking a
    // thread pinned to the current CPU will mark the CPU for preemption.
    //
    // This test covers four cases:
    //
    // 1. preemption is disabled
    // 2. eager resched is disabled
    // 3. timeslice extension
    // 4. a spinlock is held
    //
    // See https://fxbug.dev/42051271 for motivation.
    #[test]
    fn test_local_preempt_pending() {
        // Now test each case using the common code above.

        // 1. Preemption disabled should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _apd = AutoPreemptDisabler::new();
            // Unblock the |waiter|.  Because we've got preemption disabled, a preemption event
            // for the local CPU should become pending.
            event.signal();
            // SAFETY: The current thread stays alive for the duration of this test, and
            // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 2. Eager resched disabled should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _aerd = AutoEagerReschedDisabler::new();
            // Unblock the |waiter|.  Because we've got eager resched disabled (which implies
            // preempt disable), a preemption event for the local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 3. A timeslice extension should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            // Unblock the |waiter|.  Because we've got eager resched disabled (which implies
            // preempt disable), a preemption event for the local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 4. Holding a spinlock should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            ksync::declare_singleton_monitored_spinlock!(LocalLock);
            ksync::lock!(let _guard = LocalLock::get().lock_with(ksync::source_tag!()));
            // Unblock the |waiter|.  Because we're holding a spinlock, a preemption event for the
            // local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));
    }

    /// Mirror of C++ `test_evaluate_timeslice_extension`: extension activation and expiry.
    #[test]
    fn test_evaluate_timeslice_extension() {
        // Nothing preventing preemption.
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        expect_true!(preemption_state.preempt_is_enabled());
        expect_true!(preemption_state.evaluate_timeslice_extension());
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by count).
        {
            let _apd = AutoPreemptDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by eager resched count).
        {
            let _aerd = AnnotatedAutoEagerReschedDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by infinite timeslice extension).
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // In the tests below, the current thread will defer preemption for
        // EPSILON_DURATION and sleep for EPSILON_DURATION.  The only requirement for
        // correctness is that this value is greater than zero.  We use the value 1 to
        // minimize test runtime.
        const EPSILON_DURATION: DurationMono = DurationMono(1);

        // See that the timeslice extension expires.
        {
            let _guard = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            // Note, we cannot reliably assert that preemption is disabled at this point
            // because a preemption request may have already occurred and
            // EPSILON_DURATION may have already elapsed.
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_true!(preemption_state.evaluate_timeslice_extension());
            expect_true!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // AutoPreemptDisabler inside an expired AutoExpiringPreemptDisabler.
        {
            let _guard1 = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            let _guard2 = AutoPreemptDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_false!(preemption_state.evaluate_timeslice_extension());
            // Still false because of the APD.
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // AutoEagerReschedDisabler inside an expired AutoExpiringPreemptDisabler.
        {
            let _guard1 = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            let _guard2 = AnnotatedAutoEagerReschedDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_false!(preemption_state.evaluate_timeslice_extension());
            // Still false because of the AERD.
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Holder for the `CriticalMutex` of `test_apd_in_critical_mutex`.
    #[ksync::guarded]
    struct CriticalMutexHolder {
        #[mutex]
        mutex: ksync::KMutex<ksync::RawCriticalMutex>,
    }

    /// An AutoPreemptDisabler inside a CriticalMutex does not clear the mutex's extension.
    //
    // Verify that an AutoPreemptDisabler inside a CriticalMutex with a non-expired
    // timeslice extension does not clear the CriticalMutex's extension.
    #[test]
    fn test_apd_in_critical_mutex() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        stack_pin_init!(let holder = pin_init!(CriticalMutexHolder {
            mutex <- ksync::KMutex::init(),
        }));

        {
            // `KMutex<RawCriticalMutex>`'s guard is the `Acquire`/`Release(should_clear)` pair.
            ksync::lock!(let _guard = holder.lock_mutex());

            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());

            {
                let _apd = AutoPreemptDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());
                expect_false!(preemption_state.evaluate_timeslice_extension());
                expect_false!(preemption_state.preempt_is_enabled());
            }
        }
    }

    /// Mirror of C++ `test_flush_race`: reenabling with an inactive timeslice extension in place.
    #[test]
    fn test_flush_race() {
        // This test simulates a race condition where a preemption is requested (via IPI
        // or timer) concurrent with reenabling preemption / eager rescheduling while an
        // inactive timeslice extension is in place.
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        expect_true!(preemption_state.preempt_is_enabled());

        // In each block below, dropping `_guard` clears the extension with the counts at zero and
        // so flushes the pending local bit the block left behind, as in C++.

        // Test PreemptReenable.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            {
                let _apd = AutoPreemptDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());

                // We'll simulate a race by marking the current CPU as pending for
                // preemption without going through the normal PreemptSetPending path
                // because we want to test the behavior when the extension is
                // inactive, and PreemptSetPending will activate it.
                preemption_state.preempts_pending_add(curr_mask);
                // When we leave this scope and reenable preemption, we'd better not flush.
            }
            expect_false!(preemption_state.preempt_is_enabled());
            // See that we did not flush.
            expect_eq!(curr_mask, preemption_state.preempts_pending());
        }

        // Test PreemptReenableDelayFlush.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            let should_preempt = {
                let _irqd = InterruptDisableGuard::new();
                crate::arch_rs::set_blocking_disallowed(true);
                let _cleanup = zr::defer(|| crate::arch_rs::set_blocking_disallowed(false));
                preemption_state.preempt_disable();
                preemption_state.preempts_pending_add(curr_mask);
                // SAFETY: This pairs with the `preempt_disable()` just above, and no
                // `PreemptDisabledToken` exists in this test.
                unsafe { preemption_state.preempt_reenable_delay_flush() }
            };
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(should_preempt);
        }

        // Test EagerReschedReenable.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            {
                let _aerd = AnnotatedAutoEagerReschedDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());
                preemption_state.preempts_pending_add(CPU_MASK_ALL);
            }
            expect_false!(preemption_state.preempt_is_enabled());
            // See that we flushed the remote CPUs, but not the local.
            expect_eq!(curr_mask, preemption_state.preempts_pending());
        }

        expect_true!(preemption_state.preempt_is_enabled());
        expect_eq!(preemption_state.preempts_pending(), 0);
        expect_false!(crate::arch_rs::blocking_disallowed());
    }

    /// Verify that it's legal to call preempt while holding a spinlock.
    //
    // This is a regression test for https://fxbug.dev/473600952.
    #[test]
    fn test_preempt_with_spinlock() {
        ksync::declare_singleton_monitored_spinlock!(SpinlockForTest);

        {
            ksync::lock!(let _guard = SpinlockForTest::get().lock_with(ksync::source_tag!()));
            preempt();
            // SAFETY: The current thread stays alive for the duration of this test, and
            // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            assert_ne!(0, pending & cpu_num_to_mask(curr_cpu_num()));
        }
    }
}
