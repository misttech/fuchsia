// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exception::{
    dispatch_user_exception as rust_dispatch_user_exception,
    dump_common_exception_context as rust_dump_common_exception_context,
};
use crate::arch_rs::ArchExceptionContext;
use zx_status::Status;
use zx_types::zx_status_t;

/// C ABI entry point called by architecture exception handlers when they cannot handle a user
/// exception.
///
/// # Safety
///
/// * `arch_context` must point to a valid, initialized `arch_exception_context_t` for the current
///   thread that outlives this call.
/// * If `(*arch_context).frame` is non-null, it must point to a valid, initialized, writable
///   `iframe_t` that outlives this call without conflicting Rust references.
/// * No thread lock, dispatcher lock, spinlock, or active `ChainLockTransaction` may be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dispatch_user_exception(
    exception_type: u32,
    arch_context: *const ArchExceptionContext,
) -> zx_status_t {
    // SAFETY: Caller upholds all preconditions of `rust_dispatch_user_exception`.
    Status::result_into_raw(unsafe { rust_dispatch_user_exception(exception_type, arch_context) })
}

/// C ABI entry point called by architecture `arch_dump_exception_context` implementations to print
/// common fields of `arch_exception_context_t`.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dump_common_exception_context(context: *const ArchExceptionContext) {
    // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
    unsafe {
        rust_dump_common_exception_context(context);
    }
}
