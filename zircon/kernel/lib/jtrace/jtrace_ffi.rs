// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI boundary for Jump Trace (`jtrace`).

use core::ffi::c_void;

use super::{
    ProdEntry, TraceBufferType, jtrace_dump, jtrace_init, jtrace_invalidate, jtrace_log,
    jtrace_set_after_thread_init_early, jtrace_set_location,
};

/// FFI entry point for C++ `jtrace_init()`.
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub extern "C" fn rust_jtrace_init() {
    jtrace_init();
}

/// FFI entry point for C++ `jtrace_set_after_thread_init_early()`.
///
/// # Safety
///
/// The caller must ensure that early thread initialization is complete so that
/// [`ThreadPtr::current()`](crate::kernel::thread::ThreadPtr::current) is valid on the calling
/// CPU.
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_jtrace_set_after_thread_init_early() {
    // SAFETY: Caller guarantees early thread initialization is complete.
    unsafe { jtrace_set_after_thread_init_early() };
}

/// FFI entry point for C++ `jtrace_set_location(void* ptr, size_t len)`.
///
/// # Safety
///
/// The caller must ensure that this function is not called concurrently with itself or
/// `jtrace_dump(TraceBufferType::Recovered)`. If `ptr` is non-null, properly aligned, and `len`
/// is large enough to hold at least one entry, the caller must ensure that `ptr` refers to a live
/// (not freed) object/allocation of `len` initialized (e.g. zero-initialized or previously
/// initialized persistent RAM) readable and writable bytes that remains valid for the `'static`
/// lifetime of the kernel.
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_jtrace_set_location(ptr: *mut c_void, len: usize) {
    // SAFETY: Caller upholds the buffer validity, initialization, and lifetime preconditions.
    unsafe { jtrace_set_location(ptr, len) };
}

/// FFI entry point for C++ `jtrace_invalidate()`.
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub extern "C" fn rust_jtrace_invalidate() {
    jtrace_invalidate();
}

/// FFI entry point for C++ `jtrace_log(Entry<kJTraceUseLargeEntries>& e)`.
///
/// # Safety
///
/// The caller must ensure that `e`'s pointer fields (`tag`, and for `LargeEntry` `ffl_info` and
/// its `file`/`func` pointers) are either null or point to valid `'static` null-terminated C
/// strings / [`FileFuncLineInfo`](super::FileFuncLineInfo).
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_jtrace_log(e: &mut ProdEntry) {
    // SAFETY: Caller guarantees `e`'s pointer fields are null or valid `'static` C strings /
    // `FileFuncLineInfo`.
    unsafe { jtrace_log(e) };
}

/// FFI entry point for C++ `jtrace_dump(TraceBufferType which)`.
// SAFETY: Globally unique symbol name matching the extern "C" declaration in
// <lib/jtrace/jtrace_ffi.h>.
#[unsafe(no_mangle)]
pub extern "C" fn rust_jtrace_dump(which: TraceBufferType) {
    jtrace_dump(which);
}
