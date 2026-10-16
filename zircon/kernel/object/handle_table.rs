// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::{Dispatcher, DispatcherOps};
use super::handle::{HandleOwner, HandleRef, HandleValue};
use super::handle_table_ffi::{
    cpp_handle_table_add_handle_locked, cpp_handle_table_for_each_handle_batched,
    cpp_handle_table_for_each_handle_locked, cpp_handle_table_get_handle_locked,
    cpp_handle_table_handle_count, cpp_handle_table_handle_count_locked, cpp_handle_table_koid,
    cpp_handle_table_lock, cpp_handle_table_map_handle_to_value,
    cpp_handle_table_remove_handle_locked,
};
use super::process_dispatcher::ProcessDispatcher;
use crate::kernel::thread::AutoExpiringPreemptDisabler;
use core::convert::Infallible;
use core::ffi::c_void;
use core::fmt::{self, Debug, Formatter};
use core::mem::size_of;
use core::ptr::{self, NonNull};
use fbl::{Array, HasRefCount, Recyclable, RefPtr};
use ksync::{BrwLockPi, BrwLockPiReadGuard, BrwLockPiWriteGuard, LockClass, LockToken};
use pin_init::PinInit;
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_OBJ_TYPE_NONE, ZX_RIGHT_NONE, zx_handle_t, zx_info_handle_extended_t,
    zx_koid_t, zx_rights_t, zx_status_t,
};

/// Lock class tag for the handle table's reader-writer lock.
#[derive(Debug, Default, Copy, Clone)]
pub struct HandleTableLockClass;

impl LockClass for HandleTableLockClass {
    const ID: *mut c_void = ptr::null_mut();
}

/// Facade for the C++ `HandleTable` class.
#[repr(C)]
pub struct HandleTable {
    _facade: OpaqueFacade,
}

zr::static_assert!(size_of::<HandleTable>() == 0);

impl Debug for HandleTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandleTable").field("koid", &self.get_koid()).finish()
    }
}

/// A mutable entry for a handle in a [`HandleTable`] while holding the handle table's write lock.
///
/// Holding this entry exclusively borrows the write lock token for `'a`, preventing any other
/// [`HandleRef`] or table mutation from coexisting until the entry is dropped or consumed by
/// [`HandleEntry::remove`].
pub struct HandleEntry<'a> {
    table: &'a HandleTable,
    ptr: NonNull<c_void>,
    _marker: core::marker::PhantomData<&'a mut LockToken<'a, HandleTableLockClass>>,
}

impl<'a> HandleEntry<'a> {
    /// Borrows this entry as a [`HandleRef`] tied to the borrow of `self`.
    #[inline]
    pub fn as_ref(&self) -> HandleRef<'_> {
        // SAFETY: `self.ptr` is a valid handle pointer in `self.table` and the handle table write
        // lock is held for the lifetime of `&self`.
        unsafe { HandleRef::from_raw(self.ptr) }
    }

    /// Removes the handle from the handle table and returns the owned handle.
    #[inline]
    pub fn remove(self) -> HandleOwner {
        // SAFETY: `self.table` is valid, the handle table write lock is exclusively held via
        // `self`, and `self.ptr` is a valid handle in `self.table` looked up under the same lock.
        let raw = unsafe {
            cpp_handle_table_remove_handle_locked(self.table.as_ffi_mut(), self.ptr.as_ptr())
        };
        // SAFETY: `cpp_handle_table_remove_handle_locked` returns the non-null owned handle pointer
        // corresponding to `self.ptr`.
        unsafe { HandleOwner::from_raw_non_null(NonNull::new_unchecked(raw)) }
    }
}

impl HandleTable {
    /// Returns a `*const HandleTable` pointer suitable for passing to FFI routines.
    #[inline]
    pub fn as_ffi(&self) -> *const Self {
        self as *const Self
    }

    /// Returns a `*mut HandleTable` pointer suitable for passing to FFI routines that
    /// take a non-const `HandleTable*`.
    #[inline]
    pub fn as_ffi_mut(&self) -> *mut Self {
        self.as_ffi() as *mut Self
    }

    /// Returns a reference to the handle table's priority-inheriting reader-writer lock.
    #[inline]
    fn lock(&self) -> &BrwLockPi<HandleTableLockClass> {
        // SAFETY: `self` is a valid `HandleTable`, and its lock is a valid `BrwLockPi`.
        unsafe {
            let lock_ptr = cpp_handle_table_lock(self.as_ffi());
            &*(lock_ptr as *const BrwLockPi<HandleTableLockClass>)
        }
    }

    /// Acquires a reader lock on this handle table.
    #[inline]
    pub fn read_lock(
        &self,
    ) -> impl PinInit<BrwLockPiReadGuard<'_, HandleTableLockClass>, Infallible> {
        self.lock().read_lock()
    }

    /// Acquires a writer lock on this handle table.
    #[inline]
    pub fn write_lock(
        &self,
    ) -> impl PinInit<BrwLockPiWriteGuard<'_, HandleTableLockClass>, Infallible> {
        self.lock().write_lock()
    }

    /// Returns the KOID of this handle table.
    #[inline]
    pub fn get_koid(&self) -> zx_koid_t {
        // SAFETY: `self` is a valid `HandleTable` reference.
        unsafe { cpp_handle_table_koid(self.as_ffi()) }
    }

    /// Returns the number of outstanding handles in this handle table.
    #[inline]
    pub fn handle_count(&self) -> u32 {
        // SAFETY: `self` is a valid `HandleTable` reference.
        unsafe { cpp_handle_table_handle_count(self.as_ffi()) }
    }

    /// Returns the number of outstanding handles in this handle table while holding its lock.
    #[inline]
    pub fn handle_count_locked(&self, _token: &LockToken<'_, HandleTableLockClass>) -> u32 {
        // SAFETY: `self` is a valid `HandleTable` reference and the handle table lock is held.
        unsafe { cpp_handle_table_handle_count_locked(self.as_ffi()) }
    }

    /// Maps a handle to its user-visible `HandleValue` for this handle table.
    #[inline]
    pub fn map_handle_to_value(&self, handle: HandleRef<'_>) -> HandleValue {
        // SAFETY: `self` is a valid `HandleTable` and `handle` is a valid `HandleRef`.
        let raw = unsafe { cpp_handle_table_map_handle_to_value(self.as_ffi(), handle.as_ptr()) };
        HandleValue::new(raw)
    }

    /// Retrieves a handle reference while holding the handle table lock.
    #[inline]
    pub fn get_handle_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, HandleTableLockClass>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'a>> {
        self.get_handle_locked_opt(token, Some(caller), handle_value)
    }

    /// Same as [`Self::get_handle_locked`], except `caller` can be `None` to skip
    /// `ZX_POL_BAD_HANDLE` policy enforcement.
    #[inline]
    fn get_handle_locked_opt<'a>(
        &'a self,
        _token: &'a LockToken<'_, HandleTableLockClass>,
        caller: Option<&ProcessDispatcher>,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'a>> {
        let caller_ptr = caller.map_or(ptr::null_mut(), ProcessDispatcher::as_ffi_mut);
        // SAFETY: `self` is valid, `caller_ptr` is null or a valid `ProcessDispatcher`, and the
        // handle table lock is held for `'a`.
        let ptr = unsafe {
            cpp_handle_table_get_handle_locked(
                self.as_ffi_mut(),
                caller_ptr,
                handle_value.raw_value(),
            )
        };
        // SAFETY: `ptr` is a valid handle pointer in `self` while the lock is held.
        NonNull::new(ptr).map(|ptr| unsafe { HandleRef::from_raw(ptr) })
    }

    /// Retrieves a mutable handle entry while holding the handle table write lock.
    #[inline]
    pub fn get_handle_entry_locked<'a>(
        &'a self,
        token: &'a mut LockToken<'_, HandleTableLockClass>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleEntry<'a>> {
        let ptr = self.get_handle_locked(token, caller, handle_value)?.as_non_null();
        Some(HandleEntry { table: self, ptr, _marker: core::marker::PhantomData })
    }

    /// Adds an owned handle to the handle table while holding the write lock.
    #[inline]
    pub fn add_handle_locked(
        &self,
        _token: &mut LockToken<'_, HandleTableLockClass>,
        handle: HandleOwner,
    ) {
        // SAFETY: `self` is valid, the handle table write lock is held, and `handle` ownership is
        // transferred to C++.
        unsafe {
            cpp_handle_table_add_handle_locked(self.as_ffi_mut(), handle.release());
        }
    }

    /// Removes a handle by value from the handle table while holding the write lock.
    #[inline]
    pub fn remove_handle_locked(
        &self,
        token: &mut LockToken<'_, HandleTableLockClass>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleOwner> {
        self.get_handle_entry_locked(token, caller, handle_value).map(HandleEntry::remove)
    }

    /// Adds an owned handle to this handle table.
    pub fn add_handle(&self, handle: HandleOwner) {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = self.write_lock());
        self.add_handle_locked(guard.as_mut().token_mut(), handle);
    }

    /// Removes a handle from this handle table and returns the owned handle if found.
    pub fn remove_handle(
        &self,
        caller: &ProcessDispatcher,
        handle: HandleValue,
    ) -> Option<HandleOwner> {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = self.write_lock());
        self.remove_handle_locked(guard.as_mut().token_mut(), caller, handle)
    }

    /// Removes a slice of raw handle values from this handle table.
    ///
    /// Matching C++ `HandleTable::RemoveHandles(ProcessDispatcher&, ktl::span<const zx_handle_t>)`,
    /// `ZX_HANDLE_INVALID` entries are skipped, and if any non-zero handle is missing, returns
    /// `ZX_ERR_BAD_HANDLE` after processing the remaining handles.
    #[inline]
    pub fn remove_handles(
        &self,
        caller: &ProcessDispatcher,
        handles: &[zx_handle_t],
    ) -> Result<(), Status> {
        let mut status = Ok(());
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = self.write_lock());
        for &handle in handles {
            if handle != ZX_HANDLE_INVALID
                && self
                    .remove_handle_locked(
                        guard.as_mut().token_mut(),
                        caller,
                        HandleValue::new(handle),
                    )
                    .is_none()
            {
                status = Err(Status::BAD_HANDLE);
            }
        }
        status
    }

    /// Get the dispatcher corresponding to this handle value, after checking that this handle has
    /// the desired rights.
    /// `ZX_ERR_WRONG_TYPE` is returned before `ZX_ERR_ACCESS_DENIED`, because if the wrong handle
    /// was passed, evaluating its rights does not have much meaning and also this aids in
    /// debugging.
    /// If successful, returns the dispatcher and the rights the handle currently has.
    /// If `caller` is `None`, `ZX_POL_BAD_HANDLE` will not be enforced.
    #[inline]
    fn get_dispatcher_with_rights_impl<T>(
        &self,
        caller: Option<&ProcessDispatcher>,
        handle_value: HandleValue,
        desired_rights: zx_rights_t,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        let (has_desired_rights, rights, generic_dispatcher) = {
            // Scope utilized to reduce lock duration.
            ksync::lock!(let guard = self.read_lock());
            let handle = self
                .get_handle_locked_opt(guard.token(), caller, handle_value)
                .ok_or(Status::BAD_HANDLE)?;
            (handle.has_rights(desired_rights), handle.rights(), handle.dispatcher())
        };

        // Wrong type takes precedence over access denied.
        if T::TYPE != ZX_OBJ_TYPE_NONE && generic_dispatcher.get_type() != T::TYPE {
            return Err(Status::WRONG_TYPE);
        }

        if !has_desired_rights {
            return Err(Status::ACCESS_DENIED);
        }

        // SAFETY: We verified the type of the dispatcher matches `T::TYPE`, so it is safe to cast.
        Ok((unsafe { generic_dispatcher.cast::<T>() }, rights))
    }

    /// Resolves a handle to a dispatcher of type `T` without requiring any rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    #[inline]
    pub fn get_dispatcher<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_with_rights::<T>(caller, handle_value, ZX_RIGHT_NONE)
    }

    /// Resolves a handle to a dispatcher of type `T` and returns its associated rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    #[inline]
    pub fn get_dispatcher_and_rights<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_with_rights_impl::<T>(Some(caller), handle_value, ZX_RIGHT_NONE)
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` and returns its
    /// actual rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    /// - `ZX_ERR_ACCESS_DENIED` if `handle_value` lacks the requested `rights`.
    #[inline]
    pub fn get_dispatcher_with_rights_and_actual<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_with_rights_impl::<T>(Some(caller), handle_value, rights)
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` in this handle
    /// table.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    /// - `ZX_ERR_ACCESS_DENIED` if `handle_value` lacks the requested `rights`.
    #[inline]
    pub fn get_dispatcher_with_rights<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        let (dispatcher, _actual_rights) =
            self.get_dispatcher_with_rights_impl::<T>(Some(caller), handle_value, rights)?;
        Ok(dispatcher)
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` in this handle
    /// table, without enforcing the `ZX_POL_BAD_HANDLE` job policy on lookup failure.
    ///
    /// Mirrors C++ `HandleTable::GetDispatcherWithRightsNoPolicyCheck`. As with
    /// [`Self::get_dispatcher_with_rights`], `ZX_ERR_WRONG_TYPE` takes precedence over
    /// `ZX_ERR_ACCESS_DENIED`.
    #[inline]
    pub fn get_dispatcher_with_rights_no_policy_check<T>(
        &self,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        let (dispatcher, _actual_rights) =
            self.get_dispatcher_with_rights_impl::<T>(None, handle_value, rights)?;
        Ok(dispatcher)
    }

    /// Calls `func` on every handle owned by the handle table while holding the handle table lock.
    /// Stops if `func` returns an error, returning the error value.
    pub fn for_each_handle<F>(&self, func: F) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        ksync::lock!(let guard = self.read_lock());
        self.for_each_handle_locked(guard.token(), func)
    }

    /// Similar to [`Self::for_each_handle`], but requires the caller to be holding the handle
    /// table lock.
    pub fn for_each_handle_locked<F>(
        &self,
        _token: &LockToken<'_, HandleTableLockClass>,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        /// # Safety
        ///
        /// `ctx` must point to a valid, unaliased `F` on the caller's stack and `disp` must point
        /// to a valid `Dispatcher` for the duration of the synchronous callback invocation.
        unsafe extern "C" fn trampoline<F>(
            ctx: *mut c_void,
            handle: zx_handle_t,
            rights: zx_rights_t,
            disp: *const Dispatcher,
        ) -> zx_status_t
        where
            F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
        {
            // SAFETY: `cpp_handle_table_for_each_handle_locked` invokes `trampoline` synchronously
            // and sequentially on the same thread before returning, so `ctx` points to the live,
            // unaliased `func` on `for_each_handle_locked`'s stack, and `disp` points to a live
            // `Dispatcher` owned by a `Handle` in `handles_` while the handle table lock is held.
            let (cb, disp) = unsafe { (&mut *ctx.cast::<F>(), &*disp) };
            Status::result_into_raw(cb(handle, rights, disp))
        }

        // SAFETY: `self` is a valid `HandleTable`, the handle table lock is held via `_token`, and
        // `cpp_handle_table_for_each_handle_locked` only invokes `trampoline` synchronously before
        // returning, while `func` remains live and unaliased on this stack frame.
        let status = unsafe {
            cpp_handle_table_for_each_handle_locked(
                self.as_ffi(),
                (&raw mut func).cast(),
                trampoline::<F>,
            )
        };
        Status::ok(status)
    }

    /// Iterates over every handle owned by this handle table in batches and calls `func` on each
    /// one without holding the handle table lock during `func`.
    pub fn for_each_handle_batched<F>(&self, mut func: F) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        /// # Safety
        ///
        /// `ctx` must point to a valid, unaliased `F` on the caller's stack and `disp` must point
        /// to a valid `Dispatcher` for the duration of the synchronous callback invocation.
        unsafe extern "C" fn trampoline<F>(
            ctx: *mut c_void,
            handle: zx_handle_t,
            rights: zx_rights_t,
            disp: *const Dispatcher,
        ) -> zx_status_t
        where
            F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
        {
            // SAFETY: `cpp_handle_table_for_each_handle_batched` invokes `trampoline`
            // synchronously and sequentially on the same thread before returning, so `ctx` points
            // to the live, unaliased `func` on `for_each_handle_batched`'s stack, and `disp`
            // points to a live `Dispatcher` kept alive by the batch's
            // `fbl::RefPtr<const Dispatcher>`.
            let (cb, disp) = unsafe { (&mut *ctx.cast::<F>(), &*disp) };
            Status::result_into_raw(cb(handle, rights, disp))
        }

        // SAFETY: `self` is a valid `HandleTable`, and `cpp_handle_table_for_each_handle_batched`
        // only invokes `trampoline` synchronously before returning, while `func` remains live and
        // unaliased on this stack frame.
        let status = unsafe {
            cpp_handle_table_for_each_handle_batched(
                self.as_ffi_mut(),
                (&raw mut func).cast(),
                trampoline::<F>,
            )
        };
        Status::ok(status)
    }

    /// Returns extended handle information for all handles in this handle table.
    pub fn get_handle_info(&self) -> Result<Array<zx_info_handle_extended_t>, Status> {
        loop {
            let count = self.handle_count() as usize;
            // TODO: Bug 45685. This memory allocation should come from a different pool since it
            // can be larger than one page.
            let mut handles = Array::<zx_info_handle_extended_t>::try_new_zeroed_slice(count)
                .map_err(|_| Status::NO_MEMORY)?;

            {
                ksync::lock!(let guard = self.read_lock());
                if count != self.handle_count_locked(guard.token()) as usize {
                    continue;
                }

                let mut index = 0;
                let _ = self.for_each_handle_locked(guard.token(), |handle, rights, disp| {
                    let entry = &mut handles[index];
                    index += 1;
                    *entry = zx_info_handle_extended_t {
                        type_: disp.get_type(),
                        handle_value: handle,
                        rights,
                        reserved: 0,
                        koid: disp.get_koid(),
                        related_koid: disp.get_related_koid(),
                        peer_owner_koid: 0,
                    };
                    Ok(())
                });
            }
            return Ok(handles);
        }
    }
}
