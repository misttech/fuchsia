// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::DispatcherOps;
use super::handle::{HandleOwner, HandleValue, KernelHandle};
use super::handle_table::HandleTable;
use super::job_dispatcher::JobDispatcher;
#[cfg(target_arch = "x86_64")]
use super::process_dispatcher_ffi::cpp_process_dispatcher_hw_trace_context_id;
use super::process_dispatcher_ffi::{
    cpp_process_add_initialized_thread, cpp_process_attach_aspace_to_thread,
    cpp_process_dispatcher_aspace_at, cpp_process_dispatcher_create,
    cpp_process_dispatcher_create_shared, cpp_process_dispatcher_current,
    cpp_process_dispatcher_enforce_basic_policy, cpp_process_dispatcher_exit_current,
    cpp_process_dispatcher_futex_context, cpp_process_dispatcher_get_debug_addr,
    cpp_process_dispatcher_get_dyn_break_on_load, cpp_process_dispatcher_get_info,
    cpp_process_dispatcher_get_timer_slack_policy,
    cpp_process_dispatcher_get_timer_slack_policy_amount, cpp_process_dispatcher_handle_table,
    cpp_process_dispatcher_is_current, cpp_process_dispatcher_job, cpp_process_dispatcher_kill,
    cpp_process_dispatcher_make_and_add_handle,
    cpp_process_dispatcher_make_and_add_handle_from_ref, cpp_process_dispatcher_resume,
    cpp_process_dispatcher_set_critical_to_job, cpp_process_dispatcher_set_debug_addr,
    cpp_process_dispatcher_set_dyn_break_on_load, cpp_process_dispatcher_start,
    cpp_process_dispatcher_suspend, cpp_process_dispatcher_vdso_base_address,
    cpp_process_get_job_koid, cpp_process_remove_thread,
};
use super::thread_dispatcher::ThreadDispatcher;
use super::vm_address_region_dispatcher::VmAddressRegionDispatcher;
use crate::arch_rs::UserEntryState;
use crate::kernel::thread::ThreadPtr;
use crate::vm::vm_aspace::VmAspace;
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{zx_info_process_t, zx_rights_t, zx_vaddr_t};

crate::object::dispatcher::impl_dispatcher_facade!(
    pub struct ProcessDispatcher,
    zx_types::ZX_OBJ_TYPE_PROCESS
);

/// A wrapper around a raw pointer to the current [`ProcessDispatcher`].
///
/// This type explicitly does not implement [`Send`] or [`Sync`], guaranteeing that it cannot be
/// shared with or sent to other threads. Because the current thread must be part of this process,
/// the process cannot be destroyed while this thread is executing, making it safe to dereference
/// the raw pointer into a [`ProcessDispatcher`] reference.
#[derive(Debug)]
pub struct CurrentProcessDispatcher {
    ptr: *const ProcessDispatcher,
}

impl core::ops::Deref for CurrentProcessDispatcher {
    type Target = ProcessDispatcher;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `CurrentProcessDispatcher` does not implement `Send` or `Sync`, so it cannot be
        // shared with other threads. The only way `self.ptr` could become invalid is if the
        // process has been destroyed, which cannot happen while this thread (which is part of the
        // process) is executing.
        unsafe { &*self.ptr }
    }
}

impl ProcessDispatcher {
    /// Returns a wrapper that dereferences to the current [`ProcessDispatcher`].
    #[inline]
    pub fn get_current() -> CurrentProcessDispatcher {
        // SAFETY: Calling `cpp_process_dispatcher_current` is safe when executing in a valid
        // thread context.
        let ptr = unsafe { cpp_process_dispatcher_current() };
        CurrentProcessDispatcher { ptr }
    }

    /// Executes the given function with a reference to the current process.
    #[inline]
    pub fn with_current<R>(f: impl FnOnce(&ProcessDispatcher) -> R) -> R {
        f(&Self::get_current())
    }

    /// Returns whether this `ProcessDispatcher` is the current process.
    pub fn is_current(&self) -> bool {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_is_current(self.as_ffi()) }
    }

    /// Returns a reference to this process's [`HandleTable`].
    #[inline]
    pub fn handle_table(&self) -> &HandleTable {
        // SAFETY: `self` is a valid `ProcessDispatcher`, and its handle table is valid for the
        // lifetime of `self`.
        unsafe { &*cpp_process_dispatcher_handle_table(self.as_ffi()) }
    }

    /// Starts execution of this process.
    pub fn start(
        &self,
        thread: fbl::RefPtr<ThreadDispatcher>,
        pc: zx_vaddr_t,
        sp: zx_vaddr_t,
        arg_handle: Option<HandleOwner>,
        arg2: usize,
    ) -> Result<(), Status> {
        let arg_handle_ptr = match arg_handle {
            Some(h) => h.release(),
            None => core::ptr::null_mut(),
        };
        let raw_thread = fbl::RefPtr::into_raw(thread) as *mut _;
        // SAFETY: `self` is a valid reference, `raw_thread` transfers an acquired refcount,
        // and `arg_handle_ptr` ownership is transferred to C++.
        let status = unsafe {
            cpp_process_dispatcher_start(
                self.as_ffi_mut(),
                raw_thread,
                pc,
                sp,
                arg_handle_ptr,
                arg2,
            )
        };
        Status::ok(status)
    }

    /// Kills this process with the given return code.
    pub fn kill(&self, retcode: i64) {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_kill(self.as_ffi_mut(), retcode) }
    }

    /// Suspends execution of this process.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_STATE` if the process is dying or dead.
    pub fn suspend(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let status = unsafe { cpp_process_dispatcher_suspend(self.as_ffi_mut()) };
        Status::ok(status)
    }

    /// Resumes execution of this process.
    pub fn resume(&self) {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_resume(self.as_ffi_mut()) }
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
                self.as_ffi_mut(),
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
            cpp_process_dispatcher_make_and_add_handle_from_ref(
                self.as_ffi_mut(),
                raw_dispatcher as *mut _,
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
            unsafe { cpp_process_dispatcher_enforce_basic_policy(self.as_ffi_mut(), policy) };
        Status::ok(status)
    }

    /// Returns the timer slack policy amount for this process.
    pub fn get_timer_slack_policy_amount(&self) -> i64 {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_get_timer_slack_policy_amount(self.as_ffi()) }
    }

    /// Returns the timer slack policy for this process.
    #[inline]
    pub fn get_timer_slack_policy(&self) -> crate::kernel::deadline::TimerSlack {
        let mut slack = crate::kernel::deadline::TimerSlack::none();
        // SAFETY: `self` is a valid `ProcessDispatcher` reference and `slack` points to valid memory.
        unsafe {
            cpp_process_dispatcher_get_timer_slack_policy(self.as_ffi(), &mut slack);
        }
        slack
    }

    /// Returns information about this process.
    pub fn get_info(&self) -> zx_info_process_t {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_get_info(self.as_ffi()) }
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
                self.as_ffi_mut(),
                fbl::RefPtr::into_raw(job) as *mut _,
                retcode_nonzero,
            )
        };
        Status::ok(status)
    }

    /// Returns the default rights for a process handle.
    pub const fn default_rights() -> zx_rights_t {
        zx_types::ZX_DEFAULT_PROCESS_RIGHTS
    }

    /// Creates a new `ProcessDispatcher`.
    pub fn create(
        job: fbl::RefPtr<JobDispatcher>,
        name: &[u8],
        flags: u32,
    ) -> Result<
        (KernelHandle<Self>, zx_rights_t, KernelHandle<VmAddressRegionDispatcher>, zx_rights_t),
        Status,
    > {
        let mut proc_handle = MaybeUninit::uninit();
        let mut proc_rights = MaybeUninit::<zx_rights_t>::uninit();
        let mut vmar_handle = MaybeUninit::uninit();
        let mut vmar_rights = MaybeUninit::<zx_rights_t>::uninit();

        // SAFETY: `job` is a valid `RefPtr<JobDispatcher>` whose refcount is transferred to C++,
        // `name` points to readable memory of length `name.len()`, and output pointers point to valid uninitialized storage.
        let status = unsafe {
            cpp_process_dispatcher_create(
                fbl::RefPtr::into_raw(job) as *mut _,
                name.as_ptr().cast(),
                name.len(),
                flags,
                &raw mut proc_handle,
                &raw mut proc_rights,
                &raw mut vmar_handle,
                &raw mut vmar_rights,
            )
        };
        Status::ok(status)?;
        // SAFETY: Initialized by C++ on success.
        unsafe {
            Ok((
                proc_handle.assume_init(),
                proc_rights.assume_init(),
                vmar_handle.assume_init(),
                vmar_rights.assume_init(),
            ))
        }
    }

    /// Creates a new `ProcessDispatcher` that shares state with `shared_proc`.
    pub fn create_shared(
        shared_proc: fbl::RefPtr<Self>,
        name: &[u8],
        flags: u32,
    ) -> Result<
        (KernelHandle<Self>, zx_rights_t, KernelHandle<VmAddressRegionDispatcher>, zx_rights_t),
        Status,
    > {
        let mut proc_handle = MaybeUninit::uninit();
        let mut proc_rights = MaybeUninit::<zx_rights_t>::uninit();
        let mut vmar_handle = MaybeUninit::uninit();
        let mut vmar_rights = MaybeUninit::<zx_rights_t>::uninit();

        // SAFETY: `shared_proc` is a valid `RefPtr<ProcessDispatcher>` whose refcount is transferred to C++.
        let status = unsafe {
            cpp_process_dispatcher_create_shared(
                fbl::RefPtr::into_raw(shared_proc) as *mut _,
                name.as_ptr().cast(),
                name.len(),
                flags,
                &raw mut proc_handle,
                &raw mut proc_rights,
                &raw mut vmar_handle,
                &raw mut vmar_rights,
            )
        };
        Status::ok(status)?;
        // SAFETY: Initialized by C++ on success.
        unsafe {
            Ok((
                proc_handle.assume_init(),
                proc_rights.assume_init(),
                vmar_handle.assume_init(),
                vmar_rights.assume_init(),
            ))
        }
    }

    /// Exits the current process with the given return code.
    pub fn exit_current(retcode: i64) -> ! {
        // SAFETY: Terminating current process execution within valid thread context.
        unsafe { cpp_process_dispatcher_exit_current(retcode) }
    }

    /// Returns a reference to this process's address space at the given virtual address.
    pub fn aspace_at(&self, va: usize) -> Option<&VmAspace> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let aspace_ptr = unsafe { cpp_process_dispatcher_aspace_at(self.as_ffi_mut(), va) };
        // SAFETY: `aspace_ptr` is either null or points to a valid `VmAspace` managed by the process.
        unsafe { aspace_ptr.as_ref() }
    }

    /// Returns the normal address space for this process.
    ///
    /// All processes have a normal address space.  The normal aspace is the
    /// address space that's active when a thread is in normal mode.
    ///
    /// For "shared processes", on architectures that support unified aspaces, the normal aspace
    /// is a unified aspace. A unified aspace is an aspace that spans both the shared and restricted
    /// aspace, and is used by threads in normal mode to avoid having to switch between the shared
    /// and restricted aspaces.
    ///
    /// On architectures that don't yet support unified aspaces, the normal
    /// aspace is a shared aspace (`ShareableProcessState::aspace()`).
    ///
    /// For non-shared processes (regular ones), the normal aspace is the one and only aspace
    /// belonging to the process (`ShareableProcessState::aspace()`).
    ///
    /// TODO(https://fxbug.dev/42083004): Update this comment once all architectures support unified
    /// aspaces.
    pub fn normal_aspace(&self) -> &VmAspace {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let raw: *mut process_dispatcher_bindings::VmAspace = unsafe {
            process_dispatcher_bindings::cpp_process_dispatcher_normal_aspace(
                self.as_ffi_mut().cast(),
            )
        };
        // SAFETY: The normal address space is valid for the lifetime of `self`.
        unsafe { raw.cast::<VmAspace>().as_ref_unchecked() }
    }

    /// Returns the "restricted" address space for a process, or nullptr if it does not have a
    /// restricted address space.
    ///
    /// The restricted address space spans the bottom half of the process' total address space, and
    /// is private to the process. Threads executing in restricted mode are restricted to this
    /// address space.
    pub fn restricted_aspace(&self) -> Option<&VmAspace> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let raw: *mut process_dispatcher_bindings::VmAspace = unsafe {
            process_dispatcher_bindings::cpp_process_dispatcher_restricted_aspace(
                self.as_ffi_mut().cast(),
            )
        };
        // SAFETY: The restricted address space is valid for the lifetime of `self`.
        unsafe { raw.cast::<VmAspace>().as_ref() }
    }

    /// Returns the job associated with this process.
    pub fn job(&self) -> Option<fbl::RefPtr<JobDispatcher>> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let ptr = unsafe { cpp_process_dispatcher_job(self.as_ffi_mut()) };
        // SAFETY: `ptr` is exported via `fbl::ExportToRawPtr` with an acquired refcount.
        unsafe { fbl::RefPtr::try_from_raw(ptr) }
    }

    /// Returns the debug address of the dynamic loader (`_dl_debug_addr`) for this process.
    pub fn get_debug_addr(&self) -> usize {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_get_debug_addr(self.as_ffi()) }
    }

    /// Sets the debug address of the dynamic loader (`_dl_debug_addr`) for this process.
    pub fn set_debug_addr(&self, addr: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let status = unsafe { cpp_process_dispatcher_set_debug_addr(self.as_ffi_mut(), addr) };
        Status::ok(status)
    }

    /// Returns the dynamic break-on-load state for this process.
    pub fn get_dyn_break_on_load(&self) -> usize {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_get_dyn_break_on_load(self.as_ffi()) }
    }

    /// Sets the dynamic break-on-load state for this process.
    pub fn set_dyn_break_on_load(&self, break_on_load: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        let status = unsafe {
            cpp_process_dispatcher_set_dyn_break_on_load(self.as_ffi_mut(), break_on_load)
        };
        Status::ok(status)
    }

    /// Returns the base address of the vDSO mapping for this process.
    pub fn vdso_base_address(&self) -> usize {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_vdso_base_address(self.as_ffi()) }
    }

    /// Returns the hardware trace context ID for this process.
    #[cfg(target_arch = "x86_64")]
    pub fn hw_trace_context_id(&self) -> usize {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference.
        unsafe { cpp_process_dispatcher_hw_trace_context_id(self.as_ffi()) }
    }

    /// Returns a `*const ProcessDispatcher` pointer suitable for passing to FFI routines.
    #[inline]
    pub fn as_ffi(&self) -> *const Self {
        self as *const Self
    }

    /// Returns a `*mut ProcessDispatcher` pointer suitable for passing to FFI routines that
    /// take a non-const `ProcessDispatcher*`.
    #[inline]
    pub fn as_ffi_mut(&self) -> *mut Self {
        self.as_ffi() as *mut Self
    }

    /// Returns a reference to the [`FutexContext`](super::futex_context::FutexContext) for this
    /// process.
    #[inline]
    pub fn futex_context(&self) -> &super::futex_context::FutexContext {
        // SAFETY: `self` is a valid `ProcessDispatcher` reference, and
        // `cpp_process_dispatcher_futex_context` returns a pointer to the initialized
        // `FutexContext` storage owned by `shareable_state_`.
        unsafe { &*cpp_process_dispatcher_futex_context(self.as_ffi()).cast() }
    }

    /// Grows the futex state pool for this process.
    pub fn futex_context_grow_pool(&self) -> Result<(), Status> {
        self.futex_context().grow_futex_state_pool()
    }

    /// Shrinks the futex state pool for this process.
    pub fn futex_context_shrink_pool(&self) -> Result<(), Status> {
        self.futex_context().shrink_futex_state_pool();
        Ok(())
    }

    /// Attaches the normal address space of this process to `core_thread`.
    pub fn attach_normal_aspace_to_thread(&self, core_thread: ThreadPtr) -> Result<(), Status> {
        // SAFETY: `self` is a valid `ProcessDispatcher`, and `core_thread` upholds the
        // `ThreadPtr` invariant that it points to a live kernel thread.
        let status =
            unsafe { cpp_process_attach_aspace_to_thread(self.as_ffi(), core_thread.as_raw()) };
        Status::ok(status)
    }

    /// Returns the parent job koid.
    pub fn job_koid(&self) -> zx_types::zx_koid_t {
        // SAFETY: `self` is a valid `ProcessDispatcher`.
        unsafe { cpp_process_get_job_koid(self.as_ffi()) }
    }

    /// Transitions `thread` from the initialized state to a runnable state and adds it to the
    /// thread list of this process.
    ///
    /// If `ensure_initial_thread` is true, this fails unless `thread` is the initial thread in
    /// the process.
    pub fn add_initialized_thread(
        &self,
        thread: &ThreadDispatcher,
        ensure_initial_thread: bool,
        entry: &UserEntryState,
    ) -> Result<(), Status> {
        // SAFETY: `self` and `thread` are valid references.
        let status = unsafe {
            cpp_process_add_initialized_thread(
                self.as_ffi(),
                thread as *const _,
                ensure_initial_thread,
                entry,
            )
        };
        Status::ok(status)
    }

    /// Removes `thread` from the thread list of this process.
    pub fn remove_thread(&self, thread: &ThreadDispatcher) {
        // SAFETY: `self` and `thread` are valid references.
        unsafe { cpp_process_remove_thread(self.as_ffi(), thread as *const _) }
    }

    /// Returns the process's normal `Exceptionate`.
    pub fn exceptionate(&self) -> &super::exceptionate::Exceptionate {
        // SAFETY: `self` is a valid `ProcessDispatcher`, which owns `exceptionate_` for its entire
        // lifetime.
        unsafe {
            &*super::process_dispatcher_ffi::cpp_process_dispatcher_exceptionate(self.as_ffi_mut())
        }
    }

    /// Returns the process's debug `Exceptionate`.
    pub fn debug_exceptionate(&self) -> &super::exceptionate::Exceptionate {
        // SAFETY: `self` is a valid `ProcessDispatcher`, which owns `debug_exceptionate_` for its
        // entire lifetime.
        unsafe {
            &*super::process_dispatcher_ffi::cpp_process_dispatcher_debug_exceptionate(
                self.as_ffi_mut(),
            )
        }
    }
}

zr::static_assert!(core::mem::size_of::<ProcessDispatcher>() == 0);
