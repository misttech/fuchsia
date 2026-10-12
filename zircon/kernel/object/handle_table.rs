// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::{Dispatcher, DispatcherOps};
use super::handle::{HandleOwner, HandleRef, HandleValue};
use super::handle_table_ffi::{
    cpp_handle_table_add_handle_locked,
    cpp_handle_table_get_dispatcher_with_rights_no_policy_check,
    cpp_handle_table_get_handle_locked, cpp_handle_table_koid, cpp_handle_table_lock,
    cpp_handle_table_map_handle_to_value, cpp_handle_table_remove_handle_locked,
};
use super::process_dispatcher::ProcessDispatcher;
use crate::kernel::thread::AutoExpiringPreemptDisabler;
use core::convert::Infallible;
use core::ffi::c_void;
use core::fmt::{self, Debug, Formatter};
use core::mem::{MaybeUninit, size_of};
use core::ptr::{self, NonNull};
use fbl::{HasRefCount, Recyclable, RefPtr};
use ksync::{BrwLockPi, BrwLockPiReadGuard, BrwLockPiWriteGuard, LockClass, LockToken};
use pin_init::PinInit;
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_OBJ_TYPE_NONE, ZX_RIGHT_NONE, zx_handle_t, zx_koid_t, zx_rights_t,
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
        _token: &'a LockToken<'_, HandleTableLockClass>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'a>> {
        // SAFETY: `self` and `caller` are valid and the handle table lock is held for `'a`.
        let ptr = unsafe {
            cpp_handle_table_get_handle_locked(
                self.as_ffi_mut(),
                caller.as_ffi_mut(),
                handle_value.raw_value(),
            )
        };
        // SAFETY: `ptr` is a valid handle pointer in `self` while the lock is held.
        NonNull::new(ptr).map(|ptr| unsafe { HandleRef::from_raw(ptr) })
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
        let ptr = self.get_handle_locked(token, caller, handle_value)?.as_ptr();
        // SAFETY: `self` is valid, the handle table write lock is held, and `ptr` was looked up in
        // `self` under the same lock.
        let raw =
            unsafe { cpp_handle_table_remove_handle_locked(self.as_ffi_mut(), ptr as *mut _) };
        // SAFETY: `raw` is a non-null owned handle pointer released from the handle table.
        Some(unsafe { HandleOwner::from_raw(raw).unwrap() })
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
        let (dispatcher, rights) = {
            ksync::lock!(let guard = self.read_lock());
            let handle = self
                .get_handle_locked(guard.token(), caller, handle_value)
                .ok_or(Status::BAD_HANDLE)?;
            (handle.dispatcher(), handle.rights())
        };
        if T::TYPE != ZX_OBJ_TYPE_NONE && dispatcher.get_type() != T::TYPE {
            return Err(Status::WRONG_TYPE);
        }
        // SAFETY: We verified the type of the dispatcher matches `T::TYPE`, so it is safe to cast.
        Ok((unsafe { dispatcher.cast::<T>() }, rights))
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
        let (dispatcher, actual_rights) =
            self.get_dispatcher_and_rights::<T>(caller, handle_value)?;
        if (actual_rights & rights) != rights {
            return Err(Status::ACCESS_DENIED);
        }
        Ok((dispatcher, actual_rights))
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
            self.get_dispatcher_with_rights_and_actual::<T>(caller, handle_value, rights)?;
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
        const { assert!(T::TYPE != ZX_OBJ_TYPE_NONE) };
        let mut dispatcher = MaybeUninit::<RefPtr<Dispatcher>>::uninit();
        // SAFETY: `self` is a valid `HandleTable` whose lock is not held here, and `dispatcher`
        // points to valid uninitialized memory that is initialized only on success.
        let status = unsafe {
            cpp_handle_table_get_dispatcher_with_rights_no_policy_check(
                self.as_ffi_mut(),
                handle_value.raw_value(),
                T::TYPE,
                rights,
                &raw mut dispatcher,
            )
        };
        Status::ok(status)?;
        // SAFETY: The call succeeded, so `dispatcher` was initialized, and C++ verified that its
        // type matches `T::TYPE`, so it is safe to cast.
        Ok(unsafe { dispatcher.assume_init().cast::<T>() })
    }
}
