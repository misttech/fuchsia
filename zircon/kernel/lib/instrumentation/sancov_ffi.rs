// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C FFI bindings for `sancov`.

use core::slice;
use core::sync::atomic::AtomicU32;

use super::kernel_mapped_vmo::KernelMappedVmo;
use super::sancov::{
    data_size, init_sancov, sanitizer_cov_trace_pc_guard, sanitizer_cov_trace_pc_guard_init,
};

/// Returns the byte size of the sancov PC or counter table for `guards_count` guards.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub extern "C" fn rust_sancov_data_size(guards_count: usize) -> usize {
    data_size(guards_count)
}

/// Initializes the sancov PC and counter VMOs and migrates pre-initialization guard counts.
///
/// # Safety
///
/// - `guards_start` must point to a valid, aligned array of `guards_count` `u32` elements.
/// - `pc_vmo` and `counts_vmo` must point to valid, aligned, distinct `KernelMappedVmo` instances
///   that remain live and mapped for `'static` (the remaining lifetime of the kernel).
/// - Must only be called once on the boot CPU before secondary CPUs are brought online.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub unsafe extern "C" fn rust_sancov_init(
    guards_start: *mut u32,
    guards_count: usize,
    pc_vmo: *mut KernelMappedVmo,
    counts_vmo: *mut KernelMappedVmo,
) {
    // SAFETY: Caller guarantees `guards_start`, `pc_vmo`, and `counts_vmo` are valid and distinct.
    // `AtomicU32` has the same size and alignment as `u32` and allows interior mutability if
    // instrumented functions run during `init_sancov`.
    let (guards, pc_vmo, counts_vmo) = unsafe {
        (
            slice::from_raw_parts(guards_start.cast::<AtomicU32>(), guards_count),
            &*pc_vmo,
            &*counts_vmo,
        )
    };
    init_sancov(guards, pc_vmo, counts_vmo);
}

/// Validates that the compiler-provided sancov guard section bounds match the linker symbols.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub extern "C" fn rust_sancov_trace_pc_guard_init(
    start: *const u32,
    end: *const u32,
    expected_start: *const u32,
    expected_end: *const u32,
) {
    sanitizer_cov_trace_pc_guard_init(start, end, expected_start, expected_end);
}

/// Records a coverage hit for the guard slot at `guard_ptr`.
///
/// # Safety
///
/// See [`sanitizer_cov_trace_pc_guard`].
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub unsafe extern "C" fn rust_sancov_trace_pc_guard(
    guard_ptr: *mut u32,
    guards_start: *const u32,
    return_address: usize,
    executable_start: usize,
) {
    // SAFETY: Caller upholds `sanitizer_cov_trace_pc_guard` safety requirements.
    unsafe {
        sanitizer_cov_trace_pc_guard(guard_ptr, guards_start, return_address, executable_start);
    }
}
