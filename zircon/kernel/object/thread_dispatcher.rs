// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::MessageWaiter;
use super::handle::KernelHandle;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher_ffi::{
    cpp_sys_thread_legacy_yield, cpp_sys_thread_raise_exception, cpp_thread_dispatcher_alloc,
    cpp_thread_dispatcher_current, cpp_thread_dispatcher_exit_current,
    cpp_thread_dispatcher_get_current_message_waiter, cpp_thread_dispatcher_get_exception_report,
    cpp_thread_dispatcher_get_info_for_userspace, cpp_thread_dispatcher_get_runtime_stats,
    cpp_thread_dispatcher_get_stats_for_userspace, cpp_thread_dispatcher_has_started,
    cpp_thread_dispatcher_initialize, cpp_thread_dispatcher_is_current,
    cpp_thread_dispatcher_is_dying_or_dead, cpp_thread_dispatcher_kill,
    cpp_thread_dispatcher_kill_current, cpp_thread_dispatcher_lock_acquire,
    cpp_thread_dispatcher_lock_release, cpp_thread_dispatcher_read_state,
    cpp_thread_dispatcher_release_core_thread_observation, cpp_thread_dispatcher_restricted_kick,
    cpp_thread_dispatcher_resume, cpp_thread_dispatcher_set_base_profile,
    cpp_thread_dispatcher_set_core_thread, cpp_thread_dispatcher_set_soft_affinity,
    cpp_thread_dispatcher_start, cpp_thread_dispatcher_suspend, cpp_thread_dispatcher_write_state,
    cpp_thread_start_routine_entry,
};
use crate::counters::define_kcounter;
use crate::kernel::scheduler_state::{DEFAULT_PRIORITY, SchedulerStateBaseProfile};
use crate::kernel::thread::{Thread, ThreadPtr};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use debug::ltrace_entry_obj;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, LockToken, RawCriticalMutex, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_OBJ_TYPE_THREAD, zx_exception_context_t, zx_excp_type_t, zx_info_task_runtime_t,
    zx_info_thread_stats_t, zx_info_thread_t, zx_rights_t, zx_vaddr_t,
};

pub use crate::kernel::types::cpu_mask_t;

const LOCAL_TRACE: u32 = 0;

define_kcounter!(DISPATCHER_THREAD_CREATE_COUNT, "dispatcher.thread.create", Sum);
define_kcounter!(DISPATCHER_THREAD_DESTROY_COUNT, "dispatcher.thread.destroy", Sum);

/// Internal state storage for `ThreadDispatcher`.
///
/// The remaining `ThreadDispatcher` fields are still C++; they migrate here as the methods
/// that use them move to Rust.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ThreadDispatcherState {
    canary: Canary<{ fbl::magic(b"THRD") }>,

    /// A ref pointer back to the parent process.
    process: RefPtr<ProcessDispatcher>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

zr::static_assert_size_and_align!(
    ThreadDispatcherState,
    object_constants::kThreadDispatcherStateSize,
    object_constants::kThreadDispatcherStateAlign,
);

impl ThreadDispatcherState {
    /// Initializes the `ThreadDispatcherState` in-place.
    ///
    /// `dispatcher` is the enclosing `ThreadDispatcher`.
    pub fn init(
        dispatcher: &ThreadDispatcher,
        process: RefPtr<ProcessDispatcher>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            canary: {
                // Creation side effects (trace logging and incrementing the creation counter)
                // live inside the initializer so that they run only when it is applied rather
                // than when the `PinInit` is constructed: a `PinInit` dropped without being
                // applied would otherwise bump the create counter with no matching destroy.
                ltrace_entry_obj!(dispatcher);
                DISPATCHER_THREAD_CREATE_COUNT.add(1);
                Canary::new()
            },
            process,
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ThreadDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        let this = self.project();
        this.canary.assert();
        DISPATCHER_THREAD_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct ThreadDispatcher,
    ThreadDispatcherState,
    ZX_OBJ_TYPE_THREAD,
    object_constants::kThreadDispatcherStateOffset
);

zr::static_assert!(core::mem::size_of::<ThreadDispatcher>() == 0);

/// A wrapper around a raw pointer to the current [`ThreadDispatcher`].
///
/// This type explicitly does not implement [`Send`] or [`Sync`], guaranteeing that it cannot be
/// shared with or sent to other threads. Because the current user thread cannot be destroyed
/// while it is executing, it is safe to dereference the raw pointer into a [`ThreadDispatcher`]
/// reference.
#[derive(Debug)]
pub struct CurrentThreadDispatcher {
    ptr: *const ThreadDispatcher,
}

impl core::ops::Deref for CurrentThreadDispatcher {
    type Target = ThreadDispatcher;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `CurrentThreadDispatcher` does not implement `Send` or `Sync`, and the current
        // user thread cannot be destroyed while executing.
        unsafe { &*self.ptr }
    }
}

/// A wrapper around a raw pointer to the current thread's [`MessageWaiter`].
///
/// This type explicitly does not implement [`Send`] or [`Sync`], guaranteeing that it cannot be
/// shared with or sent to other threads. Because the [`MessageWaiter`] is embedded in the
/// currently executing [`ThreadDispatcher`], it cannot be destroyed while this thread is
/// executing, making it safe to dereference the raw pointer into a [`MessageWaiter`] reference.
pub struct CurrentMessageWaiter {
    ptr: *const MessageWaiter,
}

impl core::ops::Deref for CurrentMessageWaiter {
    type Target = MessageWaiter;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `CurrentMessageWaiter` does not implement `Send` or `Sync`, so it cannot be
        // shared with other threads. `self.ptr` points to the `MessageWaiter` embedded in the
        // current `ThreadDispatcher`, which cannot be destroyed while this thread is executing.
        unsafe { &*self.ptr }
    }
}

/// Default rights for a thread handle.
pub const DEFAULT_RIGHTS: zx_rights_t = zx_types::ZX_DEFAULT_THREAD_RIGHTS;

impl ThreadDispatcher {
    /// Returns default rights for a thread handle.
    pub const fn default_rights() -> zx_rights_t {
        DEFAULT_RIGHTS
    }

    /// Returns a wrapper that dereferences to the current [`ThreadDispatcher`].
    #[inline]
    pub fn get_current() -> CurrentThreadDispatcher {
        // SAFETY: Calling `cpp_thread_dispatcher_current` is safe when executing in a valid user
        // thread context.
        let ptr = unsafe { cpp_thread_dispatcher_current() };
        debug_assert!(!ptr.is_null());
        CurrentThreadDispatcher { ptr }
    }

    /// Returns a wrapper that dereferences to the current thread's [`MessageWaiter`].
    #[inline]
    pub fn get_current_message_waiter() -> CurrentMessageWaiter {
        // SAFETY: Calling `cpp_thread_dispatcher_get_current_message_waiter` is safe when
        // executing in a valid user thread context.
        let ptr = unsafe { cpp_thread_dispatcher_get_current_message_waiter() };
        CurrentMessageWaiter { ptr }
    }

    /// Asserts that the dispatcher state is valid.
    pub fn assert_canary(&self) {
        self.state().canary.assert();
    }

    /// Returns a reference to the parent process.
    pub fn process(&self) -> &ProcessDispatcher {
        self.assert_canary();
        &self.state().process
    }

    /// Returns the koid of the parent process.
    pub fn get_related_koid(&self) -> zx_types::zx_koid_t {
        self.process().get_koid()
    }

    /// Creates a new child `ThreadDispatcher` under the given parent process.
    pub fn create(
        process: RefPtr<ProcessDispatcher>,
        flags: u32,
        name: &[u8],
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // Create the user-mode thread and attach it to the process and lower level thread.
        let process_raw = RefPtr::into_raw(process).cast_mut();
        // SAFETY: `process_raw` is a valid pointer carrying an acquired refcount.
        let handle = unsafe {
            KernelHandle::create(|out| cpp_thread_dispatcher_alloc(process_raw, flags, out))
        }?;

        let user_thread = &**handle.dispatcher() as *const Self as *mut Self;

        // The default profile for user mode threads is a fair profile with default
        // weight which is not inherited by default.
        let profile = SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, false);

        // Create the lower level thread and attach it to the scheduler.
        // SAFETY: `user_thread` is valid for the lifetime of `handle`.
        let core_thread = unsafe {
            crate::kernel::thread::create_with_profile(
                name,
                rust_thread_start_routine,
                user_thread.cast::<c_void>(),
                &profile,
            )?
        };

        // We haven't yet completed initialization of `user_thread`, and references to it
        // haven't possibly escaped this thread. We can safely set `core_thread` outside the
        // lock.
        // SAFETY: `user_thread` and `core_thread` are valid pointers.
        unsafe {
            cpp_thread_dispatcher_set_core_thread(user_thread, core_thread.as_raw());
        }

        // The syscall layer will call initialize().
        Ok((handle, Self::default_rights()))
    }

    /// Initializes a newly created `ThreadDispatcher`.
    pub fn initialize(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status = unsafe { cpp_thread_dispatcher_initialize(self as *const _ as *mut _) };
        Status::ok(status)
    }

    /// Starts execution of this thread.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        entry: zx_vaddr_t,
        stack: zx_vaddr_t,
        arg1: u64,
        arg2: u64,
        tp: u64,
        abi_reg: u64,
        ensure_initial_thread: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status = unsafe {
            cpp_thread_dispatcher_start(
                self as *const _ as *mut _,
                entry,
                stack,
                arg1,
                arg2,
                tp,
                abi_reg,
                ensure_initial_thread,
            )
        };
        Status::ok(status)
    }

    /// Terminates the current thread. Does not return.
    pub fn exit_current() -> ! {
        // SAFETY: Terminates the current execution thread cleanly.
        unsafe {
            cpp_thread_dispatcher_exit_current();
        }
        unreachable!()
    }

    /// Marks the current thread for termination.
    pub fn kill_current() {
        // SAFETY: Sets the current thread's state to dying.
        unsafe {
            cpp_thread_dispatcher_kill_current();
        }
    }

    /// Kills this thread.
    pub fn kill(&self) {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        unsafe { cpp_thread_dispatcher_kill(self as *const _ as *mut _) }
    }

    /// Returns whether this `ThreadDispatcher` is the current thread.
    pub fn is_current(&self) -> bool {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        unsafe { cpp_thread_dispatcher_is_current(self as *const _) }
    }

    /// Suspends execution of this thread.
    pub fn suspend(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status = unsafe { cpp_thread_dispatcher_suspend(self as *const _ as *mut _) };
        Status::ok(status)
    }

    /// Resumes execution of this thread.
    pub fn resume(&self) {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        unsafe { cpp_thread_dispatcher_resume(self as *const _ as *mut _) }
    }

    /// Kicks this thread out of restricted mode.
    pub fn restricted_kick(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        Status::ok(unsafe { cpp_thread_dispatcher_restricted_kick(self as *const _ as *mut _) })
    }

    /// Reads architectural state from this thread into `buffer`.
    pub fn read_state(
        &self,
        state_kind: u32,
        buffer: *mut core::ffi::c_void,
        buffer_size: usize,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status = unsafe {
            cpp_thread_dispatcher_read_state(
                self as *const _ as *mut _,
                state_kind,
                buffer,
                buffer_size,
            )
        };
        Status::ok(status)
    }

    /// Writes architectural state to this thread from `buffer`.
    pub fn write_state(
        &self,
        state_kind: u32,
        buffer: *const core::ffi::c_void,
        buffer_size: usize,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status = unsafe {
            cpp_thread_dispatcher_write_state(
                self as *const _ as *mut _,
                state_kind,
                buffer,
                buffer_size,
            )
        };
        Status::ok(status)
    }

    /// Sets the base profile for this thread.
    pub fn set_base_profile(&self, profile: &SchedulerStateBaseProfile) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference and `profile` points to
        // an opaque `SchedulerState::BaseProfile`.
        let status = unsafe {
            cpp_thread_dispatcher_set_base_profile(
                self as *const _ as *mut _,
                profile.get() as *const _,
            )
        };
        Status::ok(status)
    }

    /// Sets soft CPU affinity for this thread.
    pub fn set_soft_affinity(&self, mask: cpu_mask_t) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        let status =
            unsafe { cpp_thread_dispatcher_set_soft_affinity(self as *const _ as *mut _, mask) };
        Status::ok(status)
    }

    /// Returns thread info for userspace.
    pub fn get_info_for_userspace(&self) -> zx_info_thread_t {
        let mut info = MaybeUninit::<zx_info_thread_t>::uninit();
        // SAFETY: `self` is valid and `info` points to uninitialized stack memory.
        unsafe {
            cpp_thread_dispatcher_get_info_for_userspace(self as *const _, info.as_mut_ptr());
            info.assume_init()
        }
    }

    /// Returns thread CPU stats for userspace.
    pub fn get_stats_for_userspace(&self) -> Result<zx_info_thread_stats_t, Status> {
        let mut info = MaybeUninit::<zx_info_thread_stats_t>::uninit();
        // SAFETY: `self` is valid and `info` points to uninitialized stack memory.
        let status = unsafe {
            cpp_thread_dispatcher_get_stats_for_userspace(
                self as *const _ as *mut _,
                info.as_mut_ptr(),
            )
        };
        Status::ok(status)?;
        // SAFETY: `cpp_thread_dispatcher_get_stats_for_userspace` initialized `info` on success.
        unsafe { Ok(info.assume_init()) }
    }

    /// Returns thread task runtime stats.
    pub fn get_runtime_stats(&self) -> zx_info_task_runtime_t {
        let mut info = MaybeUninit::<zx_info_task_runtime_t>::uninit();
        // SAFETY: `self` is valid and `info` points to uninitialized stack memory.
        unsafe {
            cpp_thread_dispatcher_get_runtime_stats(self as *const _, info.as_mut_ptr());
            info.assume_init()
        }
    }

    /// Returns the thread's current exception report.
    pub fn get_exception_report(&self) -> Result<zx_types::zx_exception_report_t, Status> {
        let mut report = MaybeUninit::<zx_types::zx_exception_report_t>::uninit();
        // SAFETY: self is a valid ThreadDispatcher and report points to uninitialized stack memory.
        let status = unsafe {
            cpp_thread_dispatcher_get_exception_report(self as *const _, report.as_mut_ptr())
        };
        Status::ok(status)?;
        // SAFETY: cpp_thread_dispatcher_get_exception_report succeeded and initialized report.
        Ok(unsafe { report.assume_init() })
    }

    /// Raises a user exception for the job debugger.
    pub fn raise_user_exception(
        options: u32,
        exception_type: zx_excp_type_t,
        user_context: &zx_exception_context_t,
    ) -> Result<(), Status> {
        // SAFETY: `user_context` is a valid reference.
        let status =
            unsafe { cpp_sys_thread_raise_exception(options, exception_type, user_context) };
        Status::ok(status)
    }

    /// Yields execution of the current thread.
    pub fn legacy_yield(options: u32) -> Result<(), Status> {
        // SAFETY: `cpp_sys_thread_legacy_yield` yields the current thread if options is 0.
        let status = unsafe { cpp_sys_thread_legacy_yield(options) };
        Status::ok(status)
    }

    /// Returns the thread's `Exceptionate`.
    pub fn exceptionate(&self) -> &super::exceptionate::Exceptionate {
        // SAFETY: `self` is a valid `ThreadDispatcher`, which owns `exceptionate_` for its entire
        // lifetime.
        unsafe {
            &*super::thread_dispatcher_ffi::cpp_thread_dispatcher_exceptionate(
                self as *const _ as *mut _,
            )
        }
    }

    /// Returns true if the thread was ever started (even if it is dead now).
    ///
    /// Threads never return to an INITIAL state after starting, so once this is true it will
    /// never flip back to false.
    ///
    /// The caller must not hold this thread's dispatcher lock.
    #[inline]
    pub fn has_started(&self) -> bool {
        // SAFETY: `self` is a valid `ThreadDispatcher`.
        unsafe { cpp_thread_dispatcher_has_started(self as *const _) }
    }

    /// Returns true if the thread is dying or dead.
    ///
    /// Threads never return to a previous state from dying/dead so once this is true it will
    /// never flip back to false.
    ///
    /// The caller must not hold this thread's dispatcher lock.
    #[inline]
    pub fn is_dying_or_dead(&self) -> bool {
        // SAFETY: `self` is a valid `ThreadDispatcher`.
        unsafe { cpp_thread_dispatcher_is_dying_or_dead(self as *const _) }
    }
}

/// Lock class tag for a [`ThreadDispatcher`]'s dispatcher lock (C++
/// `ThreadDispatcher::get_lock()`), which is the `lock` field of [`ThreadDispatcherState`].
///
/// A [`LockToken`] of this class is the Rust analogue of C++'s `TA_REQ(get_lock())`: it proves
/// that *some* `ThreadDispatcher`'s dispatcher lock is held by the current thread. Because a
/// `LockToken` is per lock class rather than per lock instance, it does not prove *which*
/// `ThreadDispatcher`'s lock is held; methods taking such a token remain `unsafe` and require the
/// caller to guarantee that the token came from the lock of the instance being operated on.
pub type ThreadDispatcherLockClass = ThreadDispatcherStateLockClass;

/// Optional RAII guard for a [`ThreadDispatcher`]'s critical mutex, mirroring C++
/// `NullLockPolicy` / `Guard<CriticalMutex>::NullLock`.
pub struct NullableThreadDispatcherGuard<'a> {
    thread: Option<&'a ThreadDispatcher>,
    should_clear: bool,
}

impl<'a> NullableThreadDispatcherGuard<'a> {
    /// Acquires `thread`'s dispatcher lock if `thread` is `Some`.
    #[inline]
    pub fn new(thread: Option<&'a ThreadDispatcher>) -> Self {
        let should_clear = if let Some(t) = thread {
            // SAFETY: `t` is a valid `ThreadDispatcher` reference.
            unsafe { cpp_thread_dispatcher_lock_acquire(t as *const _ as *mut _) }
        } else {
            false
        };
        Self { thread, should_clear }
    }

    /// Returns a [`LockToken`] proving that the guarded thread's dispatcher lock is held, or
    /// `None` if this guard does not currently hold a lock (it was created with `None` or has
    /// been released).
    ///
    /// The token borrows `self`, so the guard cannot be released while the token is alive. Note
    /// that the token only identifies the lock class; callers passing it to `unsafe` methods on a
    /// [`ThreadDispatcher`] must ensure that the `ThreadDispatcher` is the one this guard was
    /// created for.
    #[inline]
    pub fn token(&self) -> Option<LockToken<'_, ThreadDispatcherLockClass>> {
        // SAFETY: `self.thread` is `Some` exactly when `Self::new` acquired that thread's
        // dispatcher lock and it has not yet been released by `Self::release`. The returned token
        // borrows `self`, so it cannot outlive the lock hold (`release` needs `&mut self`).
        self.thread.map(|_| unsafe { LockToken::new() })
    }

    /// Releases the dispatcher lock early if currently held.
    #[inline]
    pub fn release(&mut self) {
        if let Some(t) = self.thread.take() {
            // SAFETY: `t`'s lock was acquired by `Self::new` and has not yet been released.
            unsafe {
                cpp_thread_dispatcher_lock_release(t as *const _ as *mut _, self.should_clear);
            }
        }
    }
}

impl Drop for NullableThreadDispatcherGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

/// RAII observation of a [`ThreadDispatcher`]'s `core_thread_` under `core_thread_lock_`,
/// mirroring C++ `ThreadDispatcher::CoreThreadObservation`.
#[repr(C)]
pub struct CoreThreadObservation {
    core_thread: *mut Thread,
    lock: *mut c_void,
}

impl Default for CoreThreadObservation {
    #[inline]
    fn default() -> Self {
        Self { core_thread: core::ptr::null_mut(), lock: core::ptr::null_mut() }
    }
}

zr::static_assert!(
    core::mem::size_of::<CoreThreadObservation>() == 2 * core::mem::size_of::<usize>()
);
zr::static_assert!(
    core::mem::align_of::<CoreThreadObservation>() == core::mem::align_of::<usize>()
);

impl CoreThreadObservation {
    /// Returns the observed [`ThreadPtr`], if any.
    #[inline]
    pub fn core_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: `self.core_thread` is either null or a valid `Thread*` observed under
        // `core_thread_lock_`.
        unsafe { ThreadPtr::from_raw(self.core_thread) }
    }

    /// Releases `core_thread_lock_` if an observation is active.
    #[inline]
    pub fn release(&mut self) {
        // SAFETY: `self` is a valid `CoreThreadObservation` initialized by `Default` or
        // `cpp_thread_dispatcher_observe_core_thread`.
        unsafe {
            cpp_thread_dispatcher_release_core_thread_observation(self as *mut Self);
        }
    }
}

impl Drop for CoreThreadObservation {
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

/// The reason a thread is currently blocked, matching C++ `ThreadDispatcher::Blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Blocked(pub u32);

impl Blocked {
    pub const NONE: Self = Self(0);
    pub const EXCEPTION: Self = Self(1);
    pub const SLEEPING: Self = Self(2);
    pub const FUTEX: Self = Self(3);
    pub const PORT: Self = Self(4);
    pub const CHANNEL: Self = Self(5);
    pub const WAIT_ONE: Self = Self(6);
    pub const WAIT_MANY: Self = Self(7);
    pub const INTERRUPT: Self = Self(8);
    pub const PAGER: Self = Self(9);
}

/// RAII helper that sets the current thread's `blocked_reason` and restores it on drop.
pub struct AutoBlocked {
    prev_reason: Blocked,
}

impl AutoBlocked {
    /// Sets the blocked reason on the current thread.
    #[inline]
    pub fn new(reason: Blocked) -> Self {
        // SAFETY: `cpp_thread_dispatcher_set_blocked_reason` transitions the current thread's
        // blocked state on the current CPU and returns the previous blocked reason.
        let prev_reason = unsafe {
            super::thread_dispatcher_ffi::cpp_thread_dispatcher_set_blocked_reason(reason)
        };
        Self { prev_reason }
    }
}

impl Drop for AutoBlocked {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Restoring the previous blocked reason on the current thread upon exiting
        // the blocking scope is always safe and restores previous thread state.
        unsafe {
            super::thread_dispatcher_ffi::cpp_thread_dispatcher_set_blocked_reason(
                self.prev_reason,
            );
        }
    }
}

zr::static_assert!(core::mem::size_of::<Blocked>() == 4);
zr::static_assert!(core::mem::align_of::<Blocked>() == 4);

/// Trampoline entry routine called by `Thread::Create`.
///
/// The body stays in C++ because it builds the architecture-specific iframe and performs the
/// `arch_enter_uspace()` transition.
extern "C" fn rust_thread_start_routine(arg: *mut c_void) -> i32 {
    // SAFETY: `arg` is the `ThreadDispatcher` pointer handed to `create_with_profile`, and the
    // core thread cannot run before that dispatcher exists.
    unsafe { cpp_thread_start_routine_entry(arg) }
}

/// In-tree kernel unit tests for `ThreadDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "thread_dispatcher_rust")]
mod tests {
    use super::super::job_dispatcher::JobDispatcher;
    use super::super::process_dispatcher::ProcessDispatcher;
    use super::ThreadDispatcher;
    use unittest::{expect_eq, expect_ok};

    /// Tests creating and destroying a thread without initializing it.
    #[test]
    fn test_create_destroy_thread_no_init() {
        let root_job = JobDispatcher::create_root_job();
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(root_job, b"k-ut-p0", 0).expect("process created");
        let (thread_handle, rights) =
            ThreadDispatcher::create(proc_handle.dispatcher().clone(), 0, b"k-ut-t0")
                .expect("thread created");
        expect_eq!(rights, ThreadDispatcher::default_rights());
        expect_eq!(
            thread_handle.dispatcher().get_related_koid(),
            proc_handle.dispatcher().get_koid()
        );
        expect_eq!(
            thread_handle.dispatcher().process().get_koid(),
            proc_handle.dispatcher().get_koid()
        );
        expect_eq!(thread_handle.dispatcher().get_type(), zx_types::ZX_OBJ_TYPE_THREAD);
        expect_eq!(thread_handle.dispatcher().is_current(), false);
        thread_handle.dispatcher().assert_canary();
    }

    /// Tests creating, initializing, and destroying a thread.
    #[test]
    fn test_create_init_destroy_thread() {
        let root_job = JobDispatcher::create_root_job();
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(root_job, b"k-ut-p1", 0).expect("process created");
        let (thread_handle, rights) =
            ThreadDispatcher::create(proc_handle.dispatcher().clone(), 0, b"k-ut-t1")
                .expect("thread created");
        expect_eq!(rights, ThreadDispatcher::default_rights());
        expect_ok!(thread_handle.dispatcher().initialize());
        thread_handle.dispatcher().assert_canary();
    }
}
