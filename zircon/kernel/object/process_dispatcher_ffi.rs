// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::Dispatcher;
use super::handle::{HandleValue, KernelHandle};
use super::job_dispatcher::JobDispatcher;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::ThreadDispatcher;
use zx_types::{zx_info_process_t, zx_rights_t, zx_status_t, zx_vaddr_t};

unsafe extern "C" {
    /// Returns a raw pointer to the current process dispatcher.
    ///
    /// # Safety
    ///
    /// The caller must only call this when executing within a valid thread context.
    pub(crate) fn cpp_process_dispatcher_current() -> *const ProcessDispatcher;

    /// Checks if the given ProcessDispatcher is the current process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_is_current(process: *const ProcessDispatcher) -> bool;

    /// Calls into C++ implementation to start a process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    /// `thread` must point to a valid `ThreadDispatcher`.
    /// `arg_handle` must point to a valid raw handle or be null.
    pub(crate) fn cpp_process_dispatcher_start(
        process: *const ProcessDispatcher,
        thread: *const ThreadDispatcher,
        pc: zx_vaddr_t,
        sp: zx_vaddr_t,
        arg_handle: *mut core::ffi::c_void,
        arg2: usize,
    ) -> zx_status_t;

    /// Calls into C++ implementation to kill a process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_kill(process: *const ProcessDispatcher, retcode: i64);

    /// Calls into C++ implementation to suspend a process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_suspend(process: *mut ProcessDispatcher) -> zx_status_t;

    /// Calls into C++ implementation to resume a process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_resume(process: *mut ProcessDispatcher);

    /// Creates and adds a handle to the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    /// `handle` must point to a valid `KernelHandle<Dispatcher>`.
    /// `out_handle` must point to writable memory.
    pub(crate) fn cpp_process_dispatcher_make_and_add_handle(
        process: *const ProcessDispatcher,
        handle: *mut KernelHandle<Dispatcher>,
        rights: zx_rights_t,
        out_handle: *mut HandleValue,
    ) -> zx_status_t;

    /// Creates and adds a handle from a RefPtr to the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    /// `dispatcher` must be a valid `fbl::RefPtr<Dispatcher>`.
    /// `out_handle` must point to writable memory.
    pub(crate) fn cpp_process_dispatcher_make_and_add_handle_from_ref(
        process: *const ProcessDispatcher,
        raw_dispatcher: *const Dispatcher,
        rights: zx_rights_t,
        out_handle: *mut HandleValue,
    ) -> zx_status_t;

    /// Retrieves a dispatcher and rights from the handle table of the current process.
    ///
    /// Upon success, the `out_dispatcher` argument is initialized by C++ to contain a
    /// `fbl::RefPtr<Dispatcher>` pointing to the dispatcher associated with the given handle.
    /// The caller typically uses `MaybeUninit::uninit()` and checks the return status to
    /// determine if C++ initialized the value.
    ///
    /// # Safety
    ///
    /// `out_dispatcher` must point to valid uninitialized memory for a `fbl::RefPtr<Dispatcher>`.
    /// `out_rights` must point to valid uninitialized memory for a `zx_rights_t`.
    pub(crate) fn cpp_handle_table_get_dispatcher(
        handle: HandleValue,
        out_dispatcher: *mut fbl::RefPtr<Dispatcher>,
        out_rights: *mut zx_rights_t,
    ) -> zx_status_t;

    /// Enforces basic policy for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_enforce_basic_policy(
        process: *const ProcessDispatcher,
        policy: u32,
    ) -> zx_status_t;

    /// Returns the timer slack policy amount for the given process.
    pub(crate) fn cpp_process_dispatcher_get_timer_slack_policy_amount(
        process: *const ProcessDispatcher,
    ) -> i64;

    /// Retrieves process info from C++.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_info(
        process: *const ProcessDispatcher,
    ) -> zx_info_process_t;

    /// Sets a process as critical to a job.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    /// `job` must point to a valid `JobDispatcher` whose reference was transferred.
    pub(crate) fn cpp_process_dispatcher_set_critical_to_job(
        process: *mut ProcessDispatcher,
        job: *mut JobDispatcher,
        retcode_nonzero: bool,
    ) -> zx_status_t;
}
