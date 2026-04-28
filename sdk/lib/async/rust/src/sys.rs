// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains functions that wrap low-level libasync functions.
//!
//! In order to run tests through MIRI, we have to avoid external function
//! calls. These function calls include all of the helper functions like
//! `async_post_task` and `async_cancel_task`, which look up the corresponding
//! function pointers in the dispatcher's `ops` table and call them. When we're
//! not running through MIRI, we call the helper methods per best practices.

use libasync_sys::{async_dispatcher_t, async_task_t};
use zx::sys::zx_status_t;

/// # Safety
///
/// `dispatcher` must point to a valid `async_dispatcher_t`, and `task` must
/// point to a valid `async_task_t`.
#[inline(always)]
pub unsafe fn async_post_task(
    dispatcher: *mut async_dispatcher_t,
    task: *mut async_task_t,
) -> zx_status_t {
    #[cfg(not(miri))]
    // SAFETY: The caller guaranteed that `dispatcher` points to a valid
    // `async_dispatcher_t`, and `task` points to a valid `async_task_t`.
    unsafe {
        libasync_sys::async_post_task(dispatcher, task)
    }
    #[cfg(miri)]
    {
        use core::ptr::addr_of;

        use libasync_sys::ASYNC_OPS_V1;
        use zx_types::ZX_ERR_NOT_SUPPORTED;

        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let ops = unsafe { addr_of!(*(*dispatcher).ops) };
        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let version = unsafe { addr_of!((*ops).version).read() };
        if version < ASYNC_OPS_V1 {
            return ZX_ERR_NOT_SUPPORTED;
        }
        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let v1 = unsafe { &*addr_of!((*ops).v1) };
        let Some(post_task) = v1.post_task else {
            return ZX_ERR_NOT_SUPPORTED;
        };
        // SAFETY: The caller guaranteed that `dispatcher` points to a valid
        // `async_dispatcher_t`, and `task` points to a valid `async_task_t`.
        unsafe { (post_task)(dispatcher, task) }
    }
}

/// # Safety
///
/// `dispatcher` must point to a valid `async_dispatcher_t`, and `task` must
/// point to a valid `async_task_t`.
#[inline(always)]
pub unsafe fn async_cancel_task(
    dispatcher: *mut async_dispatcher_t,
    task: *mut async_task_t,
) -> zx_status_t {
    #[cfg(not(miri))]
    // SAFETY: The caller guaranteed that `dispatcher` points to a valid
    // `async_dispatcher_t`, and `task` points to a valid `async_task_t`.
    unsafe {
        libasync_sys::async_cancel_task(dispatcher, task)
    }
    #[cfg(miri)]
    {
        use core::ptr::addr_of;

        use libasync_sys::ASYNC_OPS_V1;
        use zx_types::ZX_ERR_NOT_SUPPORTED;

        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let ops = unsafe { addr_of!(*(*dispatcher).ops) };
        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let version = unsafe { addr_of!((*ops).version).read() };
        if version < ASYNC_OPS_V1 {
            return ZX_ERR_NOT_SUPPORTED;
        }
        // SAFETY: The caller guaranteed that the dispatcher points to a valid
        // `async_dispatcher_t`.
        let v1 = unsafe { &*addr_of!((*ops).v1) };
        let Some(cancel_task) = v1.cancel_task else {
            return ZX_ERR_NOT_SUPPORTED;
        };
        // SAFETY: The caller guaranteed that `dispatcher` points to a valid
        // `async_dispatcher_t`, and `task` points to a valid `async_task_t`.
        unsafe { (cancel_task)(dispatcher, task) }
    }
}
