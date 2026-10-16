// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::Dispatcher;
use super::handle_table::HandleTable;
use super::process_dispatcher::ProcessDispatcher;
use core::ffi::c_void;
use zx_types::{zx_handle_t, zx_koid_t, zx_rights_t, zx_status_t};

pub(crate) type HandleTableForEachHandleFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    handle: zx_handle_t,
    rights: zx_rights_t,
    disp: *const Dispatcher,
) -> zx_status_t;

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
    /// `handle_table` must point to a valid `HandleTable`, `caller` must be null (to skip
    /// `ZX_POL_BAD_HANDLE` enforcement) or point to a valid `ProcessDispatcher`, and the handle
    /// table lock must be held.
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

    /// Returns the number of outstanding handles in `handle_table`.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`.
    pub(crate) fn cpp_handle_table_handle_count(handle_table: *const HandleTable) -> u32;

    /// Returns the number of outstanding handles in `handle_table` while holding the handle table
    /// lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable` and the handle table lock must be held.
    pub(crate) fn cpp_handle_table_handle_count_locked(handle_table: *const HandleTable) -> u32;

    /// Iterates over every handle owned by `handle_table` while holding the handle table lock.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable` with its lock held, and `ctx` and `func`
    /// must be valid for synchronous invocations during the call.
    pub(crate) fn cpp_handle_table_for_each_handle_locked(
        handle_table: *const HandleTable,
        ctx: *mut c_void,
        func: HandleTableForEachHandleFn,
    ) -> zx_status_t;

    /// Iterates over every handle owned by `handle_table` in batches without holding the handle
    /// table lock while invoking `func`.
    ///
    /// # Safety
    ///
    /// `handle_table` must point to a valid `HandleTable`, and `ctx` and `func` must be valid
    /// for synchronous invocations during the call.
    pub(crate) fn cpp_handle_table_for_each_handle_batched(
        handle_table: *mut HandleTable,
        ctx: *mut c_void,
        func: HandleTableForEachHandleFn,
    ) -> zx_status_t;
}
