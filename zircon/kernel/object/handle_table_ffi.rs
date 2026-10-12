// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::Dispatcher;
use super::handle_table::HandleTable;
use super::process_dispatcher::ProcessDispatcher;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_types::{zx_handle_t, zx_koid_t, zx_obj_type_t, zx_rights_t, zx_status_t};

unsafe extern "C" {
    /// Returns a pointer to the handle table's `BrwLockPi` lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`.
    pub(crate) fn cpp_handle_table_lock(handle_table: *const HandleTable) -> *mut c_void;

    /// Returns the KOID of `handle_table`.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`.
    pub(crate) fn cpp_handle_table_koid(handle_table: *const HandleTable) -> zx_koid_t;

    /// Maps a `Handle` pointer to its user-visible handle value for `handle_table`.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable` and `handle` must point to a valid
    /// `Handle`.
    pub(crate) fn cpp_handle_table_map_handle_to_value(
        handle_table: *const HandleTable,
        handle: *const c_void,
    ) -> zx_handle_t;

    /// Looks up a handle in `handle_table` while holding the lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`, `caller` must point to a valid
    /// `ProcessDispatcher`, and the handle table lock must be held.
    pub(crate) fn cpp_handle_table_get_handle_locked(
        handle_table: *mut HandleTable,
        caller: *mut ProcessDispatcher,
        handle_value: zx_handle_t,
    ) -> *mut c_void;

    /// Adds an owned handle to `handle_table` while holding its write lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`, `handle` must be a valid owned
    /// `Handle*` whose ownership is transferred, and the handle table write lock must be held.
    pub(crate) fn cpp_handle_table_add_handle_locked(
        handle_table: *mut HandleTable,
        handle: *mut c_void,
    );

    /// Removes a handle by pointer from `handle_table` while holding its write lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`, `handle` must point to a handle in
    /// `handle_table`, and the handle table write lock must be held.
    pub(crate) fn cpp_handle_table_remove_handle_locked(
        handle_table: *mut HandleTable,
        handle: *mut c_void,
    ) -> *mut c_void;

    /// Looks up a dispatcher of `expected_type` with `rights` in `handle_table` without enforcing
    /// `ZX_POL_BAD_HANDLE`, via `HandleTable::GetDispatcherWithRightsNoPolicyCheck`.
    ///
    /// Returns `ZX_ERR_BAD_HANDLE` if the handle is not found, `ZX_ERR_WRONG_TYPE` if the
    /// dispatcher is not of `expected_type`, and `ZX_ERR_ACCESS_DENIED` if the handle lacks
    /// `rights`. `out_dispatcher` is initialized only on success.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable` whose lock is not held by the caller,
    /// and `out_dispatcher` must point to valid uninitialized memory for a
    /// `fbl::RefPtr<Dispatcher>`.
    pub(crate) fn cpp_handle_table_get_dispatcher_with_rights_no_policy_check(
        handle_table: *mut HandleTable,
        handle_value: zx_handle_t,
        expected_type: zx_obj_type_t,
        rights: zx_rights_t,
        out_dispatcher: *mut MaybeUninit<RefPtr<Dispatcher>>,
    ) -> zx_status_t;
}
