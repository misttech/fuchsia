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
use zx_types::{zx_handle_t, zx_info_process_t, zx_rights_t, zx_status_t, zx_vaddr_t};

unsafe extern "C" {
    /// Returns a raw pointer to the current process dispatcher.
    ///
    /// # Safety
    ///
    /// The caller must only call this when executing within a valid thread context.
    pub(crate) fn cpp_process_dispatcher_current() -> *mut ProcessDispatcher;

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
    /// `thread` must carry an acquired reference count transferred to C++.
    /// `arg_handle` must be a valid raw owned handle or null.
    pub(crate) fn cpp_process_dispatcher_start(
        process: *mut ProcessDispatcher,
        thread: *mut ThreadDispatcher,
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
    pub(crate) fn cpp_process_dispatcher_kill(process: *mut ProcessDispatcher, retcode: i64);

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
        process: *mut ProcessDispatcher,
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
        process: *mut ProcessDispatcher,
        raw_dispatcher: *mut Dispatcher,
        rights: zx_rights_t,
        out_handle: *mut HandleValue,
    ) -> zx_status_t;

    /// Retrieves a dispatcher and rights from the handle table of the current process.
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

    /// Removes a handle from the given process's handle table and returns the raw handle pointer.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_remove_handle(
        process: *mut ProcessDispatcher,
        handle: zx_handle_t,
    ) -> *mut core::ffi::c_void;

    /// Enforces basic policy for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_enforce_basic_policy(
        process: *mut ProcessDispatcher,
        policy: u32,
    ) -> zx_status_t;

    /// Returns the timer slack policy amount for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_timer_slack_policy_amount(
        process: *const ProcessDispatcher,
    ) -> i64;

    /// Returns the timer slack policy for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and `out_slack` must point to valid memory.
    pub(crate) fn cpp_process_dispatcher_get_timer_slack_policy(
        process: *const ProcessDispatcher,
        out_slack: *mut crate::kernel::deadline::TimerSlack,
    );

    /// Returns a pointer to the handle table's BrwLockPi lock for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_handle_table_lock(
        process: *const ProcessDispatcher,
    ) -> *mut core::ffi::c_void;

    /// Looks up a handle in the handle table while holding the lock.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and the handle table lock must be held.
    pub(crate) fn cpp_process_dispatcher_handle_table_get_handle_locked(
        process: *const ProcessDispatcher,
        handle_value: zx_types::zx_handle_t,
    ) -> *mut core::ffi::c_void;

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

    /// Creates a new process and its root VMAR under the specified job.
    ///
    /// # Safety
    ///
    /// `job` must point to a valid `JobDispatcher` whose reference count was transferred.
    /// `name_ptr` must point to `name_len` readable bytes.
    /// Output handle and rights pointers must point to writable memory.
    pub(crate) fn cpp_process_dispatcher_create(
        job: *mut JobDispatcher,
        name_ptr: *const core::ffi::c_char,
        name_len: usize,
        flags: u32,
        out_proc_handle: *mut KernelHandle<ProcessDispatcher>,
        out_proc_rights: *mut zx_rights_t,
        out_vmar_handle: *mut KernelHandle<
            super::vm_address_region_dispatcher::VmAddressRegionDispatcher,
        >,
        out_vmar_rights: *mut zx_rights_t,
    ) -> zx_status_t;

    /// Creates a new shared process that shares state with `shared_proc`.
    ///
    /// # Safety
    ///
    /// `shared_proc` must point to a valid `ProcessDispatcher` whose reference count was transferred.
    /// `name_ptr` must point to `name_len` readable bytes.
    /// Output handle and rights pointers must point to writable memory.
    pub(crate) fn cpp_process_dispatcher_create_shared(
        shared_proc: *mut ProcessDispatcher,
        name_ptr: *const core::ffi::c_char,
        name_len: usize,
        flags: u32,
        out_proc_handle: *mut KernelHandle<ProcessDispatcher>,
        out_proc_rights: *mut zx_rights_t,
        out_restricted_vmar_handle: *mut KernelHandle<
            super::vm_address_region_dispatcher::VmAddressRegionDispatcher,
        >,
        out_restricted_vmar_rights: *mut zx_rights_t,
    ) -> zx_status_t;

    /// Exits the current process with the given return code.
    ///
    /// # Safety
    ///
    /// Must only be called within a valid running thread context. Terminates current process execution.
    pub(crate) fn cpp_process_dispatcher_exit_current(retcode: i64) -> !;

    /// Returns the address space of `process` at the given virtual address.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_aspace_at(
        process: *mut ProcessDispatcher,
        va: usize,
    ) -> *mut crate::vm::vm_aspace::VmAspace;

    /// Returns the job of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_job(process: *mut ProcessDispatcher)
    -> *mut JobDispatcher;
}
