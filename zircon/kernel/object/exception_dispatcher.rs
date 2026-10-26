// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exception_dispatcher_ffi::cpp_exception_dispatcher_create;
use super::handle::HandleOwner;
use super::thread_dispatcher::ThreadDispatcher;
use crate::arch_rs::{ArchExceptionContext, arch_fill_in_exception_context};
use crate::counters::define_kcounter;
use crate::kernel::deadline::Deadline;
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::THREAD_SIGNAL_SUSPEND;
use core::ptr::NonNull;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::FromZeros;
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_EXCEPTION_RIGHTS, ZX_ERR_INTERNAL_INTR_KILLED, ZX_EXCEPTION_STATE_HANDLED,
    ZX_EXCEPTION_STATE_THREAD_EXIT, ZX_EXCEPTION_STATE_TRY_NEXT, ZX_OBJ_TYPE_EXCEPTION,
    zx_exception_report_t, zx_excp_type_t, zx_rights_t,
};

zr::static_assert_size_and_align!(
    ExceptionDispatcherState,
    object_constants::kExceptionDispatcherStateSize,
    object_constants::kExceptionDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_EXCEPTION_CREATE_COUNT, "dispatcher.exception.create", Sum);
define_kcounter!(DISPATCHER_EXCEPTION_DESTROY_COUNT, "dispatcher.exception.destroy", Sum);

/// Internal state storage for `ExceptionDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ExceptionDispatcherState {
    canary: Canary<{ fbl::magic(b"EXCD") }>,

    /// Set during construction and immutable thereafter, so does not need to be guarded by `lock`.
    thread: RefPtr<ThreadDispatcher>,
    /// Set during construction and immutable thereafter, so does not need to be guarded by `lock`.
    exception_type: zx_excp_type_t,

    /// Updated by `Exceptionate` whenever we get transmitted, according to the rights that
    /// specific `Exceptionate` was registered with.
    #[guarded_by(lock)]
    thread_rights: zx_rights_t,
    /// Updated by `Exceptionate` whenever we get transmitted, according to the rights that
    /// specific `Exceptionate` was registered with.
    #[guarded_by(lock)]
    process_rights: zx_rights_t,

    /// Cleared if the underlying thread is killed or finishes exception handling while userspace
    /// still has access to this exception.
    #[guarded_by(lock)]
    report: Option<NonNull<zx_exception_report_t>>,
    /// Cleared if the underlying thread is killed or finishes exception handling while userspace
    /// still has access to this exception.
    #[guarded_by(lock)]
    arch_context: Option<NonNull<ArchExceptionContext>>,

    #[guarded_by(lock)]
    disposition: u32,
    #[guarded_by(lock)]
    second_chance: bool,

    #[pin]
    response_event: AutounsignalEvent,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

// SAFETY: `report` and `arch_context` are protected by `lock` and cleared before the pointed-to
// stack frames go out of scope.
unsafe impl Send for ExceptionDispatcherState {}
// SAFETY: Concurrent access to `ExceptionDispatcherState` is synchronized by `lock` and
// `response_event`.
unsafe impl Sync for ExceptionDispatcherState {}

impl ExceptionDispatcherState {
    /// Initializes an `ExceptionDispatcherState` in place.
    pub fn init(
        _dispatcher: *const ExceptionDispatcher,
        thread: RefPtr<ThreadDispatcher>,
        exception_type: zx_excp_type_t,
        report: *const zx_exception_report_t,
        arch_context: *const ArchExceptionContext,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        debug_assert!(!report.is_null());
        debug_assert!(!arch_context.is_null());
        pin_init!(Self {
            canary: {
                DISPATCHER_EXCEPTION_CREATE_COUNT.add(1);
                Canary::new()
            },
            thread,
            exception_type,
            thread_rights: 0.into(),
            process_rights: 0.into(),
            report: NonNull::new(report.cast_mut()).into(),
            arch_context: NonNull::new(arch_context.cast_mut()).into(),
            disposition: ZX_EXCEPTION_STATE_TRY_NEXT.into(),
            second_chance: false.into(),
            response_event <- AutounsignalEvent::init_unsignaled(),
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ExceptionDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        self.canary.assert();
        DISPATCHER_EXCEPTION_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Zircon channel-based exception handling uses two primary classes, `ExceptionDispatcher`
    /// and `Exceptionate`.
    ///
    /// An `ExceptionDispatcher` represents a single currently-active exception. This will be
    /// transmitted to registered exception handlers in userspace and provides them with exception
    /// state and control functionality.
    pub struct ExceptionDispatcher,
    ExceptionDispatcherState,
    ZX_OBJ_TYPE_EXCEPTION,
    object_constants::kExceptionDispatcherStateOffset
);

impl ExceptionDispatcher {
    /// Returns the default rights for an `ExceptionDispatcher` handle.
    pub const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_EXCEPTION_RIGHTS
    }

    /// Builds an architecture-specific `zx_exception_report_t` from `context`.
    ///
    /// # Safety
    ///
    /// `context` must point to a valid, initialized `arch_exception_context_t`.
    pub unsafe fn build_arch_report(
        exception_type: u32,
        context: *const ArchExceptionContext,
    ) -> zx_exception_report_t {
        let mut report = zx_exception_report_t::new_zeroed();
        report.header.size = core::mem::size_of::<zx_exception_report_t>() as u32;
        report.header.type_ = exception_type;
        // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`, and
        // `report` is a valid writable local.
        unsafe {
            arch_fill_in_exception_context(context, &mut report);
        }
        report
    }

    /// Creates a new `ExceptionDispatcher`, returning `None` on memory allocation failure.
    ///
    /// # Safety
    ///
    /// `report` and `arch_context` must remain valid until [`Self::clear`] is called or the
    /// returned `ExceptionDispatcher` is destroyed.
    pub unsafe fn create(
        thread: RefPtr<ThreadDispatcher>,
        exception_type: zx_excp_type_t,
        report: *const zx_exception_report_t,
        arch_context: *const ArchExceptionContext,
    ) -> Option<RefPtr<Self>> {
        // SAFETY: `RefPtr::into_raw(thread)` transfers ownership of one reference count to
        // `cpp_exception_dispatcher_create`, which returns an owned raw pointer or null.
        unsafe {
            let raw = cpp_exception_dispatcher_create(
                RefPtr::into_raw(thread).cast_mut(),
                exception_type,
                report,
                arch_context,
            );
            RefPtr::try_from_raw(raw)
        }
    }

    /// Returns a reference to the exception's thread.
    pub fn thread(&self) -> &RefPtr<ThreadDispatcher> {
        let state = self.state();
        state.canary.assert();
        &state.thread
    }

    /// Returns the exception type (`ZX_EXCP_*`).
    pub fn exception_type(&self) -> zx_excp_type_t {
        let state = self.state();
        state.canary.assert();
        state.exception_type
    }

    /// Marks the current exception handler as done.
    ///
    /// Once a handle has been created around this object, either [`Self::wait_for_handle_close`]
    /// or [`Self::discard_handle_close`] must be called to reset state for the next handler.
    pub fn on_zero_handles(&self) {
        let state = self.state();
        state.canary.assert();
        state.response_event.signal();
    }

    /// Returns a copy of the exception report provided at `ExceptionDispatcher` creation, or
    /// `None` if the exception thread has died.
    pub fn fill_report(&self) -> Option<zx_exception_report_t> {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let guard = state.lock_lock());
        // SAFETY: `report_ptr` is non-null and valid until `clear()` is called under `lock`.
        (*guard.fields().report).map(|report_ptr| unsafe { *report_ptr.as_ptr() })
    }

    /// Sets the task rights to use for subsequent handle creation.
    ///
    /// `rights == 0` indicates that the current exception handler is not allowed to access the
    /// corresponding task handle, for example a thread-level handler cannot access its parent
    /// process handle.
    ///
    /// This must only be called by an `Exceptionate` before transmitting the exception - we don't
    /// ever want to be changing task rights while the exception is out in userspace.
    pub fn set_task_rights(&self, thread_rights: zx_rights_t, process_rights: zx_rights_t) {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let mut guard = state.lock_lock());
        let fields = guard.as_mut().fields_mut();
        *fields.thread_rights = thread_rights;
        *fields.process_rights = process_rights;
    }

    /// Creates a new thread handle for the exception's thread.
    ///
    /// # Errors
    ///
    /// * [`Status::ACCESS_DENIED`]: If the thread task rights have been set to 0.
    /// * [`Status::NO_MEMORY`]: If the `Handle` failed to allocate.
    pub fn make_thread_handle(&self) -> Result<HandleOwner, Status> {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let guard = state.lock_lock());
        let thread_rights = *guard.fields().thread_rights;
        if thread_rights == 0 {
            return Err(Status::ACCESS_DENIED);
        }

        HandleOwner::make_from_ref(state.thread.clone(), thread_rights).ok_or(Status::NO_MEMORY)
    }

    /// Creates a new process handle for the exception's process.
    ///
    /// # Errors
    ///
    /// * [`Status::ACCESS_DENIED`]: If the process task rights have been set to 0.
    /// * [`Status::NO_MEMORY`]: If the `Handle` failed to allocate.
    pub fn make_process_handle(&self) -> Result<HandleOwner, Status> {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let guard = state.lock_lock());
        let process_rights = *guard.fields().process_rights;
        if process_rights == 0 {
            return Err(Status::ACCESS_DENIED);
        }

        // We have a RefPtr to `thread` so it can't die, and the thread keeps its
        // process alive, so we know the process is safe to wrap in a RefPtr.
        HandleOwner::make_from_ref(RefPtr::from_ref(state.thread.process()), process_rights)
            .ok_or(Status::NO_MEMORY)
    }

    /// Returns whether to resume the thread on exception close, pass it to the next handler in
    /// line, or kill the thread (`ZX_EXCEPTION_STATE_*`).
    pub fn get_disposition(&self) -> u32 {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let guard = state.lock_lock());
        *guard.fields().disposition
    }

    /// Sets the exception disposition (`ZX_EXCEPTION_STATE_*`).
    pub fn set_disposition(&self, disposition: u32) {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let mut guard = state.lock_lock());
        *guard.as_mut().fields_mut().disposition = disposition;
    }

    /// Returns whether a debugger should have a second chance to handle the exception after the
    /// process handler has tried and failed to do so.
    pub fn is_second_chance(&self) -> bool {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let guard = state.lock_lock());
        *guard.fields().second_chance
    }

    /// Sets whether a debugger should have a second chance to handle the exception.
    pub fn set_whether_second_chance(&self, second_chance: bool) {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let mut guard = state.lock_lock());
        *guard.as_mut().fields_mut().second_chance = second_chance;
    }

    /// Blocks until the exception handler is done processing.
    ///
    /// This must be called exactly once every time this exception is successfully sent out to
    /// userspace, in order to wait for the response and reset the internal state.
    ///
    /// # Returns
    ///
    /// * `Ok(())`: If the exception was handled and the thread should resume.
    /// * `Err(Status::NEXT)`: If the exception should be passed to the next handler.
    /// * `Err(Status::STOP)`: If the handler requested `ZX_EXCEPTION_STATE_THREAD_EXIT`.
    /// * `Err(Status(ZX_ERR_INTERNAL_INTR_KILLED))`: If the thread was killed.
    pub fn wait_for_handle_close(&self) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        loop {
            // Continue to wait for the exception response if we get suspended.
            // Both the suspension and the exception need to be closed out before
            // the thread can resume.
            // The THREAD_SIGNAL_SUSPEND signal_mask checks for a suspend signal that might have
            // come in before the wait_mask() call. And the status check for
            // Status::INTERRUPTED_RETRY in the loop checks for a suspend that gets signaled while
            // the wait is ongoing.
            match state.response_event.wait_mask(&Deadline::infinite(), THREAD_SIGNAL_SUSPEND) {
                Ok(()) => break,
                Err(Status::INTERRUPTED_RETRY) => continue,
                Err(status) if status.into_raw() == ZX_ERR_INTERNAL_INTR_KILLED => {
                    // If the thread was killed it doesn't matter whether the handler
                    // wanted to resume or not.
                    return Err(status);
                }
                Err(status) => {
                    // Our event wait should only ever return one of the internal errors
                    // handled above or the ZX_OK we send in on_zero_handles().
                    panic!("unexpected exception event result: {}\n", status.into_raw());
                }
            }
        }

        // Return the close action and reset it for next time.
        ksync::lock!(let mut guard = state.lock_lock());
        let fields = guard.as_mut().fields_mut();
        let result = match *fields.disposition {
            ZX_EXCEPTION_STATE_HANDLED => Ok(()),
            ZX_EXCEPTION_STATE_THREAD_EXIT => Err(Status::STOP),
            _ => Err(Status::NEXT),
        };
        *fields.disposition = ZX_EXCEPTION_STATE_TRY_NEXT;
        result
    }

    /// Resets the exception state for the next handler.
    ///
    /// This must be called instead of [`Self::wait_for_handle_close`] if a handle is created
    /// around this exception but fails to make it out to userspace, in order to reset the
    /// internal state.
    pub fn discard_handle_close(&self) {
        let state = self.state();
        state.canary.assert();

        let _ = state.response_event.unsignal();

        ksync::lock!(let mut guard = state.lock_lock());
        *guard.as_mut().fields_mut().disposition = ZX_EXCEPTION_STATE_TRY_NEXT;
    }

    /// Wipes out exception state, which indicates the thread has died or finished handling the
    /// exception.
    pub fn clear(&self) {
        let state = self.state();
        state.canary.assert();

        ksync::lock!(let mut guard = state.lock_lock());
        let fields = guard.as_mut().fields_mut();
        *fields.report = None;
        *fields.arch_context = None;
    }

    /// Returns whether the internal response event is currently signaled (for unit tests).
    #[cfg(ktest)]
    pub(crate) fn is_response_event_signaled(&self) -> bool {
        let state = self.state();
        state.canary.assert();
        state.response_event.is_signaled()
    }
}
