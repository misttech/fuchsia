// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{
    Blocked, CoreThreadObservation, ThreadDispatcher, ThreadDispatcherState,
};
use crate::arch_rs::{ArchExceptionContext, UserEntryState};
use crate::kernel::scheduler_state::ThreadStateKind;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{
    zx_exception_context_t, zx_excp_type_t, zx_info_task_runtime_t, zx_koid_t, zx_rights_t,
    zx_status_t,
};

unsafe extern "C" {
    /// Returns a raw pointer to the current thread dispatcher.
    ///
    /// # Safety
    ///
    /// Must only be called when executing within a valid user thread context.
    pub(crate) fn cpp_thread_dispatcher_current() -> *mut ThreadDispatcher;

    /// Checks if the given ThreadDispatcher is the current thread.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_dispatcher_is_current(thread: *const ThreadDispatcher) -> bool;

    /// Acquires `thread`'s critical mutex and returns whether the timeslice extension should be
    /// cleared on release.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_dispatcher_lock_acquire(thread: *mut ThreadDispatcher) -> bool;

    /// Releases `thread`'s critical mutex.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` whose lock was acquired by the caller.
    pub(crate) fn cpp_thread_dispatcher_lock_release(
        thread: *mut ThreadDispatcher,
        should_clear: bool,
    );

    /// Releases a `CoreThreadObservation` via `CoreThreadObservation::Release()`.
    ///
    /// # Safety
    ///
    /// `obs` must point to a valid `CoreThreadObservation`.
    pub(crate) fn cpp_thread_dispatcher_release_core_thread_observation(
        obs: *mut CoreThreadObservation,
    );

    /// Allocates and constructs a ThreadDispatcher.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` whose reference count was transferred.
    /// `out_handle` must point to valid uninitialized memory for a
    /// `KernelHandle<ThreadDispatcher>`.
    pub(crate) fn cpp_thread_dispatcher_alloc(
        process: *mut ProcessDispatcher,
        flags: u32,
        out_handle: *mut MaybeUninit<KernelHandle<ThreadDispatcher>>,
    ) -> zx_status_t;

    /// Stores the lower level kernel thread on a ThreadDispatcher.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`, and `core_thread` must point to a
    /// valid kernel `Thread`.
    pub(crate) fn cpp_thread_dispatcher_set_core_thread(
        thread: *mut ThreadDispatcher,
        core_thread: *mut crate::kernel::thread::Thread,
    );

    /// Loads the lower level kernel thread from a ThreadDispatcher, which is null once
    /// `ExitingCurrent()` has cleared it.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`, and the caller must hold that
    /// dispatcher's state lock: `core_thread_` is `TA_GUARDED(get_lock())` on the C++ side.
    pub(crate) fn cpp_thread_dispatcher_get_core_thread(
        thread: *const ThreadDispatcher,
    ) -> *mut crate::kernel::thread::Thread;

    /// Stores the register values userspace will start executing with.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` and `entry` to a valid
    /// `UserEntryState`.
    pub(crate) fn cpp_thread_dispatcher_set_user_entry(
        thread: *mut ThreadDispatcher,
        entry: *const UserEntryState,
    );

    /// Attaches a ThreadDispatcher to its lower level kernel thread, taking out an additional
    /// reference on the dispatcher which the kernel thread holds until it exits.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` and `core_thread` must be that
    /// dispatcher's live kernel thread.
    pub(crate) fn cpp_thread_dispatcher_attach_to_core_thread(
        thread: *mut ThreadDispatcher,
        core_thread: *mut crate::kernel::thread::Thread,
    );

    /// Enters userspace on a newly created thread. Does not return.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_start_routine_entry(thread: *mut core::ffi::c_void) -> i32;

    /// Sends the `ZX_EXCP_THREAD_EXITING` single-shot exception to the process debugger.
    ///
    /// TRANSITIONAL: while `arch_exception_context_t` remains a C++ type on x86_64 and arm64.
    ///
    /// # Safety
    ///
    /// `process` and `thread` must be valid `ProcessDispatcher` and `ThreadDispatcher` pointers.
    pub(crate) fn cpp_thread_handle_single_shot_exiting(
        process: *mut ProcessDispatcher,
        thread: *mut ThreadDispatcher,
    );
    /// Takes the core thread spinlock and clears the linked core thread.
    ///
    /// TRANSITIONAL: while `core_thread_` remains a C++ field.
    ///
    /// # Safety
    ///
    /// `thread` must be a valid `ThreadDispatcher` pointer, and the caller must hold the
    /// dispatcher's state lock (`ThreadDispatcherState::lock`).
    pub(crate) fn cpp_thread_dispatcher_reset_core_thread(thread: *mut ThreadDispatcher);
    /// Initializes the given opaque runtime stats storage.
    ///
    /// # Safety
    ///
    /// `storage` must be suitably aligned and sized.
    pub(crate) fn cpp_thread_runtime_stats_init(storage: *mut core::ffi::c_void);

    /// Updates the thread runtime stats.
    ///
    /// # Safety
    ///
    /// `storage` must point to initialized ThreadRuntimeStats, and interrupts must be disabled.
    pub(crate) fn cpp_thread_runtime_stats_update(
        storage: *mut core::ffi::c_void,
        new_state: ThreadStateKind,
    );
    /// Adds page fault ticks to runtime stats.
    ///
    /// # Safety
    ///
    /// `storage` must point to initialized ThreadRuntimeStats.
    pub(crate) fn cpp_thread_runtime_stats_add_page_fault_ticks(
        storage: *mut core::ffi::c_void,
        ticks: i64,
    );
    /// Adds lock contention ticks to runtime stats.
    ///
    /// # Safety
    ///
    /// `storage` must point to initialized ThreadRuntimeStats.
    pub(crate) fn cpp_thread_runtime_stats_add_lock_contention_ticks(
        storage: *mut core::ffi::c_void,
        ticks: i64,
    );
    /// Gets compensated runtime stats.
    ///
    /// # Safety
    ///
    /// `storage` must point to initialized ThreadRuntimeStats. `out_stats` must point to
    /// valid uninitialized memory for `zx_info_task_runtime_t`.
    pub(crate) fn cpp_thread_runtime_stats_get_compensated(
        storage: *mut core::ffi::c_void,
        out_stats: *mut core::mem::MaybeUninit<zx_info_task_runtime_t>,
    );

    /// Reads thread architectural state.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `buffer` must point to at least `buffer_size` writable bytes.
    pub(crate) fn cpp_thread_dispatcher_read_state(
        thread: *mut ThreadDispatcher,
        state_kind: u32,
        buffer: *mut core::ffi::c_void,
        buffer_size: usize,
    ) -> zx_status_t;

    /// Writes thread architectural state.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `buffer` must point to at least `buffer_size` readable bytes.
    pub(crate) fn cpp_thread_dispatcher_write_state(
        thread: *mut ThreadDispatcher,
        state_kind: u32,
        buffer: *const core::ffi::c_void,
        buffer_size: usize,
    ) -> zx_status_t;

    /// Raises an exception for job debugger.
    ///
    /// # Safety
    ///
    /// `user_context` must point to a valid initialized `zx_exception_context_t`.
    pub(crate) fn cpp_sys_thread_raise_exception(
        options: u32,
        exception_type: zx_excp_type_t,
        user_context: &zx_exception_context_t,
    ) -> zx_status_t;

    /// Legacy thread yield.
    ///
    /// # Safety
    ///
    /// Yields execution of the current thread to the scheduler.
    pub(crate) fn cpp_sys_thread_legacy_yield(options: u32) -> zx_status_t;
}

// Exported FFI functions for C++ callers:

/// Creates a new ThreadDispatcher.
///
/// # Safety
///
/// `process` must point to a valid `ProcessDispatcher` whose reference count was transferred.
/// `name_ptr` must point to at least `name_len` readable bytes.
/// `out_handle` and `out_rights` must point to valid uninitialized memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_create(
    process: *mut ProcessDispatcher,
    flags: u32,
    name_ptr: *const core::ffi::c_char,
    name_len: usize,
    out_handle: *mut MaybeUninit<KernelHandle<ThreadDispatcher>>,
    out_rights: *mut MaybeUninit<zx_rights_t>,
) -> zx_status_t {
    // SAFETY: The caller transferred a reference count on `process`.
    let process_ref = unsafe { RefPtr::from_raw(process) };
    // SAFETY: The caller guarantees `name_ptr` points to `name_len` readable bytes
    // whenever `name_len > 0`.
    let name_slice = unsafe { zr::slice_from_raw_parts(name_ptr.cast(), name_len) };
    match ThreadDispatcher::create(process_ref, flags, name_slice) {
        Ok((handle, rights)) => {
            // SAFETY: The caller guarantees both out pointers are valid for writes.
            unsafe {
                (*out_handle).write(handle);
                (*out_rights).write(rights);
            }
            zx_types::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Initializes `ThreadDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `ThreadDispatcherState`.
/// `process` must point to a valid `ProcessDispatcher` carrying an acquired refcount.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_state_init(
    state: *mut ThreadDispatcherState,
    disp: &ThreadDispatcher,
    process: *mut ProcessDispatcher,
) {
    // SAFETY: The caller transferred a reference count on `process` and guarantees `state`
    // points to uninitialized storage of the right size and alignment.
    unsafe {
        let process_ref = RefPtr::from_raw(process);
        let _ =
            pin_init::PinInit::__pinned_init(ThreadDispatcherState::init(disp, process_ref), state);
    }
}

/// Returns the related process koid for this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_related_koid(thread: &ThreadDispatcher) -> zx_koid_t {
    thread.get_related_koid()
}

/// Returns the parent process of this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_process(
    thread: &ThreadDispatcher,
) -> *mut ProcessDispatcher {
    thread.process() as *const ProcessDispatcher as *mut ProcessDispatcher
}

/// Asserts that the thread dispatcher's canary is valid.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_assert_canary(thread: &ThreadDispatcher) {
    thread.assert_canary();
}

/// Completes initialization of a newly constructed thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_initialize(thread: &ThreadDispatcher) -> zx_status_t {
    Status::result_into_raw(thread.initialize())
}

/// Starts this thread running inside its parent process with the provided entry state.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_start(
    thread: &ThreadDispatcher,
    entry: &UserEntryState,
    ensure_initial_thread: bool,
) -> zx_status_t {
    Status::result_into_raw(thread.start(*entry, ensure_initial_thread))
}

/// Transitions a thread from the initialized state to either running or suspended.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_make_runnable(
    thread: &ThreadDispatcher,
    entry: &UserEntryState,
    suspended: bool,
) -> zx_status_t {
    Status::result_into_raw(thread.make_runnable(*entry, suspended))
}

/// Kills this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_kill(thread: &ThreadDispatcher) {
    thread.kill();
}

/// Suspends this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_suspend(thread: &ThreadDispatcher) -> zx_status_t {
    Status::result_into_raw(thread.suspend())
}

/// Resumes this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_resume(thread: &ThreadDispatcher) {
    thread.resume();
}

/// Callback from the kernel when the thread is suspending.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_suspending(thread: &ThreadDispatcher) {
    thread.suspending();
}

/// Callback from the kernel when the thread is resuming.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_resuming(thread: &ThreadDispatcher) {
    thread.resuming();
}
/// Returns whether the thread is dying or dead.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_is_dying_or_dead(thread: &ThreadDispatcher) -> bool {
    thread.is_dying_or_dead()
}

/// Returns a pointer to the thread's runtime stats storage.
///
/// TRANSITIONAL: while `ThreadRuntimeStats` remains a C++ type, the `ThreadDispatcher` C++
/// inlines (`UpdateRuntimeStats`, `AddPageFaultTicks`, `AddLockContentionTicks`,
/// `GetCompensatedTaskRuntimeStats`) resolve the object through this entry and call its methods
/// directly. The canary check the C++ methods performed lives here.
///
/// # Safety
///
/// `thread` must be a valid reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_get_runtime_stats(
    thread: &ThreadDispatcher,
) -> *mut core::ffi::c_void {
    thread.assert_canary();
    thread.state().runtime_stats.as_void_ptr()
}

/// Called when the current thread is exiting.
///
/// # Safety
///
/// Must only be called on the current running thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_exiting_current(thread: &ThreadDispatcher) {
    thread.exiting_current();
}

/// Sets the blocked reason of the current thread and returns the previous one.
///
/// # Safety
///
/// The calling thread must be a user thread, so that `Thread::Current::Get()->user_thread()` is
/// its live `ThreadDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_set_blocked_reason(reason: u32) -> u32 {
    let reason = Blocked::from_raw(reason).expect("invalid ThreadDispatcher::Blocked");
    ThreadDispatcher::set_blocked_reason(reason).0
}

/// Returns true if suspended or in exception.
///
/// # Safety
///
/// The caller must hold `thread`'s state lock (`get_lock()`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_suspended_or_in_exception_locked(
    thread: &ThreadDispatcher,
) -> bool {
    // SAFETY: The caller holds `thread`'s state lock.
    unsafe {
        let token = ksync::LockToken::new();
        thread.suspended_or_in_exception_locked(&token)
    }
}

/// Returns a pointer to the thread's core thread spinlock, as a C++ `Lock<SpinLock>`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_core_thread_lock(
    thread: &ThreadDispatcher,
) -> *mut core::ffi::c_void {
    core::ptr::from_ref(thread.state().core_thread_lock.raw_mutex()).cast_mut().cast()
}

/// Returns a pointer to the thread's `Exceptionate`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_exceptionate(
    thread: &ThreadDispatcher,
) -> *mut core::ffi::c_void {
    core::ptr::from_ref(&thread.state().exceptionate).cast_mut().cast()
}

/// Sends a single-shot exception over `exceptionate` and waits for the handler response.
///
/// # Safety
///
/// * `thread` must be the currently executing user thread (`thread.is_current()`).
/// * `thread`'s dispatcher lock must not be held (`TA_EXCL(get_lock())`), nor any thread lock,
///   spinlock, or active `ChainLockTransaction`.
/// * `arch_context` must point to a valid, initialized `arch_exception_context_t` for the current
///   thread that outlives this call, and if `(*arch_context).frame` is non-null, it must point to
///   a valid, initialized, writable `iframe_t` for the duration of the call without conflicting
///   Rust references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_handle_single_shot_exception(
    thread: &ThreadDispatcher,
    exceptionate: &Exceptionate,
    exception_type: zx_excp_type_t,
    arch_context: *const ArchExceptionContext,
) -> bool {
    // SAFETY: Caller upholds all preconditions of `handle_single_shot_exception`.
    unsafe { thread.handle_single_shot_exception(exceptionate, exception_type, arch_context) }
}
