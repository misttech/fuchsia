// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exception_dispatcher::{ExceptionDispatcher, ExceptionDispatcherState};
use super::thread_dispatcher::ThreadDispatcher;
use crate::arch_rs::ArchExceptionContext;
use fbl::RefPtr;
use zx_types::{zx_exception_report_t, zx_excp_type_t};

unsafe extern "C" {
    /// Calls into C++ to allocate and initialize an `ExceptionDispatcher`.
    ///
    /// # Safety
    ///
    /// `thread` must be a valid owning raw pointer exported from `fbl::RefPtr<ThreadDispatcher>`,
    /// and `report` and `arch_context` must remain valid until `clear()` is called.
    pub(super) fn cpp_exception_dispatcher_create(
        thread: *mut ThreadDispatcher,
        exception_type: zx_excp_type_t,
        report: *const zx_exception_report_t,
        arch_context: *const ArchExceptionContext,
    ) -> *mut ExceptionDispatcher;
}

crate::object::dispatcher::impl_dispatcher_state_init!(
    ExceptionDispatcher,
    ExceptionDispatcherState,
    thread: RefPtr<ThreadDispatcher>,
    exception_type: zx_excp_type_t,
    report: *const zx_exception_report_t,
    arch_context: *const ArchExceptionContext,
);

/// Invokes `on_zero_handles` on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_on_zero_handles(disp: &ExceptionDispatcher) {
    disp.on_zero_handles();
}
