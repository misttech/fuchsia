// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::MessageWaiter;
use super::dispatcher::{DispatcherOps, impl_dispatcher_facade_with_state};
use super::exception_dispatcher::ExceptionDispatcher;
use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher_ffi::{
    cpp_sys_thread_raise_exception, cpp_thread_dispatcher_alloc,
    cpp_thread_dispatcher_attach_to_core_thread, cpp_thread_dispatcher_current,
    cpp_thread_dispatcher_get_core_thread, cpp_thread_dispatcher_is_current,
    cpp_thread_dispatcher_lock_acquire, cpp_thread_dispatcher_lock_release,
    cpp_thread_dispatcher_read_state, cpp_thread_dispatcher_release_core_thread_observation,
    cpp_thread_dispatcher_reset_core_thread, cpp_thread_dispatcher_set_core_thread,
    cpp_thread_dispatcher_set_user_entry, cpp_thread_dispatcher_write_state,
    cpp_thread_handle_single_shot_exiting, cpp_thread_runtime_stats_add_lock_contention_ticks,
    cpp_thread_runtime_stats_add_page_fault_ticks, cpp_thread_runtime_stats_get_compensated,
    cpp_thread_runtime_stats_init, cpp_thread_runtime_stats_update, cpp_thread_start_routine_entry,
};
use super::thread_state::{Exception, Lifecycle, ThreadState, lifecycle_to_string};
use crate::arch_rs::{ArchExceptionContext, UserEntryState};
use crate::counters::define_kcounter;
use crate::kernel::scheduler_state::{
    DEFAULT_PRIORITY, SchedulerStateBaseProfile, ThreadStateKind,
};
use crate::kernel::thread::{Thread, ThreadPtr, with_exception_context};
use core::ffi::c_void;
use debug::{ltrace_entry_obj, ltrace_exit_obj, ltracef};
use fbl::{Canary, RefPtr};
use kprint::kprintln;
use ksync::{KMutex, LockToken, RawCriticalMutex, RawSpinlock, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_ERR_INTERNAL_INTR_KILLED, ZX_OBJ_TYPE_THREAD, ZX_THREAD_RUNNING, ZX_THREAD_SUSPENDED,
    ZX_THREAD_TERMINATED, zx_exception_context_t, zx_exception_report_t, zx_excp_type_t,
    zx_info_task_runtime_t, zx_info_thread_stats_t, zx_info_thread_t, zx_rights_t,
};

use crate::kernel::types::cpu_mask_t;

const LOCAL_TRACE: u32 = 0;

define_kcounter!(DISPATCHER_THREAD_CREATE_COUNT, "dispatcher.thread.create", Sum);
define_kcounter!(DISPATCHER_THREAD_DESTROY_COUNT, "dispatcher.thread.destroy", Sum);

zr::define_opaque_storage_ffi! {
    /// Opaque storage wrapper for `ThreadRuntimeStats`.
    pub(crate) struct ThreadRuntimeStatsStorage(
        object_constants::kThreadRuntimeStatsStorageSize,
        object_constants::kThreadRuntimeStatsStorageAlign,
        8,
        cpp_thread_runtime_stats_init,
    );
}

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

    /// Thread lifecycle and exception state machine.
    #[guarded_by(lock)]
    state: ThreadState,

    /// The runtime stats for this thread.
    #[pin]
    pub(crate) runtime_stats: ThreadRuntimeStatsStorage,
    /// This is only valid while `lifecycle_locked()` is `Running`.
    ///
    /// This field is an atomic because it may be accessed concurrently by multiple
    /// threads. It may be read by any thread, but may only be updated by the
    /// "this" thread.
    ///
    /// In general, loads of this field should be performed with acquire semantics
    /// and stores with release semantics because this field is used to synchronize
    /// threads (think: wait for a thread to become blocked, then inspect some
    /// state the thread has written).
    ///
    /// Because this is simply an atomic, readers must be OK with observing stale
    /// values. That is, by the time a reader can take action on the value, the
    /// value may no longer be accurate.
    blocked_reason: core::sync::atomic::AtomicU32,

    /// Support for sending an exception to an exception handler and then waiting for a response.
    /// Exceptionates have internal locking so we don't need to guard it here.
    #[pin]
    pub(crate) exceptionate: Exceptionate,

    /// Non-null if the thread is currently processing a channel exception.
    #[guarded_by(lock)]
    pub(crate) exception: Option<RefPtr<ExceptionDispatcher>>,

    /// Holds the type of the exceptionate currently processing the exception,
    /// which may be our `exceptionate` or one of our parents'.
    #[guarded_by(lock)]
    exceptionate_type: u32,

    /// Tracks the number of times `suspend()` has been called. `resume()` will resume this
    /// thread only when this reference count reaches 0.
    #[guarded_by(lock)]
    suspend_count: i32,

    /// Per-thread structure used while waiting in a `ChannelDispatcher::call`.
    /// Needed to support the requirements of being able to interrupt a call
    /// in order to suspend a thread.
    #[pin]
    pub(crate) channel_waiter: MessageWaiter,

    /// A lower level lock which is needed to satisfy some odd synchronization
    /// requirements when blocking a thread in a futex and declaring a new owner in
    /// the process.
    ///
    /// It guards the C++ `core_thread_` field, so it has no `#[guarded_by]` users here; C++
    /// resolves it through `rust_thread_dispatcher_get_core_thread_lock` and acquires it with
    /// `Guard<SpinLock, IrqSave>`. Declaring it `#[mutex]` registers its lockdep class.
    #[mutex]
    pub(crate) core_thread_lock: KMutex<RawSpinlock>,

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
            state: ThreadState::new().into(),
            // SAFETY: `ThreadRuntimeStatsStorage::init()` initializes the opaque storage via
            // placement new.
            runtime_stats <- unsafe { ThreadRuntimeStatsStorage::init() },
            blocked_reason: core::sync::atomic::AtomicU32::new(Blocked::NONE.0),
            exceptionate <- Exceptionate::new(zx_types::ZX_EXCEPTION_CHANNEL_TYPE_THREAD),
            exception: None.into(),
            exceptionate_type: zx_types::ZX_EXCEPTION_CHANNEL_TYPE_NONE.into(),
            suspend_count: 0.into(),
            channel_waiter <- MessageWaiter::init(),
            core_thread_lock <- KMutex::init(),
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ThreadDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        let disp_ptr = core::ptr::from_ref(self.as_ref().get_ref())
            .wrapping_byte_sub(object_constants::kThreadDispatcherStateOffset)
            .cast::<ThreadDispatcher>();
        let this = self.project();
        DISPATCHER_THREAD_DESTROY_COUNT.add(1);

        let state = this.state.get_inner_mut();
        let has_started =
            state.lifecycle() != Lifecycle::Initial && state.lifecycle() != Lifecycle::Initialized;
        let is_dying_or_dead =
            state.lifecycle() == Lifecycle::Dying || state.lifecycle() == Lifecycle::Dead;
        // The state is read without taking the lock. Destruction implies the last reference is
        // gone, so there is no concurrent access to race with.
        debug_assert!(
            !has_started || is_dying_or_dead,
            "Thread {:p} killed in bad state: {}",
            disp_ptr,
            lifecycle_to_string(state.lifecycle())
        );

        if state.lifecycle() != Lifecycle::Initial {
            // We grew the pool in initialize(), which transitioned the thread from its initial
            // state.
            this.process.futex_context().shrink_futex_state_pool();
        }

        // If make_runnable hasn't been called, then our core_thread has never run and we need to
        // be the ones to remove it.
        // SAFETY: `disp_ptr` points to the enclosing `ThreadDispatcher`, and destruction has
        // exclusive access because the last reference is gone.
        if !has_started
            && let Some(core_thread) =
                unsafe { ThreadPtr::from_raw(cpp_thread_dispatcher_get_core_thread(disp_ptr)) }
        {
            // Since the Thread is in an initialized state we can directly destruct it.
            // SAFETY: `core_thread` was initialized but never started, so forgetting it is safe.
            unsafe { core_thread.forget() };
        }
    }
}

impl_dispatcher_facade_with_state!(
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
const DEFAULT_RIGHTS: zx_rights_t = zx_types::ZX_DEFAULT_THREAD_RIGHTS;

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

    /// Returns a wrapper that dereferences to the current [`ThreadDispatcher`], or `None` if the
    /// current thread is not a user thread (i.e., not associated with a `ThreadDispatcher`).
    #[inline]
    pub fn try_get_current() -> Option<CurrentThreadDispatcher> {
        // SAFETY: `cpp_thread_dispatcher_current` returns the current thread's `user_thread()`, or
        // null when executing on a kernel-only thread.
        let ptr = unsafe { cpp_thread_dispatcher_current() };
        if ptr.is_null() { None } else { Some(CurrentThreadDispatcher { ptr }) }
    }

    /// Returns a wrapper that dereferences to the current thread's [`MessageWaiter`].
    /// For ChannelDispatcher use.
    #[inline]
    pub fn get_current_message_waiter() -> CurrentMessageWaiter {
        let current = Self::get_current();
        CurrentMessageWaiter { ptr: &raw const current.state().channel_waiter }
    }

    /// Asserts that the dispatcher state is valid.
    #[inline]
    pub fn assert_canary(&self) {
        self.state().canary.assert();
    }

    /// Returns a reference to the parent process.
    #[inline]
    pub fn process(&self) -> &ProcessDispatcher {
        self.assert_canary();
        &self.state().process
    }

    /// Returns the koid of the parent process.
    #[inline]
    pub fn get_related_koid(&self) -> zx_types::zx_koid_t {
        self.process().get_koid()
    }

    /// Creates a new child `ThreadDispatcher` under the given parent process.
    pub fn create(
        process: RefPtr<ProcessDispatcher>,
        _flags: u32,
        name: &[u8],
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // Create the user-mode thread and attach it to the process and lower level thread.
        let process_raw = RefPtr::into_raw(process).cast_mut();
        // SAFETY: `process_raw` is a valid pointer carrying an acquired refcount.
        let handle =
            unsafe { KernelHandle::create(|out| cpp_thread_dispatcher_alloc(process_raw, out)) }?;

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

    /// Changes states of the object, doing what is appropriate for the state transition.
    ///
    /// Callers have already asserted the canary before taking the state lock.
    ///
    /// # Safety
    ///
    /// `token` must have been obtained from locking **this particular instance's** state lock
    /// (`self.state().lock`).
    unsafe fn set_state_locked(
        &self,
        token: &mut ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
        lifecycle: Lifecycle,
    ) {
        ltracef!(
            "thread {:p}: state {} ({})\n",
            self,
            lifecycle as u32,
            lifecycle_to_string(lifecycle)
        );

        // SAFETY: The caller guarantees `token` was obtained by locking `self.state().lock`.
        unsafe { self.state().state.get_mut(token) }.set_lifecycle(lifecycle);

        match lifecycle {
            Lifecycle::Running => {
                self.update_state_locked(token, ZX_THREAD_SUSPENDED, ZX_THREAD_RUNNING);
            }
            Lifecycle::Suspended => {
                self.update_state_locked(token, ZX_THREAD_RUNNING, ZX_THREAD_SUSPENDED);
            }
            Lifecycle::Dead => {
                self.update_state_locked(
                    token,
                    ZX_THREAD_RUNNING | ZX_THREAD_SUSPENDED,
                    ZX_THREAD_TERMINATED,
                );
            }
            _ => {
                // Nothing to do.
                // In particular, for the Dying state we don't modify the Suspended
                // or Running signals: For observer purposes they'll only be interested
                // in the transition from {Suspended, Running} to Dead.
            }
        }
    }

    /// Returns the current lifecycle while the state lock is held.
    fn lifecycle_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> Lifecycle {
        self.state().canary.assert();
        // SAFETY: The caller holds the state lock, proven by `token`.
        unsafe { self.state().state.get(token) }.lifecycle()
    }

    /// Returns true if the thread is dying or dead. Threads never return to a previous state
    /// from dying/dead so once this is true it will never flip back to false.
    ///
    /// The caller must not hold this thread's dispatcher lock.
    pub fn is_dying_or_dead(&self) -> bool {
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        self.is_dying_or_dead_locked(guard.token())
    }

    /// Returns true if the thread is dying or dead, with the state lock already held.
    pub(crate) fn is_dying_or_dead_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> bool {
        let lifecycle = self.lifecycle_locked(token);
        lifecycle == Lifecycle::Dying || lifecycle == Lifecycle::Dead
    }

    /// Returns true if the thread was ever started (even if it is dead now).
    /// Threads never return to an Initial state after starting, so once this is
    /// true it will never flip back to false.
    ///
    /// The caller must not hold this thread's dispatcher lock.
    pub fn has_started(&self) -> bool {
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        self.has_started_locked(guard.token())
    }

    /// Returns true if the thread was ever started, with the state lock already held.
    pub(crate) fn has_started_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> bool {
        let lifecycle = self.lifecycle_locked(token);
        lifecycle != Lifecycle::Initial && lifecycle != Lifecycle::Initialized
    }

    /// Returns the lower level kernel thread backing this dispatcher, or `None` if
    /// `core_thread_` has been cleared.
    ///
    /// `core_thread_` is still a C++ field, guarded by this object's state lock, so `token`
    /// stands in for the `TA_GUARDED(get_lock())` on the C++ side.
    ///
    /// Callers have already asserted the canary before taking the state lock.
    fn try_core_thread_locked(
        &self,
        _token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> Option<ThreadPtr> {
        // SAFETY: `self` is a valid `ThreadDispatcher`, and the caller holds the state lock that
        // guards `core_thread_`, proven by `_token`.
        let core_thread = unsafe { cpp_thread_dispatcher_get_core_thread(self as *const _) };
        // SAFETY: A non-null `core_thread_` names the live kernel thread created for this
        // dispatcher, which outlives it.
        unsafe { ThreadPtr::from_raw(core_thread) }
    }

    /// Stands in for `TA_GUARDED(get_lock())` on the C++ side.
    ///
    /// # Panics
    ///
    /// Panics if the core thread has already been cleared.
    pub(crate) fn core_thread_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> ThreadPtr {
        self.try_core_thread_locked(token)
            .expect("core thread was cleared before the thread reached the Dead lifecycle")
    }

    /// Completes initialization of the thread object outside of the constructor.
    pub fn initialize(&self) -> Result<(), Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        // Make sure we contribute a FutexState object to our process's futex state.
        self.process().futex_context().grow_futex_state_pool()?;

        ksync::lock!(let mut guard = self.state().lock_lock());
        // Associate the proc's address space with this thread. All threads start off with the
        // normal address space as their active aspace.
        let core_thread = self.core_thread_locked(guard.token());
        self.process().attach_normal_aspace_to_thread(core_thread);
        // we've entered the initialized state
        // SAFETY: `guard` holds `self`'s state lock.
        unsafe {
            self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Initialized);
        }
        Ok(())
    }

    /// Starts this thread running inside the parent process with the provided entry state.
    ///
    /// Only valid to be called on a thread in the `Initialized` state that has not yet been
    /// started. If `ensure_initial_thread` is true, the thread will only start if it is the
    /// first thread in the process.
    pub fn start(&self, entry: UserEntryState, ensure_initial_thread: bool) -> Result<(), Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        // add ourselves to the process, which may fail if the process is in a dead state.
        // If the process is live then it will call our make_runnable routine.
        self.process().add_initialized_thread(self, ensure_initial_thread, &entry)
    }

    /// Transitions a thread from the `Initialized` state to either the `Running` or `Suspended`
    /// state.
    ///
    /// It is the caller's responsibility to ensure this thread is registered with the parent
    /// process, as such this is only expected to be called from the `ProcessDispatcher`.
    ///
    /// `ProcessDispatcher::AddInitializedThread` depends on this being infallible once both
    /// checks below have passed, so nothing after them may return an error.
    pub fn make_runnable(&self, entry: UserEntryState, suspended: bool) -> Result<(), Status> {
        self.state().canary.assert();
        if !crate::arch_rs::vm::is_valid_user_pc(entry.pc as usize) {
            return Err(Status::INVALID_ARGS);
        }

        ksync::lock!(let mut guard = self.state().lock_lock());

        if self.lifecycle_locked(guard.token()) != Lifecycle::Initialized {
            return Err(Status::BAD_STATE);
        }

        // save the user space entry state
        // SAFETY: `self` is a valid `ThreadDispatcher` and `entry` is a live local.
        unsafe { cpp_thread_dispatcher_set_user_entry(self as *const _ as *mut _, &entry) };

        // update our suspend count to account for our parent state
        if suspended {
            *guard.as_mut().fields_mut().suspend_count += 1;
        }

        let core_thread = self.core_thread_locked(guard.token());

        // Attach the ThreadDispatcher to the core thread. This takes out an additional
        // reference on the ThreadDispatcher.
        // SAFETY: `self` is a valid `ThreadDispatcher` and `core_thread` a live kernel thread.
        unsafe {
            cpp_thread_dispatcher_attach_to_core_thread(
                self as *const _ as *mut _,
                core_thread.as_raw(),
            )
        };

        // start the thread in Running state, if we're starting suspended it will transition to
        // Suspended when it checks thread signals before executing any user code
        // SAFETY: `guard` holds `self`'s state lock.
        unsafe {
            self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Running);
        }

        // Given this is no longer a kernel-only thread, start memory stall accounting
        // by entering the Inactive state.
        // SAFETY: `core_thread` is live and, having never been started, is still in the
        // `IgnoredKernelOnly` stall state.
        unsafe { core_thread.set_memory_stall_state_inactive() };

        if *guard.fields().suspend_count == 0 {
            // SAFETY: `core_thread` is live; this dispatcher holds it until it exits.
            unsafe { core_thread.resume() };
        } else {
            // core_thread.suspend() only fails if the underlying thread is already dead,
            // which we should ignore here to match the behavior of core_thread.resume();
            // our ExitingCurrent() callback will run shortly to clean us up
            // SAFETY: `core_thread` is live; this dispatcher holds it until it exits.
            let _ = unsafe { core_thread.suspend() };
        }

        Ok(())
    }

    /// Terminates the current thread. Does not return.
    pub fn exit_current() -> ! {
        crate::kernel::thread::current_exit(0)
    }

    /// Sets the exception dispatcher and exceptionate type for the thread.
    ///
    /// This state is needed by [`Self::get_info_for_userspace`].
    ///
    /// # Safety
    ///
    /// `token` must have been obtained from locking **this particular instance's** state lock
    /// (`self.state().lock`).
    #[inline]
    pub(crate) unsafe fn set_exception_dispatcher_locked(
        &self,
        token: &mut ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
        exception: RefPtr<ExceptionDispatcher>,
        exceptionate_type: u32,
    ) {
        self.state().canary.assert();
        // SAFETY: The caller guarantees `token` was obtained by locking `self.state().lock`.
        unsafe {
            *self.state().exception.get_mut(token) = Some(exception);
            *self.state().exceptionate_type.get_mut(token) = exceptionate_type;
            self.state().state.get_mut(token).set_exception(Exception::Unprocessed);
        }
    }

    /// Clears the exception dispatcher on the thread and returns whether the thread is dying or
    /// dead.
    ///
    /// # Safety
    ///
    /// `token` must have been obtained from locking **this particular instance's** state lock
    /// (`self.state().lock`).
    #[inline]
    pub(crate) unsafe fn clear_exception_locked(
        &self,
        token: &mut ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> bool {
        self.state().canary.assert();
        // SAFETY: The caller guarantees `token` was obtained by locking `self.state().lock`.
        unsafe {
            let lifecycle = self.state().state.get(token).lifecycle();
            let dying_or_dead = lifecycle == Lifecycle::Dying || lifecycle == Lifecycle::Dead;
            *self.state().exception.get_mut(token) = None;
            *self.state().exceptionate_type.get_mut(token) =
                zx_types::ZX_EXCEPTION_CHANNEL_TYPE_NONE;
            self.state().state.get_mut(token).set_exception(Exception::Idle);
            dying_or_dead
        }
    }

    /// Returns true if waiting for an exception response.
    ///
    /// # Safety
    ///
    /// `token` must have been obtained from locking **this particular instance's** state lock
    /// (`self.state().lock`).
    #[inline]
    pub(crate) unsafe fn in_exception_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> bool {
        ltrace_entry_obj!(self);
        // SAFETY: The caller guarantees `token` was obtained by locking `self.state().lock`.
        unsafe { self.state().exception.get(token).is_some() }
    }

    /// Returns true if the thread is suspended or processing an exception.
    ///
    /// # Safety
    ///
    /// `token` must have been obtained from locking **this particular instance's** state lock
    /// (`self.state().lock`).
    #[inline]
    pub(crate) unsafe fn suspended_or_in_exception_locked(
        &self,
        token: &ksync::LockToken<'_, ThreadDispatcherStateLockClass>,
    ) -> bool {
        self.state().canary.assert();
        // SAFETY: The caller guarantees `token` was obtained by locking `self.state().lock`.
        unsafe {
            self.state().state.get(token).lifecycle() == Lifecycle::Suspended
                || self.in_exception_locked(token)
        }
    }

    /// Marks the current thread for termination. The thread will actually terminate when the
    /// kernel stack unwinds.
    pub fn kill_current() {
        crate::kernel::thread::current_kill();
    }

    /// Kills this thread.
    pub fn kill(&self) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        ksync::lock!(let mut guard = self.state().lock_lock());

        match self.lifecycle_locked(guard.token()) {
            Lifecycle::Initial | Lifecycle::Initialized => {
                // thread was never started, leave in this state
            }
            Lifecycle::Running | Lifecycle::Suspended => {
                // deliver a kernel kill signal to the thread
                // SAFETY: `core_thread` is live; the lifecycle is not yet `Dead`.
                unsafe { self.core_thread_locked(guard.token()).kill() };

                // enter the dying state
                // SAFETY: `guard` holds `self`'s state lock.
                unsafe {
                    self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Dying);
                }
            }
            Lifecycle::Dying | Lifecycle::Dead => {
                // already going down
            }
        }
    }

    /// Returns whether this `ThreadDispatcher` is the current thread.
    #[inline]
    pub fn is_current(&self) -> bool {
        // SAFETY: `self` is a valid `ThreadDispatcher` reference.
        unsafe { cpp_thread_dispatcher_is_current(self as *const _) }
    }

    /// Suspends the thread.
    ///
    /// Returns `Ok` on success, or `BAD_STATE` iff the thread is dying or dead.
    pub fn suspend(&self) -> Result<(), Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        ksync::lock!(let mut guard = self.state().lock_lock());

        ltracef!(
            "{:p}: state {}\n",
            self,
            lifecycle_to_string(self.lifecycle_locked(guard.token()))
        );

        // Update `suspend_count` in all cases so that we can always verify a sane value - it's
        // possible both suspend() and resume() get called while the thread is Dying.
        debug_assert!(*guard.fields().suspend_count >= 0);
        *guard.as_mut().fields_mut().suspend_count += 1;

        match self.lifecycle_locked(guard.token()) {
            Lifecycle::Initial => {
                // Unreachable, thread leaves Initial state before create() returns.
                debug_assert!(false);
                Err(Status::BAD_STATE)
            }
            Lifecycle::Initialized => {
                // If the thread hasn't started yet, don't actually try to suspend it. We need to
                // let start() run first to set up userspace entry data, which will then suspend
                // if the count is still >0 at that time.
                Ok(())
            }
            Lifecycle::Running | Lifecycle::Suspended => {
                if *guard.fields().suspend_count == 1 {
                    // SAFETY: `core_thread` is live; the lifecycle is not yet `Dead`.
                    return unsafe { self.core_thread_locked(guard.token()).suspend() };
                }
                Ok(())
            }
            Lifecycle::Dying | Lifecycle::Dead => Err(Status::BAD_STATE),
        }
    }

    /// Resumes execution of this thread.
    pub fn resume(&self) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        ksync::lock!(let mut guard = self.state().lock_lock());

        debug_assert!(*guard.fields().suspend_count > 0);
        *guard.as_mut().fields_mut().suspend_count -= 1;

        match self.lifecycle_locked(guard.token()) {
            Lifecycle::Initial => {
                // Unreachable, thread leaves Initial state before create() returns.
                debug_assert!(false);
            }
            Lifecycle::Initialized => {}
            Lifecycle::Running | Lifecycle::Suspended => {
                // It's possible the thread never transitioned from Running -> Suspended.
                if *guard.fields().suspend_count == 0 {
                    // SAFETY: `core_thread` is live; the lifecycle is not yet `Dead`.
                    unsafe { self.core_thread_locked(guard.token()).resume() };
                }
            }
            Lifecycle::Dying | Lifecycle::Dead => {
                // If it's dying or dead then bail.
            }
        }
    }

    /// Issues a restricted kick on the thread which will kick the thread out of restricted mode
    /// to normal mode if it's currently in restricted mode, or remember the kick state for the
    /// next attempt to enter restricted state.
    ///
    /// Returns `Ok` on success or `BAD_STATE` iff the thread is dying or dead.
    pub fn restricted_kick(&self) -> Result<(), Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        ksync::lock!(let guard = self.state().lock_lock());

        match self.lifecycle_locked(guard.token()) {
            Lifecycle::Initial => {
                // Unreachable, thread leaves Initial state before create() returns.
                debug_assert!(false);
                Err(Status::BAD_STATE)
            }
            Lifecycle::Initialized | Lifecycle::Running | Lifecycle::Suspended => {
                // SAFETY: `core_thread` is live; the lifecycle is not yet `Dead`.
                unsafe { self.core_thread_locked(guard.token()).restricted_kick() }
            }
            Lifecycle::Dying | Lifecycle::Dead => Err(Status::BAD_STATE),
        }
    }

    /// Callback from the kernel when the thread is suspending.
    pub fn suspending(&self) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        // Update the state before sending any notifications out. We want the
        // receiver to see the new state.
        {
            ksync::lock!(let mut guard = self.state().lock_lock());

            // Don't suspend if we are racing with our own death.
            if self.lifecycle_locked(guard.token()) != Lifecycle::Dying {
                // SAFETY: `guard` holds `self`'s state lock.
                unsafe {
                    self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Suspended);
                }
            }
        }

        ltrace_exit_obj!(self);
    }

    /// Callback from the kernel when the thread is resuming.
    pub fn resuming(&self) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        // Update the state before sending any notifications out. We want the
        // receiver to see the new state.
        {
            ksync::lock!(let mut guard = self.state().lock_lock());

            // Don't resume if we are racing with our own death.
            if self.lifecycle_locked(guard.token()) != Lifecycle::Dying {
                // SAFETY: `guard` holds `self`'s state lock.
                unsafe {
                    self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Running);
                }
            }
        }

        ltrace_exit_obj!(self);
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
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        match guard.fields().state.lifecycle() {
            Lifecycle::Initial | Lifecycle::Dying | Lifecycle::Dead => Err(Status::BAD_STATE),
            _ => {
                let core_thread = self.core_thread_locked(guard.token());
                // The profile was already validated by the Profile dispatcher.
                // SAFETY: `core_thread` is a live kernel `Thread` while the state lock is held and
                // the lifecycle is not `Dead`, and this is normal thread context with no spinlock
                // or `ChainLockTransaction` active.
                unsafe { core_thread.set_base_profile(profile) };
                Ok(())
            }
        }
    }

    /// Sets soft CPU affinity for this thread.
    pub fn set_soft_affinity(&self, mask: cpu_mask_t) -> Result<(), Status> {
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        match guard.fields().state.lifecycle() {
            Lifecycle::Initial | Lifecycle::Dying | Lifecycle::Dead => Err(Status::BAD_STATE),
            _ => {
                let core_thread = self.core_thread_locked(guard.token());
                // The mask was already validated by the Profile dispatcher.
                // SAFETY: `core_thread` is a live kernel `Thread` while the state lock is held and
                // the lifecycle is not `Dead`.
                unsafe { core_thread.set_soft_cpu_affinity(mask) };
                Ok(())
            }
        }
    }

    /// Maps internal thread lifecycle + blocked reason to `zx_thread_state_t`.
    fn thread_lifecycle_to_state(lifecycle: Lifecycle, blocked_reason: Blocked) -> u32 {
        match lifecycle {
            Lifecycle::Initial | Lifecycle::Initialized => zx_types::ZX_THREAD_STATE_NEW,
            Lifecycle::Running => {
                // The thread may be "running" but be blocked in a syscall or exception handler.
                match blocked_reason {
                    Blocked::NONE => zx_types::ZX_THREAD_STATE_RUNNING,
                    Blocked::EXCEPTION => zx_types::ZX_THREAD_STATE_BLOCKED_EXCEPTION,
                    Blocked::SLEEPING => zx_types::ZX_THREAD_STATE_BLOCKED_SLEEPING,
                    Blocked::FUTEX => zx_types::ZX_THREAD_STATE_BLOCKED_FUTEX,
                    Blocked::PORT => zx_types::ZX_THREAD_STATE_BLOCKED_PORT,
                    Blocked::CHANNEL => zx_types::ZX_THREAD_STATE_BLOCKED_CHANNEL,
                    Blocked::WAIT_ONE => zx_types::ZX_THREAD_STATE_BLOCKED_WAIT_ONE,
                    Blocked::WAIT_MANY => zx_types::ZX_THREAD_STATE_BLOCKED_WAIT_MANY,
                    Blocked::INTERRUPT => zx_types::ZX_THREAD_STATE_BLOCKED_INTERRUPT,
                    Blocked::PAGER => zx_types::ZX_THREAD_STATE_BLOCKED_PAGER,
                    _ => {
                        debug_assert!(false, "unexpected blocked reason: {}", blocked_reason.0);
                        zx_types::ZX_THREAD_STATE_BLOCKED
                    }
                }
            }
            Lifecycle::Suspended => zx_types::ZX_THREAD_STATE_SUSPENDED,
            Lifecycle::Dying => zx_types::ZX_THREAD_STATE_DYING,
            Lifecycle::Dead => zx_types::ZX_THREAD_STATE_DEAD,
        }
    }

    /// Returns thread info for userspace.
    pub fn get_info_for_userspace(&self) -> zx_info_thread_t {
        self.state().canary.assert();
        let mut info = zx_info_thread_t::default();
        ksync::lock!(let guard = self.state().lock_lock());
        let fields = guard.fields();
        let blocked_reason =
            Blocked(self.state().blocked_reason.load(core::sync::atomic::Ordering::Acquire));
        info.state = Self::thread_lifecycle_to_state(fields.state.lifecycle(), blocked_reason);
        info.wait_exception_channel_type = *fields.exceptionate_type;
        // If we've exited then return with the lifecycle and exception type, and keep the
        // cpu_affinity_mask at 0.
        if let Some(core_thread) = self.try_core_thread_locked(guard.token()) {
            // Get CPU affinity. We assume that we can fit the entire mask in the first word
            // of cpu_affinity_mask.
            zr::static_assert!(
                crate::kernel::types::SMP_MAX_CPUS <= core::mem::size_of::<u64>() * 8
            );
            // SAFETY: `core_thread` is non-null and points to a valid kernel Thread.
            let affinity = unsafe { core_thread.get_soft_cpu_affinity() };
            info.cpu_affinity_mask.mask[0] = affinity as u64;
        }
        info
    }

    /// Returns thread CPU stats for userspace.
    pub fn get_stats_for_userspace(&self) -> Result<zx_info_thread_stats_t, Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);
        ksync::lock!(let guard = self.state().lock_lock());
        let core_thread = self.try_core_thread_locked(guard.token()).ok_or(Status::BAD_STATE)?;
        // SAFETY: `core_thread` is non-null and points to a valid kernel Thread.
        let total_runtime = unsafe { core_thread.runtime() }.0;
        // SAFETY: `core_thread` is non-null and points to a valid kernel Thread.
        let last_scheduled_cpu = unsafe { core_thread.last_cpu() };
        Ok(zx_info_thread_stats_t { total_runtime, last_scheduled_cpu, ..Default::default() })
    }

    /// This is called from Thread as it is exiting, just before it stops for good.
    /// It is an error to call this on anything other than the current thread.
    pub fn exiting_current(&self) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);
        {
            // Set ourselves in the Dying state before calling the Debugger.
            ksync::lock!(let mut guard = self.state().lock_lock());
            // SAFETY: `guard` holds `self`'s state lock.
            unsafe {
                self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Dying);
            }
        }

        // Notify a debugger if attached. Do this before marking the thread as
        // dead: the debugger expects to see the thread in the Dying state, it may
        // try to read thread registers. The debugger still has to handle the case
        // where the process is also dying (and thus the thread could transition
        // Dying -> Dead from underneath it), but that's life (or death :-)).
        //
        // Thread exit exceptions don't currently provide an iframe.
        if self.process().debug_exceptionate().has_valid_channel() {
            // SAFETY: `self` and `self.process()` are valid references, making the underlying
            // pointers valid.
            unsafe {
                cpp_thread_handle_single_shot_exiting(
                    self.process().as_ffi_mut(),
                    self as *const _ as *mut ThreadDispatcher,
                );
            }
        }

        // Mark the thread as dead. Do this before removing the thread from the
        // process because if this is the last thread then the process will be
        // marked dead, and we don't want to have a state where the process is
        // dead but one thread is not.
        {
            // Mark the thread as dead in the thread stats. The thread is technically
            // still running, but from a perspective of user-mode observable thread
            // stats, it is now dead. It is important to do this now, before we remove
            // ourselves from our process's collection. If we are the last thread to
            // leave the collection, user-mode will be signaled and will expect the
            // process to be dead (implying that its thread stats should no longer be
            // increasing). If we have not declared this thread to be dead yet, users
            // could continue to see the thread stats change even though they now
            // consider the process to be dead.
            {
                let _interrupt_disable = crate::arch_rs::InterruptDisableGuard::new();
                self.update_runtime_stats(ThreadStateKind::Death);
            }
            ksync::lock!(let mut guard = self.state().lock_lock());
            // put ourselves into the dead state
            // Then obtain the core thread lock and reset our core thread pointer.
            // SAFETY: `self` is a valid pointer, and `guard` holds `self`'s state lock.
            unsafe {
                self.set_state_locked(guard.as_mut().token_mut(), Lifecycle::Dead);
                cpp_thread_dispatcher_reset_core_thread(self as *const _ as *mut ThreadDispatcher);
            }
        }

        // Drop our exception channel endpoint so any userspace listener gets the PEER_CLOSED
        // signal.
        self.exceptionate().shutdown();
        // remove ourselves from our parent process's view
        // SAFETY: `self` is the current thread, so it was added to its process's thread list by
        // `add_initialized_thread` before it started running, and this is the only place a
        // thread is removed from that list, which it reaches exactly once as it exits.
        unsafe { self.process().remove_thread(self) };
        ltrace_exit_obj!(self);
    }

    /// Updates the runtime stats for this thread/process.
    ///
    /// This is called by Scheduler to update the runtime stats of the thread as it changes
    /// thread state (e.g. on context switches or scheduling state changes to account for time
    /// spent in each state).
    ///
    /// Must be called with interrupts disabled.
    pub fn update_runtime_stats(&self, new_state: ThreadStateKind) {
        self.state().canary.assert();
        // SAFETY: Storage pointer is valid and points to initialized ThreadRuntimeStats; the
        // caller ensures interrupts are disabled, which the shim asserts.
        unsafe {
            cpp_thread_runtime_stats_update(self.state().runtime_stats.as_void_ptr(), new_state);
        }
    }

    /// Updates time spent handling page faults. This is called by the VM during page fault
    /// handling.
    pub fn add_page_fault_ticks(&self, ticks: zx_types::zx_duration_mono_ticks_t) {
        self.state().canary.assert();
        // SAFETY: Storage pointer is valid and points to initialized ThreadRuntimeStats.
        unsafe {
            cpp_thread_runtime_stats_add_page_fault_ticks(
                self.state().runtime_stats.as_void_ptr(),
                ticks,
            );
        }
    }

    /// Updates time spent contended on locks. This is called by lock implementations.
    pub fn add_lock_contention_ticks(&self, ticks: zx_types::zx_duration_mono_ticks_t) {
        self.state().canary.assert();
        // SAFETY: Storage pointer is valid and points to initialized ThreadRuntimeStats.
        unsafe {
            let stats = self.state().runtime_stats.as_void_ptr();
            cpp_thread_runtime_stats_add_lock_contention_ticks(stats, ticks);
        }
    }

    /// Executes a closure with a reference to the current user thread's `ThreadDispatcher`.
    ///
    /// # Panics
    ///
    /// Panics if the calling thread is a kernel thread, which has no `ThreadDispatcher`. Every
    /// caller sets or restores the blocked reason of a user thread blocking in a syscall or
    /// exception, where that already holds.
    #[inline]
    pub fn with_current<R>(f: impl FnOnce(&ThreadDispatcher) -> R) -> R {
        // SAFETY: Foreign call to query the current user thread; it returns null (checked below)
        // rather than faulting when the caller is a kernel thread.
        let thread = unsafe { cpp_thread_dispatcher_current() };
        let thread = core::ptr::NonNull::new(thread)
            .expect("ThreadDispatcher::with_current called on a non-user thread");
        // SAFETY: `thread` is non-null, so the calling thread is a user thread and `thread`
        // points to its live `ThreadDispatcher`, which outlives this call because the thread is
        // running inside it.
        f(unsafe { thread.as_ref() })
    }

    /// Sets the blocked reason of the current thread and returns the previous one.
    #[inline]
    pub fn set_blocked_reason(reason: Blocked) -> Blocked {
        Self::with_current(|current| {
            // Only the current thread ever writes its own `blocked_reason`; other threads only
            // read it (`get_info_for_userspace`), so a separate load and store suffices and no
            // read-modify-write is needed.
            let blocked_reason = &current.state().blocked_reason;
            let prev = Blocked(blocked_reason.load(core::sync::atomic::Ordering::Acquire));
            blocked_reason.store(reason.0, core::sync::atomic::Ordering::Release);
            prev
        })
    }

    /// Returns thread task runtime stats.
    pub fn get_runtime_stats(&self) -> zx_info_task_runtime_t {
        self.state().canary.assert();
        let mut info = core::mem::MaybeUninit::<zx_info_task_runtime_t>::uninit();
        // SAFETY: `info` is a valid uninitialized out-pointer and the runtime stats pointer
        // is valid.
        unsafe {
            cpp_thread_runtime_stats_get_compensated(
                self.state().runtime_stats.as_void_ptr(),
                &raw mut info,
            );
            info.assume_init()
        }
    }

    /// Assuming the thread is stopped waiting for an exception response, returns the exception
    /// report, or `Err(Status::BAD_STATE)` if not in an exception.
    pub fn get_exception_report(&self) -> Result<zx_exception_report_t, Status> {
        self.state().canary.assert();
        ltrace_entry_obj!(self);

        ksync::lock!(let guard = self.state().lock_lock());
        if let Some(exception) = guard.fields().exception.as_ref() {
            // We always leave exception handling before the report gets wiped
            // so this must succeed.
            let report = exception.fill_report();
            debug_assert!(report.is_some());
            report.ok_or(Status::BAD_STATE)
        } else {
            Err(Status::BAD_STATE)
        }
    }

    /// Sends an exception over the exception channel and blocks for a response.
    ///
    /// Returns `(result, sent)`, where `sent` indicates whether the exception was successfully
    /// sent over the given `exceptionate` channel. This can be used in the `Err(Status::NEXT)`
    /// case to determine whether the exception channel didn't exist or it did exist but the
    /// receiver opted not to handle the exception.
    ///
    /// # Returns
    ///
    /// * `Ok(())`: If the exception was processed and the thread should resume.
    /// * `Err(Status::NEXT)`: If there is no channel or the receiver opted to skip.
    /// * `Err(Status::STOP)`: If the receiver requested `ZX_EXCEPTION_STATE_THREAD_EXIT`.
    /// * `Err(Status::NO_MEMORY)`: On allocation failure.
    /// * `Err(Status(ZX_ERR_INTERNAL_INTR_KILLED))`: If the thread was killed before receiving a
    ///   response.
    /// # Panics
    ///
    /// Panics if `self` is not the currently executing user thread.
    pub fn handle_exception(
        &self,
        exceptionate: &Exceptionate,
        exception: &RefPtr<ExceptionDispatcher>,
    ) -> (Result<(), Status>, bool) {
        self.state().canary.assert();
        ltrace_entry_obj!(self);
        debug_assert!(self.is_current());

        if !exceptionate.has_channel() {
            return (Err(Status::NEXT), false);
        }

        // Mark as blocked before sending the exception, otherwise a handler could
        // potentially query our state before we've marked ourself blocked.
        let _by = AutoBlocked::new(Blocked::EXCEPTION);

        {
            ksync::lock!(let mut guard = self.state().lock_lock());

            // We send the exception while locked so that if it succeeds we can
            // atomically update our state.
            if let Err(status) = exceptionate.send_exception(exception) {
                return (Err(status), false);
            }

            // This state is needed by `get_info_for_userspace()`.
            // SAFETY: `guard` holds `self`'s state lock.
            unsafe {
                self.set_exception_dispatcher_locked(
                    guard.as_mut().token_mut(),
                    exception.clone(),
                    exceptionate.type_(),
                );
            }
        }

        ltracef!("blocking on exception response\n");

        let mut result = exception.wait_for_handle_close();

        ltracef!("received exception response {}\n", Status::result_into_raw(result));

        ksync::lock!(let mut guard = self.state().lock_lock());

        // If both the thread was killed and the exception was resumed before we
        // started waiting, the exception resume status (ZX_OK) might be returned,
        // but we want thread kill to always take priority and stop exception
        // handling immediately.
        //
        // Note that this logic will always trigger for ZX_EXCP_THREAD_EXITING
        // whether the thread was killed or is exiting normally, but that's fine
        // because that exception only ever goes to a single handler so we ignore
        // the handler return value anyway.
        // SAFETY: `guard` holds `self`'s state lock.
        if unsafe { self.clear_exception_locked(guard.as_mut().token_mut()) } {
            result = Err(Status::err_from_raw(ZX_ERR_INTERNAL_INTR_KILLED));
        }

        (result, true)
    }

    /// Similar to [`Self::handle_exception`], but for single-shot exceptions which are sent to at
    /// most one handler (e.g. `ZX_EXCP_THREAD_STARTING`).
    ///
    /// The main difference is that this takes `exception_type` and `arch_context` rather than a
    /// full exception object, and internally sets up the required state and creates the exception
    /// object.
    ///
    /// Returns `true` if the exception was sent.
    ///
    /// # Safety
    ///
    /// * `self` must be the currently executing user thread (`self.is_current()`).
    /// * `self`'s dispatcher lock (`state().lock`) must not be held (`TA_EXCL(get_lock())`), nor
    ///   any thread lock, spinlock, or active `ChainLockTransaction`.
    /// * `arch_context` must point to a valid, initialized `arch_exception_context_t` for the
    ///   current thread that outlives this call, and if `(*arch_context).frame` is non-null, it
    ///   must point to a valid, initialized, writable `iframe_t` for the duration of the call
    ///   without conflicting Rust references.
    pub unsafe fn handle_single_shot_exception(
        &self,
        exceptionate: &Exceptionate,
        exception_type: zx_excp_type_t,
        arch_context: *const ArchExceptionContext,
    ) -> bool {
        self.state().canary.assert();
        ltrace_entry_obj!(self);
        debug_assert!(self.is_current());

        // Do a quick check for valid channel first. It's still possible that the
        // channel will become invalid immediately after this check, but that will
        // be caught when we try to send the exception. This is just an
        // optimization to avoid unnecessary setup/teardown in the common case.
        if !exceptionate.has_valid_channel() {
            return false;
        }

        // SAFETY: Caller guarantees `arch_context` points to a valid `arch_exception_context_t`.
        let report =
            unsafe { ExceptionDispatcher::build_arch_report(exception_type, arch_context) };

        // SAFETY: `report` and `arch_context` outlive `_cleanup` below, which calls
        // `exception.clear()` on all exit paths before `report` goes out of scope.
        let Some(exception) = (unsafe {
            ExceptionDispatcher::create(
                RefPtr::from_ref(self),
                exception_type,
                &report,
                arch_context,
            )
        }) else {
            core::hint::cold_path();
            kprintln!(
                "KERN: failed to allocate memory for exception type {:u} in thread {:u}.{:u}",
                exception_type,
                self.process().get_koid(),
                self.get_koid()
            );
            return false;
        };

        // We're about to handle the exception (`handle_exception`). Install `arch_context` on the
        // current thread to make its user register state available to debuggers and exception
        // handlers while the thread is "in exception".
        // SAFETY: Caller guarantees `self` is the current user thread, `arch_context` (and any
        // non-null `(*arch_context).frame`) is valid without conflicting references, and no
        // dispatcher lock, thread lock, spinlock, or active `ChainLockTransaction` is held.
        unsafe {
            with_exception_context(arch_context, || {
                let _cleanup = zr::defer(|| exception.clear());
                let (_result, sent) = self.handle_exception(exceptionate, &exception);
                sent
            })
        }
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

    /// Returns the thread's `Exceptionate`.
    #[inline]
    pub fn exceptionate(&self) -> &Exceptionate {
        self.assert_canary();
        &self.state().exceptionate
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

/// RAII observation of a [`ThreadDispatcher`]'s `core_thread_` under
/// [`ThreadDispatcherState::core_thread_lock`], mirroring C++
/// `ThreadDispatcher::CoreThreadObservation`.
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
        // `core_thread_lock`.
        unsafe { ThreadPtr::from_raw(self.core_thread) }
    }

    /// Releases `core_thread_lock` if an observation is active.
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
///
/// The values cross the FFI as raw `u32`s; the `static_assert`s in `thread_dispatcher_ffi.cc`
/// hold the C++ enum to this numbering.
// LINT.IfChange(Blocked)
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

    /// Returns the blocked reason `raw` names, or `None` if it is not a valid discriminant.
    pub(crate) const fn from_raw(raw: u32) -> Option<Self> {
        if raw <= Self::PAGER.0 { Some(Self(raw)) } else { None }
    }
}
// LINT.ThenChange(//zircon/kernel/object/thread_dispatcher_ffi.cc:thread_blocked)

/// RAII helper that sets the current thread's `blocked_reason` and restores it on drop.
pub struct AutoBlocked {
    prev_reason: Blocked,
}

impl AutoBlocked {
    /// Sets the blocked reason on the current thread.
    #[inline]
    pub fn new(reason: Blocked) -> Self {
        debug_assert!(reason != Blocked::NONE);
        let prev_reason = ThreadDispatcher::set_blocked_reason(reason);
        Self { prev_reason }
    }
}

impl Drop for AutoBlocked {
    #[inline]
    fn drop(&mut self) {
        ThreadDispatcher::set_blocked_reason(self.prev_reason);
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
    use super::super::exception_dispatcher::ExceptionDispatcher;
    use super::super::job_dispatcher::JobDispatcher;
    use super::super::process_dispatcher::ProcessDispatcher;
    use super::super::singleton::get_root_job_dispatcher;
    use super::{Blocked, Lifecycle, ThreadDispatcher, cpp_thread_dispatcher_reset_core_thread};
    use crate::arch_rs::{ArchExceptionContext, InterruptDisableGuard};
    use crate::kernel::scheduler_state::{DEFAULT_PRIORITY, SchedulerStateBaseProfile};
    use unittest::{assert_ok, expect_eq, expect_ok, expect_true, unwrap_ok};
    use zx_status::Status;

    /// Tests creating and destroying a thread without initializing it.
    #[test]
    fn test_create_destroy_thread_no_init() {
        let (job, _job_rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()), "job created");

        let (process, _process_rights, _vmar, _vmar_rights) = unwrap_ok!(
            ProcessDispatcher::create(job.dispatcher().clone(), b"k-ut-p0", 0),
            "process created"
        );

        let (thread, rights) = unwrap_ok!(
            ThreadDispatcher::create(process.dispatcher().clone(), 0, b"k-ut-t0"),
            "thread created"
        );
        expect_eq!(rights, ThreadDispatcher::default_rights());
        expect_eq!(thread.dispatcher().get_related_koid(), process.dispatcher().get_koid());
        expect_eq!(thread.dispatcher().process().get_koid(), process.dispatcher().get_koid());
        expect_eq!(thread.dispatcher().get_type(), zx_types::ZX_OBJ_TYPE_THREAD);
        expect_eq!(thread.dispatcher().is_current(), false);

        // Verify error paths when thread is in Initial state.
        expect_true!(
            thread
                .dispatcher()
                .set_base_profile(&SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, false))
                == Err(Status::BAD_STATE)
        );
        expect_true!(thread.dispatcher().set_soft_affinity(1) == Err(Status::BAD_STATE));

        thread.dispatcher().assert_canary();
    }

    /// Tests creating, initializing, and destroying a thread.
    #[test]
    fn test_create_init_destroy_thread() {
        let (job, _job_rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()), "job created");

        let (process, _process_rights, _vmar, _vmar_rights) = unwrap_ok!(
            ProcessDispatcher::create(job.dispatcher().clone(), b"k-ut-p1", 0),
            "process created"
        );

        let (thread, rights) = unwrap_ok!(
            ThreadDispatcher::create(process.dispatcher().clone(), 0, b"k-ut-t1"),
            "thread created"
        );
        expect_eq!(rights, ThreadDispatcher::default_rights());

        assert_ok!(thread.dispatcher().initialize(), "thread init");
        expect_eq!(thread.dispatcher().has_started(), false);
        expect_eq!(thread.dispatcher().is_dying_or_dead(), false);

        // Verify base profile setting.
        expect_ok!(
            thread
                .dispatcher()
                .set_base_profile(&SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, false))
        );

        // Verify userspace info on an initialized thread.
        let info = thread.dispatcher().get_info_for_userspace();
        expect_eq!(info.state, zx_types::ZX_THREAD_STATE_NEW);

        // Verify soft affinity setting.
        expect_ok!(thread.dispatcher().set_soft_affinity(1));
        let info_with_affinity = thread.dispatcher().get_info_for_userspace();
        expect_eq!(info_with_affinity.cpu_affinity_mask.mask[0], 1);

        // Verify that page fault and lock contention ticks are accumulated into the runtime
        // stats; the thread has never run, so its cpu time stays at zero.
        thread.dispatcher().add_page_fault_ticks(1000);
        thread.dispatcher().add_lock_contention_ticks(2000);
        let runtime_stats = thread.dispatcher().get_runtime_stats();
        expect_eq!(runtime_stats.cpu_time, 0);
        expect_true!(runtime_stats.page_fault_time > 0);
        expect_true!(runtime_stats.lock_contention_time > 0);
        expect_true!(runtime_stats.lock_contention_time > runtime_stats.page_fault_time);

        // Verify stats for userspace query.
        let stats = thread.dispatcher().get_stats_for_userspace();
        expect_true!(stats.is_ok());

        thread.dispatcher().assert_canary();
    }

    /// Tests mapping thread lifecycle and blocked reason to user-facing thread state.
    #[test]
    fn test_thread_lifecycle_to_state() {
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Initial, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_NEW
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Initialized, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_NEW
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_RUNNING
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::EXCEPTION),
            zx_types::ZX_THREAD_STATE_BLOCKED_EXCEPTION
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::SLEEPING),
            zx_types::ZX_THREAD_STATE_BLOCKED_SLEEPING
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::FUTEX),
            zx_types::ZX_THREAD_STATE_BLOCKED_FUTEX
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::PORT),
            zx_types::ZX_THREAD_STATE_BLOCKED_PORT
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::CHANNEL),
            zx_types::ZX_THREAD_STATE_BLOCKED_CHANNEL
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::WAIT_ONE),
            zx_types::ZX_THREAD_STATE_BLOCKED_WAIT_ONE
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::WAIT_MANY),
            zx_types::ZX_THREAD_STATE_BLOCKED_WAIT_MANY
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::INTERRUPT),
            zx_types::ZX_THREAD_STATE_BLOCKED_INTERRUPT
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Running, Blocked::PAGER),
            zx_types::ZX_THREAD_STATE_BLOCKED_PAGER
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Suspended, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_SUSPENDED
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Dying, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_DYING
        );
        expect_eq!(
            ThreadDispatcher::thread_lifecycle_to_state(Lifecycle::Dead, Blocked::NONE),
            zx_types::ZX_THREAD_STATE_DEAD
        );
    }

    /// Tests thread exception state, channel types, and clear_exception_locked.
    #[test]
    fn test_thread_exception_state() {
        let root_job = JobDispatcher::create_root_job();
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(root_job, b"k-ut-p2", 0).expect("process created");
        let (thread_handle, _rights) =
            ThreadDispatcher::create(proc_handle.dispatcher().clone(), 0, b"k-ut-t2")
                .expect("thread created");
        let thread = thread_handle.dispatcher();

        expect_ok!(thread.initialize());

        let report = zx_types::zx_exception_report_t {
            header: zx_types::zx_exception_header_t {
                size: core::mem::size_of::<zx_types::zx_exception_report_t>() as u32,
                type_: zx_types::ZX_EXCP_GENERAL,
            },
            // SAFETY: `zx_exception_context_t` is a C ABI struct of integer fields where all-zeroes
            // is a valid bit pattern.
            context: unsafe { core::mem::zeroed() },
        };
        let dummy_arch_context: u64 = 0;
        // SAFETY: `report` and `dummy_arch_context` remain valid on this stack frame until
        // `exception.clear()` is called below.
        let exception = unsafe {
            ExceptionDispatcher::create(
                thread.clone(),
                zx_types::ZX_EXCP_GENERAL,
                &raw const report,
                (&raw const dummy_arch_context).cast::<ArchExceptionContext>(),
            )
        }
        .expect("exception created");

        {
            ksync::lock!(let mut guard = thread.state().lock_lock());
            let token = guard.as_mut().token_mut();

            // SAFETY: `token` was obtained by locking `thread`'s state lock.
            unsafe {
                // Transition the state machine to Running before participating in exception
                // handling.
                thread.set_state_locked(token, Lifecycle::Running);

                // Initially not in exception or suspended.
                expect_eq!(thread.in_exception_locked(token), false);
                expect_eq!(thread.suspended_or_in_exception_locked(token), false);

                thread.set_exception_dispatcher_locked(
                    token,
                    exception.clone(),
                    zx_types::ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER,
                );

                expect_eq!(thread.in_exception_locked(token), true);
                expect_eq!(thread.suspended_or_in_exception_locked(token), true);
            }
        }

        // Check get_info_for_userspace and get_exception_report while in exception.
        // The lock guard is released before calling these methods, which acquire it.
        let info = thread.get_info_for_userspace();
        expect_eq!(info.wait_exception_channel_type, zx_types::ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER);
        let fetched_report = unwrap_ok!(thread.get_exception_report(), "exception report fetched");
        expect_eq!(fetched_report.header.type_, zx_types::ZX_EXCP_GENERAL);

        {
            ksync::lock!(let mut guard = thread.state().lock_lock());
            let token = guard.as_mut().token_mut();

            // clear_exception_locked returns false when thread is not dying/dead.
            // SAFETY: `token` was obtained by locking `thread`'s state lock.
            unsafe {
                expect_eq!(thread.clear_exception_locked(token), false);
                expect_eq!(thread.in_exception_locked(token), false);
            }
        }

        let info_cleared = thread.get_info_for_userspace();
        expect_eq!(
            info_cleared.wait_exception_channel_type,
            zx_types::ZX_EXCEPTION_CHANNEL_TYPE_NONE
        );

        {
            ksync::lock!(let mut guard = thread.state().lock_lock());
            let token = guard.as_mut().token_mut();

            // SAFETY: `token` was obtained by locking `thread`'s state lock.
            unsafe {
                // Transition to Dying so clear_exception_locked returns true.
                thread.set_state_locked(token, Lifecycle::Dying);

                thread.set_exception_dispatcher_locked(
                    token,
                    exception.clone(),
                    zx_types::ZX_EXCEPTION_CHANNEL_TYPE_THREAD,
                );
                // clear_exception_locked returns true when thread is dying or dead.
                expect_eq!(thread.clear_exception_locked(token), true);
            }
        }

        exception.clear();

        // Clean up the allocated core thread since it was never started.
        {
            ksync::lock!(let guard = thread.state().lock_lock());
            if let Some(core_thread) = thread.try_core_thread_locked(guard.token()) {
                // SAFETY: `thread` is valid and `guard` holds the state lock.
                unsafe {
                    cpp_thread_dispatcher_reset_core_thread(
                        core::ptr::from_ref(&**thread).cast_mut(),
                    );
                }
                // SAFETY: `core_thread` was initialized but never started, so forgetting it is
                // safe.
                unsafe { core_thread.forget() };
            }
        }
    }

    /// Tests observing the core thread and resetting it under the state lock.
    #[test]
    fn test_observe_core_thread_and_reset() {
        let root_job = JobDispatcher::create_root_job();
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(root_job, b"k-ut-p3", 0).expect("process created");
        let (thread_handle, _rights) =
            ThreadDispatcher::create(proc_handle.dispatcher().clone(), 0, b"k-ut-t3")
                .expect("thread created");
        let thread = thread_handle.dispatcher();

        {
            ksync::lock!(let guard = thread.state().lock_lock());
            let token = guard.token();
            let core_thread = thread.try_core_thread_locked(token);
            expect_true!(core_thread.is_some());

            {
                let _irqd = InterruptDisableGuard::new();
                // SAFETY: `token` is from `thread`'s state lock and interrupts are disabled.
                let obs = unsafe { thread.observe_core_thread_locked(token) };
                expect_true!(obs.core_thread().is_some());
            }

            // Reset the core thread under the state lock. A CoreThreadObservation is only valid
            // while the thread still has a core thread, so the reset is verified through the plain
            // accessor rather than by observing again.
            // SAFETY: `thread` is valid and `guard` holds the state lock.
            unsafe {
                cpp_thread_dispatcher_reset_core_thread(core::ptr::from_ref(&**thread).cast_mut());
            }
            expect_true!(thread.try_core_thread_locked(token).is_none());

            if let Some(core_thread) = core_thread {
                // SAFETY: `core_thread` was initialized but never started, so forgetting it cleans
                // up.
                unsafe { core_thread.forget() };
            }
        }
    }
}
