// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::DispatcherOps;
use super::handle::{HandleOwner, HandleValue, KernelHandle};
use super::job_dispatcher::JobDispatcher;
use super::process_dispatcher_ffi::{
    cpp_process_dispatcher_current, cpp_process_dispatcher_enforce_basic_policy,
    cpp_process_dispatcher_get_info, cpp_process_dispatcher_is_current,
    cpp_process_dispatcher_kill, cpp_process_dispatcher_make_and_add_handle,
    cpp_process_dispatcher_resume, cpp_process_dispatcher_set_critical_to_job,
    cpp_process_dispatcher_start, cpp_process_dispatcher_suspend,
};
use super::thread_dispatcher::ThreadDispatcher;
use pin_init::{PinInit, pin_data, pin_init};
use zx_status::Status;
use zx_types::{zx_info_process_t, zx_rights_t, zx_vaddr_t};

/// Lock class tag for the handle table's reader-writer lock.
pub struct HandleTableLockClass;

impl ksync::LockClass for HandleTableLockClass {
    const ID: *mut core::ffi::c_void = core::ptr::null_mut();
}

crate::object::dispatcher::impl_dispatcher_facade!(
    pub struct ProcessDispatcher,
    zx_types::ZX_OBJ_TYPE_PROCESS
);

impl ProcessDispatcher {
    /// Executes the given function with a reference to the current process.
    pub fn with_current<R>(f: impl FnOnce(&ProcessDispatcher) -> R) -> R {
        // SAFETY: The current process is guaranteed to be valid for the duration of the call.
        let proc = unsafe { &*cpp_process_dispatcher_current() };
        f(proc)
    }

    /// Returns whether this `ProcessDispatcher` is the current process.
    pub fn is_current(&self) -> bool {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_is_current(self as *const _) }
    }

    /// Starts execution of this process.
    pub fn start(
        &self,
        thread: &ThreadDispatcher,
        pc: zx_vaddr_t,
        sp: zx_vaddr_t,
        arg_handle: HandleOwner,
        arg2: usize,
    ) -> Result<(), Status> {
        // SAFETY: `self` and `thread` are valid references, and `arg_handle` ownership is transferred to C++.
        let status = unsafe {
            cpp_process_dispatcher_start(
                self as *const _,
                thread as *const _,
                pc,
                sp,
                arg_handle.release(),
                arg2,
            )
        };
        Status::ok(status)
    }

    /// Kills this process with the given return code.
    pub fn kill(&self, retcode: i64) {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_kill(self as *const _, retcode) }
    }

    /// Suspends execution of this process.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_STATE` if the process is dying or dead.
    pub fn suspend(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let status = unsafe { cpp_process_dispatcher_suspend(self as *const _ as *mut _) };
        Status::ok(status)
    }

    /// Resumes execution of this process.
    pub fn resume(&self) {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_resume(self as *const _ as *mut _) }
    }

    /// Creates a handle for the given dispatcher in this process's handle table.
    pub fn make_and_add_handle<T>(
        &self,
        handle: KernelHandle<T>,
        rights: zx_rights_t,
    ) -> Result<HandleValue, Status>
    where
        T: fbl::HasRefCount + fbl::Recyclable + DispatcherOps,
    {
        let mut handle = handle.cast();
        let mut out = HandleValue::default();
        // SAFETY: `self` is a valid `ProcessDispatcher`, `handle` is a valid `KernelHandle`, and
        // `out` points to writable memory.
        let status = unsafe {
            cpp_process_dispatcher_make_and_add_handle(
                self as *const _,
                &mut handle,
                rights,
                &mut out,
            )
        };
        Status::ok(status)?;
        Ok(out)
    }

    /// Creates a handle for the given dispatcher reference in this process's handle table.
    pub fn make_and_add_handle_from_ref<T>(
        &self,
        dispatcher: fbl::RefPtr<T>,
        rights: zx_rights_t,
    ) -> Result<HandleValue, Status>
    where
        T: fbl::HasRefCount + fbl::Recyclable + DispatcherOps,
    {
        // SAFETY: T implements DispatcherOps and is layout-compatible with Dispatcher.
        let raw_dispatcher =
            fbl::RefPtr::into_raw(unsafe { dispatcher.cast::<super::Dispatcher>() });
        let mut out = HandleValue::default();
        // SAFETY: `self` is a valid `ProcessDispatcher`, `raw_dispatcher` carries an acquired reference count
        // transferred to C++, and `out` points to writable memory.
        let status = unsafe {
            super::process_dispatcher_ffi::cpp_process_dispatcher_make_and_add_handle_from_ref(
                self as *const _,
                raw_dispatcher,
                rights,
                &mut out,
            )
        };
        Status::ok(status)?;
        Ok(out)
    }

    /// Enforces basic policy for this process.
    pub fn enforce_basic_policy(&self, policy: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let status =
            unsafe { cpp_process_dispatcher_enforce_basic_policy(self as *const _, policy) };
        Status::ok(status)
    }

    /// Returns the timer slack policy amount for this process.
    pub fn get_timer_slack_policy_amount(&self) -> i64 {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe {
            super::process_dispatcher_ffi::cpp_process_dispatcher_get_timer_slack_policy_amount(
                self as *const _,
            )
        }
    }

    /// Returns the timer slack policy for this process.
    pub fn get_timer_slack_policy(&self) -> crate::kernel::types::TimerSlack {
        let mut slack = crate::kernel::types::TimerSlack::none();
        // SAFETY: `self` is a valid `ProcessDispatcher` reference and `slack` points to valid memory.
        unsafe {
            super::process_dispatcher_ffi::cpp_process_dispatcher_get_timer_slack_policy(
                self as *const _,
                &mut slack,
            );
        }
        slack
    }

    /// Returns a reference to the handle table's priority-inheriting reader-writer lock.
    #[inline]
    pub fn handle_table_lock(&self) -> &ksync::BrwLockPi<HandleTableLockClass> {
        // SAFETY: `self` is a valid `ProcessDispatcher`, and its handle table lock is a valid `BrwLockPi`.
        unsafe {
            let lock_ptr = super::process_dispatcher_ffi::cpp_process_dispatcher_handle_table_lock(
                self as *const _,
            );
            &*(lock_ptr as *const ksync::BrwLockPi<HandleTableLockClass>)
        }
    }

    /// Returns information about this process.
    pub fn get_info(&self) -> zx_info_process_t {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_get_info(self as *const _) }
    }

    /// Sets this process as critical to the given job.
    pub fn set_critical_to_job(
        &self,
        job: fbl::RefPtr<JobDispatcher>,
        retcode_nonzero: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference, and `job` transfers an acquired
        // reference count into C++.
        let status = unsafe {
            cpp_process_dispatcher_set_critical_to_job(
                self as *const _ as *mut _,
                fbl::RefPtr::into_raw(job) as *mut _,
                retcode_nonzero,
            )
        };
        Status::ok(status)
    }
}

/// RAII reader lock guard for a process's handle table.
///
/// Encapsulates the reader lock on the handle table, ensuring that handle lookups and rights
/// checks can only occur while the lock is held.
#[pin_data]
pub struct HandleTableReadGuard<'a> {
    process: &'a ProcessDispatcher,
    #[pin]
    guard: ksync::BrwLockPiReadGuard<'a, HandleTableLockClass>,
}

impl<'a> HandleTableReadGuard<'a> {
    /// Creates a stack-pinned handle table reader lock guard for `process`.
    pub fn new(process: &'a ProcessDispatcher) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            process,
            guard <- process.handle_table_lock().read_lock(),
        })
    }

    /// Retrieves a handle reference while holding the handle table lock.
    pub fn get_handle(&self, handle_value: HandleValue) -> Option<super::handle::HandleRef<'_>> {
        // SAFETY: `self.process` is valid and the handle table lock is held for the duration of `self`.
        let ptr = unsafe {
            super::process_dispatcher_ffi::cpp_process_dispatcher_handle_table_get_handle_locked(
                self.process as *const _,
                handle_value.raw_value(),
            )
        };
        core::ptr::NonNull::new(ptr).map(|ptr| unsafe { super::handle::HandleRef::from_raw(ptr) })
    }
}
