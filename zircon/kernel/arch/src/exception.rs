// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Cross-architecture user exception context types and helpers (`<arch/exception.h>`).

use zx_types::zx_exception_report_t;

/// Architectural exception context (`arch_exception_context_t`).
#[cfg(target_arch = "riscv64")]
pub type ArchExceptionContext = super::riscv64::exceptions::ArchExceptionContext;

/// Opaque type representing a C++ `arch_exception_context_t`.
#[cfg(not(target_arch = "riscv64"))]
#[repr(C)]
pub struct ArchExceptionContext([u8; 0]);

#[cfg(not(target_arch = "riscv64"))]
mod ffi {
    use super::ArchExceptionContext;
    use zx_types::zx_exception_report_t;

    #[allow(improper_ctypes)]
    unsafe extern "C" {
        pub(super) fn cpp_arch_exception_context_user_synth_code(
            context: *const ArchExceptionContext,
        ) -> u32;
        pub(super) fn cpp_arch_exception_context_user_synth_data(
            context: *const ArchExceptionContext,
        ) -> u32;
        pub(super) fn arch_dump_exception_context(context: *const ArchExceptionContext);
        pub(super) fn arch_fill_in_exception_context(
            context: *const ArchExceptionContext,
            report: *mut zx_exception_report_t,
        );
    }
}

/// Returns `context->user_synth_code` from an architecture exception context.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t`.
#[inline]
pub unsafe fn arch_exception_context_user_synth_code(context: *const ArchExceptionContext) -> u32 {
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `context` points to a valid `ArchExceptionContext`.
        unsafe { (*context).user_synth_code }
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
        unsafe { ffi::cpp_arch_exception_context_user_synth_code(context) }
    }
}

/// Returns `context->user_synth_data` from an architecture exception context.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t`.
#[inline]
pub unsafe fn arch_exception_context_user_synth_data(context: *const ArchExceptionContext) -> u32 {
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `context` points to a valid `ArchExceptionContext`.
        unsafe { (*context).user_synth_data }
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
        unsafe { ffi::cpp_arch_exception_context_user_synth_data(context) }
    }
}

/// Dumps architecture-specific exception state to the console.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t` for the current thread.
#[inline]
pub unsafe fn arch_dump_exception_context(context: *const ArchExceptionContext) {
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
        unsafe {
            super::riscv64::exceptions::arch_dump_exception_context(context);
        }
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
        unsafe {
            ffi::arch_dump_exception_context(context);
        }
    }
}

/// Populates architecture-specific fields of `report` from `context`.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t` and `report` must point
/// to a valid, writable `zx_exception_report_t`.
#[inline]
pub unsafe fn arch_fill_in_exception_context(
    context: *const ArchExceptionContext,
    report: *mut zx_exception_report_t,
) {
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `context` and `report` are valid.
        unsafe {
            super::riscv64::exceptions::arch_fill_in_exception_context(context, report);
        }
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // SAFETY: Caller guarantees `context` and `report` are valid.
        unsafe {
            ffi::arch_fill_in_exception_context(context, report);
        }
    }
}
