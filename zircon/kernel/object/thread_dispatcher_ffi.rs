// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::MessageWaiter;
use super::handle::KernelHandle;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{CoreThreadObservation, ThreadDispatcher, ThreadDispatcherState};
use super::thread_state::{Exception, Lifecycle};
use crate::arch_rs::UserEntryState;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use ksync::LockToken;
use zx_status::Status;
use zx_types::{
    zx_exception_context_t, zx_excp_type_t, zx_info_task_runtime_t, zx_info_thread_stats_t,
    zx_info_thread_t, zx_koid_t, zx_rights_t, zx_status_t,
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

    /// Calls into C++ implementation to set the base profile of a thread.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `profile` must point to a valid `SchedulerStateBaseProfile`.
    pub(crate) fn cpp_thread_dispatcher_set_base_profile(
        thread: *mut ThreadDispatcher,
        profile: *const crate::kernel::scheduler_state::SchedulerStateBaseProfile,
    ) -> zx_status_t;

    /// Calls into C++ implementation to set the soft affinity of a thread.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_dispatcher_set_soft_affinity(
        thread: *mut ThreadDispatcher,
        mask: crate::kernel::types::cpu_mask_t,
    ) -> zx_status_t;

    /// Gets info struct for userspace.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `out_info` must point to valid uninitialized memory for `zx_info_thread_t`.
    pub(crate) fn cpp_thread_dispatcher_get_info_for_userspace(
        thread: *const ThreadDispatcher,
        out_info: *mut zx_info_thread_t,
    );

    /// Gets stats struct for userspace.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `out_info` must point to valid uninitialized memory for `zx_info_thread_stats_t`.
    pub(crate) fn cpp_thread_dispatcher_get_stats_for_userspace(
        thread: *mut ThreadDispatcher,
        out_info: *mut zx_info_thread_stats_t,
    ) -> zx_status_t;

    /// Gets runtime stats struct.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `out_info` must point to valid uninitialized memory for `zx_info_task_runtime_t`.
    pub(crate) fn cpp_thread_dispatcher_get_runtime_stats(
        thread: *const ThreadDispatcher,
        out_info: *mut zx_info_task_runtime_t,
    );

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
    /// Sets the thread blocked reason and returns the previous blocked reason.
    pub(crate) fn cpp_thread_dispatcher_set_blocked_reason(
        reason: super::thread_dispatcher::Blocked,
    ) -> super::thread_dispatcher::Blocked;

    /// Retrieves exception report from thread.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `out_report` must point to valid uninitialized memory for `zx_exception_report_t`.
    pub(crate) fn cpp_thread_dispatcher_get_exception_report(
        thread: *const ThreadDispatcher,
        out_report: *mut zx_types::zx_exception_report_t,
    ) -> zx_status_t;

    /// Returns the current thread's `MessageWaiter`.
    ///
    /// # Safety
    ///
    /// The current thread must be an active thread with an initialized `MessageWaiter`.
    #[allow(improper_ctypes)]
    pub fn cpp_thread_dispatcher_get_current_message_waiter() -> *mut MessageWaiter;

    /// Returns a pointer to the thread's `Exceptionate`.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_dispatcher_exceptionate(
        thread: *mut ThreadDispatcher,
    ) -> *mut super::exceptionate::Exceptionate;
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

/// Kicks this thread out of restricted mode.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_restricted_kick(thread: &ThreadDispatcher) -> zx_status_t {
    Status::result_into_raw(thread.restricted_kick())
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

/// Returns whether the thread is dying or dead, with the state lock already held.
///
/// # Safety
///
/// The caller must either hold the thread's state lock, which C++ does through
/// `Guard<CriticalMutex>{get_lock()}`, or otherwise guarantee that no other thread can touch
/// the state for the duration of the call.
///
/// `~ThreadDispatcher()` calls this with no lock held. That is sound because destruction
/// implies the last reference is gone, so there is no concurrent access to race with.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_is_dying_or_dead_locked(
    thread: &ThreadDispatcher,
) -> bool {
    // SAFETY: The caller guarantees exclusive access to the state, by holding the lock or by
    // being the last reference. See this function's `# Safety` section.
    let token = unsafe { LockToken::new() };
    thread.is_dying_or_dead_locked(&token)
}

/// Returns whether the thread has ever started.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_has_started(thread: &ThreadDispatcher) -> bool {
    thread.has_started()
}

/// Returns whether the thread has ever started, with the state lock already held.
///
/// # Safety
///
/// The caller must either hold the thread's state lock, which C++ does through
/// `Guard<CriticalMutex>{get_lock()}`, or otherwise guarantee that no other thread can touch
/// the state for the duration of the call. Both of this function's current callers are in
/// `~ThreadDispatcher()` and rely on the second case; see
/// `rust_thread_dispatcher_is_dying_or_dead_locked`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_has_started_locked(
    thread: &ThreadDispatcher,
) -> bool {
    // SAFETY: The caller guarantees exclusive access to the state, by holding the lock or by
    // being the last reference. See this function's `# Safety` section.
    let token = unsafe { LockToken::new() };
    thread.has_started_locked(&token)
}

/// Returns the thread's current lifecycle as a raw discriminant.
///
/// TRANSITIONAL. This exists only for the `ThreadDispatcher` methods that still read the
/// lifecycle from C++: `~ThreadDispatcher()`, `SuspendedOrInExceptionLocked()`,
/// `GetInfoForUserspace()`, `SetBaseProfile()` and `SetSoftAffinity()`. It is removed once
/// those move to Rust; do not add callers. `SuspendedOrInExceptionLocked()` also reads
/// `exception_`, so this is expected to outlive `rust_thread_dispatcher_set_state_locked`.
///
/// `Lifecycle` is `#[repr(u32)]` with discriminants matching `ThreadState::Lifecycle`; the
/// `static_assert`s in `thread_dispatcher_ffi.cc` pin the two enumerations together.
///
/// # Safety
///
/// The caller must either hold the thread's state lock, which C++ does through
/// `Guard<CriticalMutex>{get_lock()}`, or otherwise guarantee that no other thread can touch
/// the state for the duration of the call. `~ThreadDispatcher()` relies on the second case;
/// see `rust_thread_dispatcher_is_dying_or_dead_locked`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_lifecycle_locked(thread: &ThreadDispatcher) -> u32 {
    // SAFETY: The caller guarantees exclusive access to the state, by holding the lock or by
    // being the last reference. See this function's `# Safety` section.
    let token = unsafe { LockToken::new() };
    thread.lifecycle_locked(&token) as u32
}

/// Transitions the thread's lifecycle, taking the target as a raw discriminant.
///
/// TRANSITIONAL. This is the write-side counterpart of
/// `rust_thread_dispatcher_lifecycle_locked`, and `ThreadDispatcher::ExitingCurrent()` is its
/// only remaining caller. `ExitingCurrent()` cannot move to Rust until `runtime_stats_` does,
/// so this is expected to be removed alongside that field. Do not add callers.
///
/// # Safety
///
/// The caller must hold the thread's state lock, which C++ does through
/// `Guard<CriticalMutex>{get_lock()}`. That is the only obligation: `lifecycle` is a raw
/// discriminant crossing in from C++, so it is validated by `Lifecycle::from_raw` and an
/// out-of-range value panics unconditionally rather than being converted into an invalid
/// `Lifecycle` variant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_set_state_locked(
    thread: &ThreadDispatcher,
    lifecycle: u32,
) {
    let lifecycle = Lifecycle::from_raw(lifecycle).expect("invalid ThreadState::Lifecycle");
    // SAFETY: The caller guarantees the state lock is held for the duration of this call.
    let mut token = unsafe { LockToken::new() };
    thread.set_state_locked(&mut token, lifecycle);
}

/// Transitions the thread's exception state, taking the target as a raw discriminant.
///
/// TRANSITIONAL. `exception_` and `exceptionate_type_` are still C++ fields, so the exception
/// handling that writes this axis has not been ported yet. Removed with those fields; do not add
/// callers.
///
/// # Safety
///
/// The caller must hold the thread's state lock, which C++ does through
/// `Guard<CriticalMutex>{get_lock()}`. That is the only obligation: `exception` is a raw
/// discriminant crossing in from C++, so it is validated by `Exception::from_raw` and an
/// out-of-range value panics unconditionally rather than being converted into an invalid
/// `Exception` variant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_set_exception_state_locked(
    thread: &ThreadDispatcher,
    exception: u32,
) {
    let exception = Exception::from_raw(exception).expect("invalid ThreadState::Exception");
    // SAFETY: The caller guarantees the state lock is held for the duration of this call.
    let mut token = unsafe { LockToken::new() };
    thread.set_exception_locked(&mut token, exception);
}
