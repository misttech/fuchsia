// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::handle_table::HandleTable;
use super::job_dispatcher::JobDispatcher;
use super::process_dispatcher::{ProcessDispatcher, ProcessState, ShareableProcessState};
use super::thread_dispatcher::ThreadDispatcher;
use crate::arch_rs::UserEntryState;
use core::mem::MaybeUninit;
use zx_types::{
    zx_info_process_t, zx_info_task_runtime_t, zx_info_task_stats_t, zx_koid_t, zx_rights_t,
    zx_status_t,
};

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

    /// Returns a pointer to the handle table of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    #[allow(improper_ctypes)]
    pub(crate) fn cpp_process_dispatcher_handle_table(
        process: *const ProcessDispatcher,
    ) -> *const HandleTable;

    /// Returns a pointer to the shareable process state of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(super) fn cpp_process_dispatcher_shareable_state(
        process: *const ProcessDispatcher,
    ) -> *const ShareableProcessState;

    /// Returns a pointer to the address space of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    pub(super) fn cpp_shareable_process_state_aspace(
        state: *const ShareableProcessState,
    ) -> *const crate::vm::vm_aspace::VmAspace;

    /// Enforces basic policy for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_enforce_basic_policy(
        process: *mut ProcessDispatcher,
        policy: u32,
    ) -> zx_status_t;

    /// Returns the timer slack policy for the given process.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and `out_slack` must point to valid
    /// memory.
    pub(crate) fn cpp_process_dispatcher_get_timer_slack_policy(
        process: *const ProcessDispatcher,
        out_slack: *mut crate::kernel::deadline::TimerSlack,
    );

    /// Retrieves process info from C++.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_info(
        process: *const ProcessDispatcher,
    ) -> zx_info_process_t;

    /// Returns the current `ProcessDispatcher::State` of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(super) fn cpp_process_dispatcher_state(process: *const ProcessDispatcher) -> ProcessState;

    /// Retrieves task memory statistics for `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and `out_stats` must point to valid
    /// writable memory.
    pub(crate) fn cpp_process_dispatcher_get_stats(
        process: *const ProcessDispatcher,
        out_stats: *mut zx_info_task_stats_t,
    ) -> zx_status_t;

    /// Retrieves task runtime statistics for `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_runtime_stats(
        process: *const ProcessDispatcher,
    ) -> zx_info_task_runtime_t;

    /// Retrieves the KOIDs of all threads in `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and `out_threads` and `out_count` must
    /// point to valid writable memory.
    pub(crate) fn cpp_process_dispatcher_get_threads(
        process: *const ProcessDispatcher,
        out_threads: *mut *mut zx_koid_t,
        out_count: *mut usize,
    ) -> zx_status_t;

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
        out_proc_handle: *mut MaybeUninit<KernelHandle<ProcessDispatcher>>,
        out_proc_rights: *mut MaybeUninit<zx_rights_t>,
        out_vmar_handle: *mut MaybeUninit<
            KernelHandle<super::vm_address_region_dispatcher::VmAddressRegionDispatcher>,
        >,
        out_vmar_rights: *mut MaybeUninit<zx_rights_t>,
    ) -> zx_status_t;

    /// Creates a new shared process that shares state with `shared_proc`.
    ///
    /// # Safety
    ///
    /// `shared_proc` must point to a valid `ProcessDispatcher` whose reference count was
    /// transferred.
    /// `name_ptr` must point to `name_len` readable bytes.
    /// Output handle and rights pointers must point to writable memory.
    pub(crate) fn cpp_process_dispatcher_create_shared(
        shared_proc: *mut ProcessDispatcher,
        name_ptr: *const core::ffi::c_char,
        name_len: usize,
        flags: u32,
        out_proc_handle: *mut MaybeUninit<KernelHandle<ProcessDispatcher>>,
        out_proc_rights: *mut MaybeUninit<zx_rights_t>,
        out_restricted_vmar_handle: *mut MaybeUninit<
            KernelHandle<super::vm_address_region_dispatcher::VmAddressRegionDispatcher>,
        >,
        out_restricted_vmar_rights: *mut MaybeUninit<zx_rights_t>,
    ) -> zx_status_t;

    /// Exits the current process with the given return code.
    ///
    /// # Safety
    ///
    /// Must only be called within a valid running thread context. Terminates current process
    /// execution.
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

    /// Returns the normal address space of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(super) fn cpp_process_dispatcher_normal_aspace(
        process: *mut ProcessDispatcher,
    ) -> *mut crate::vm::vm_aspace::VmAspace;

    /// Returns the restricted address space of `process`, or null if none.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(super) fn cpp_process_dispatcher_restricted_aspace(
        process: *const ProcessDispatcher,
    ) -> *mut crate::vm::vm_aspace::VmAspace;

    /// Returns the job of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_job(process: *mut ProcessDispatcher)
    -> *mut JobDispatcher;

    /// Returns the debug address of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_debug_addr(process: *const ProcessDispatcher)
    -> usize;

    /// Sets the debug address of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_set_debug_addr(
        process: *mut ProcessDispatcher,
        addr: usize,
    ) -> zx_status_t;

    /// Returns the dynamic break-on-load address of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_get_dyn_break_on_load(
        process: *const ProcessDispatcher,
    ) -> usize;

    /// Sets the dynamic break-on-load address of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_set_dyn_break_on_load(
        process: *mut ProcessDispatcher,
        break_on_load: usize,
    ) -> zx_status_t;

    /// Returns the hardware trace context ID of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cpp_process_dispatcher_hw_trace_context_id(
        process: *const ProcessDispatcher,
    ) -> usize;

    /// Returns a pointer to the `FutexContext` of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_futex_context(
        process: *const ProcessDispatcher,
    ) -> *mut core::ffi::c_void;

    /// Makes `thread` runnable and adds it to the C++ thread list of `process`.
    ///
    /// Ownership of `thread` within the process thread list stays on the C++ side; this only
    /// hands the thread over to `ProcessDispatcher::AddInitializedThread`, which takes the
    /// process lock itself.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`, `thread` to a valid initialized
    /// `ThreadDispatcher` that is not already in a thread list, and `entry` to a valid
    /// `UserEntryState`.
    pub(crate) fn cpp_process_add_initialized_thread(
        process: *const ProcessDispatcher,
        thread: *const ThreadDispatcher,
        ensure_initial_thread: bool,
        entry: *const UserEntryState,
    ) -> zx_status_t;

    /// Removes `thread` from the C++ thread list of `process`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` and `thread` must point to a valid
    /// `ThreadDispatcher` that is in the thread list of `process`.
    pub(crate) fn cpp_process_remove_thread(
        process: *mut ProcessDispatcher,
        thread: *mut ThreadDispatcher,
    );

    /// Returns a pointer to the process's normal `Exceptionate`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_exceptionate(
        process: *mut ProcessDispatcher,
    ) -> *mut super::exceptionate::Exceptionate;

    /// Returns a pointer to the process's debug `Exceptionate`.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_debug_exceptionate(
        process: *mut ProcessDispatcher,
    ) -> *mut super::exceptionate::Exceptionate;

    /// Looks up a thread in `process` by `koid`, returning an exported `RefPtr<ThreadDispatcher>`
    /// raw pointer (or null if not found).
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_lookup_thread_by_id(
        process: *mut ProcessDispatcher,
        koid: zx_koid_t,
    ) -> *mut ThreadDispatcher;

    /// Returns whether `process` is marked critical to the root job.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher`.
    pub(crate) fn cpp_process_dispatcher_critical_to_root_job(
        process: *const ProcessDispatcher,
    ) -> bool;
}
