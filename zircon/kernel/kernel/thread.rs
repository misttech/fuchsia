// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::ffi::{c_char, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{PinInit, pin_data, pinned_drop};
use thread_lock_guard_bindings as lock_guard_bindings;
use zr::Opaque;
use zx_status::Status;
use zx_types::{zx_instant_mono_t, zx_status_t};

use crate::kconcurrent::{ChainLock, ChainLockTransactionToken};
use crate::kernel::restricted_state::RestrictedState;
use crate::kernel::scheduler_state::{SchedulerState, SchedulerStateBaseProfile};
use crate::kernel::types::{cpu_mask_t, cpu_num_t};
use crate::ktrace_rs::InternedString;
use crate::vm::vm_aspace::VmAspace;

pub use crate::kconcurrent::CallsiteInfo as CltTag;

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
    fn cpp_thread_resume(thread: *mut Thread);
    fn cpp_thread_join(
        thread: *mut Thread,
        out_retcode: *mut i32,
        deadline: zx_instant_mono_t,
    ) -> i32;
    fn cpp_thread_current_yield();
    fn cpp_thread_kill(thread: *mut Thread);
    fn cpp_thread_suspend(thread: *mut Thread) -> zx_status_t;
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
#[repr(C, align(8))]
pub struct Thread {
    _private: [u8; 0],
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
    phantom: PhantomData<PhantomPinned>,
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

/// RAII helper that automatically manages disabling and re-enabling preemption.
///
/// When the object goes out of scope, it automatically re-enables preemption if it had been
/// previously disabled by the instance.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AutoPreemptDisabler {
    disabled: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoPreemptDisabler {
    /// Creates a new guard and immediately disables preemption.
    pub fn new() -> Self {
        preempt_disable();
        Self { disabled: true, _marker: PhantomData }
    }

    /// Creates a new guard without immediately disabling preemption.
    pub fn new_deferred() -> Self {
        Self { disabled: false, _marker: PhantomData }
    }

    /// Disables preemption if it was not disabled by this instance already.
    pub fn disable(&mut self) {
        if !self.disabled {
            preempt_disable();
            self.disabled = true;
        }
    }

    /// Enables preemption if it was previously disabled by this instance.
    pub fn enable(&mut self) {
        if self.disabled {
            preempt_reenable();
            self.disabled = false;
        }
    }

    /// Returns whether preemption is currently disabled by this guard instance.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }
}

impl Default for AutoPreemptDisabler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoPreemptDisabler {
    fn drop(&mut self) {
        self.enable();
    }
}

/// RAII helper that defers preemption of the current thread until either `duration` nanoseconds
/// after preemption is requested or the object is destroyed, whichever comes first.
///
/// This guard is `!Send` and `!Sync` because timeslice extensions modify CPU- and thread-local
/// state.
pub struct AutoExpiringPreemptDisabler {
    should_clear: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoExpiringPreemptDisabler {
    /// Default timeslice extension duration (150us), matching C++
    /// `Mutex::DEFAULT_TIMESLICE_EXTENSION`.
    pub const DEFAULT_TIMESLICE_EXTENSION: DurationMono = DurationMono::from_micros(150);

    /// Creates a new guard and attempts to set a timeslice extension for `duration`.
    pub fn new(duration: DurationMono) -> Self {
        let should_clear = preempt_set_timeslice_extension(duration);
        Self { should_clear, _marker: PhantomData }
    }

    /// Creates a new guard with the default timeslice extension duration.
    pub fn with_default_timeslice_extension() -> Self {
        Self::new(Self::DEFAULT_TIMESLICE_EXTENSION)
    }
}

impl Drop for AutoExpiringPreemptDisabler {
    fn drop(&mut self) {
        if self.should_clear {
            preempt_clear_timeslice_extension();
        }
    }
}

/// RAII guard that disables both local preemption and eager remote rescheduling (IPIs) for its
/// scope, matching C++ `AnnotatedAutoEagerReschedDisabler`.
///
/// The C++ type additionally acquires the `preempt_disabled_token` capability, which only exists
/// for clang's static thread-safety analysis and has no runtime effect, so this is equivalent to a
/// balanced pair of [`eager_resched_disable`] and [`eager_resched_reenable`] calls.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AnnotatedAutoEagerReschedDisabler {
    disabled: bool,
    _marker: PhantomData<*mut ()>,
}

impl AnnotatedAutoEagerReschedDisabler {
    /// Creates a new guard and immediately disables eager rescheduling.
    #[inline]
    pub fn new() -> Self {
        eager_resched_disable();
        Self { disabled: true, _marker: PhantomData }
    }

    /// Re-enables eager rescheduling if previously disabled by this guard instance.
    #[inline]
    pub fn enable(&mut self) {
        if self.disabled {
            self.disabled = false;
            eager_resched_reenable();
        }
    }
}

impl Default for AnnotatedAutoEagerReschedDisabler {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AnnotatedAutoEagerReschedDisabler {
    #[inline]
    fn drop(&mut self) {
        self.enable();
    }
}

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
    use super::{AnnotatedAutoEagerReschedDisabler, ThreadPtr, eager_resched_disable_count};
    use crate::kconcurrent::{IRQ_SAVE_OPTION, SingleChainLockGuard, clt_tag};
    use unittest::{assert_eq, assert_ne, assert_true};

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
}

