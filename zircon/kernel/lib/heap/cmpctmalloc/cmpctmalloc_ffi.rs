// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Google, Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `cmpctmalloc`.

use core::ffi::c_void;

use super::cmpctmalloc::{
    CmpctDumpOptions, HEAP_MAX_ALLOC_SIZE, cmpct_alloc, cmpct_dump, cmpct_free, cmpct_get_cookie,
    cmpct_get_info, cmpct_get_size, cmpct_init, cmpct_memalign, cmpct_set_cookie,
    cmpct_set_fill_on_alloc_threshold, cmpct_sized_free, cmpct_test,
};

/// C-visible maximum allocation size (`kHeapMaxAllocSize`).
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc.h.
#[unsafe(no_mangle)]
pub static kHeapMaxAllocSize: usize = HEAP_MAX_ALLOC_SIZE;

unsafe extern "C" {
    /// Allocates `pages` contiguous pages from the VM for the heap.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is held and `pages > 0`.
    pub(super) fn heap_page_alloc(pages: usize) -> *mut c_void;

    /// Returns `pages` contiguous pages starting at `ptr` to the VM.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is held, `ptr` is a page-aligned pointer
    /// previously returned by `heap_page_alloc`, and `pages > 0`.
    pub(super) fn heap_page_free(ptr: *mut c_void, pages: usize);

    /// Reports a heap allocation failure of `size` bytes to the PMM.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is not held on the current thread.
    pub(super) fn heap_report_alloc_failure(size: usize);
}

/// FFI entry point for [`cmpct_alloc`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held on the current thread and the heap has
/// been initialized via [`rust_cmpct_init`].
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_alloc(size: usize) -> *mut c_void {
    // SAFETY: Caller upholds the safety requirements of `cmpct_alloc`.
    unsafe { cmpct_alloc(size) }
}

/// FFI entry point for [`cmpct_free`].
///
/// # Safety
///
/// The caller must ensure that `payload` is either null or a pointer previously returned by
/// `cmpct_alloc` or `cmpct_memalign` that has not yet been freed, and `TheHeapLock` is not held.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_free(payload: *mut c_void) {
    // SAFETY: Caller upholds the safety requirements of `cmpct_free`.
    unsafe { cmpct_free(payload) }
}

/// FFI entry point for [`cmpct_sized_free`].
///
/// # Safety
///
/// The caller must ensure that `payload` is either null or a pointer previously returned by
/// `cmpct_alloc` or `cmpct_memalign` with size `s` that has not yet been freed, and `TheHeapLock`
/// is not held.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_sized_free(payload: *mut c_void, s: usize) {
    // SAFETY: Caller upholds the safety requirements of `cmpct_sized_free`.
    unsafe { cmpct_sized_free(payload, s) }
}

/// FFI entry point for [`cmpct_memalign`].
///
/// # Safety
///
/// The caller must ensure that `alignment` is a power of two (or `< 8`), `TheHeapLock` is not
/// held on the current thread, and the heap has been initialized via [`rust_cmpct_init`].
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_memalign(alignment: usize, size: usize) -> *mut c_void {
    // SAFETY: Caller upholds the safety requirements of `cmpct_memalign`.
    unsafe { cmpct_memalign(alignment, size) }
}

/// FFI entry point for [`cmpct_set_fill_on_alloc_threshold`].
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub extern "C" fn rust_cmpct_set_fill_on_alloc_threshold(size: usize) {
    cmpct_set_fill_on_alloc_threshold(size);
}

/// FFI entry point for [`cmpct_init`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held and no concurrent heap operations are
/// in flight.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_init() {
    // SAFETY: Caller upholds the safety requirements of `cmpct_init`.
    unsafe { cmpct_init() }
}

/// FFI entry point for [`cmpct_dump`].
///
/// # Safety
///
/// If `options` includes [`CmpctDumpOptions::PANIC_TIME`], the caller must ensure that all other
/// CPUs are halted in a kernel panic with no concurrent mutation of `THE_HEAP`. Otherwise, the
/// caller must ensure that `TheHeapLock` is not held on the current thread.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_dump(options: u8) {
    // SAFETY: Caller upholds the safety requirements of `cmpct_dump`.
    unsafe { cmpct_dump(CmpctDumpOptions::from_bits_retain(options)) };
}

/// FFI entry point for [`cmpct_get_info`].
///
/// # Safety
///
/// The caller must ensure that each non-null pointer among `used_bytes`, `free_bytes`, and
/// `cached_bytes` is properly aligned and valid for writes of `size_of::<usize>()` bytes.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_get_info(
    used_bytes: *mut usize,
    free_bytes: *mut usize,
    cached_bytes: *mut usize,
) {
    let info = cmpct_get_info();
    // SAFETY: Caller guarantees each non-null pointer is aligned and valid for writes.
    unsafe {
        if !used_bytes.is_null() {
            used_bytes.write(info.used_bytes);
        }
        if !free_bytes.is_null() {
            free_bytes.write(info.free_bytes);
        }
        if !cached_bytes.is_null() {
            cached_bytes.write(info.cached_bytes);
        }
    }
}

/// FFI entry point for [`cmpct_test`].
///
/// # Safety
///
/// The caller must ensure that the heap has been initialized and `TheHeapLock` is not held on the
/// current thread.
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_test() {
    // SAFETY: Caller upholds the safety requirements of `cmpct_test`.
    unsafe { cmpct_test() };
}

/// FFI entry point for [`cmpct_set_cookie`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation currently owned
/// by the caller (returned by `cmpct_alloc` or `cmpct_memalign` and not yet freed), with no
/// concurrent access to its header cookie.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_set_cookie(ptr: *mut c_void, cookie: u32) {
    // SAFETY: Caller guarantees `ptr` is a live non-null allocation payload pointer.
    unsafe { cmpct_set_cookie(ptr, cookie) }
}

/// FFI entry point for [`cmpct_get_cookie`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation currently owned
/// by the caller (returned by `cmpct_alloc` or `cmpct_memalign` and not yet freed), with no
/// concurrent mutation of its header cookie.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_get_cookie(ptr: *mut c_void) -> u32 {
    // SAFETY: Caller guarantees `ptr` is a live non-null allocation payload pointer.
    unsafe { cmpct_get_cookie(ptr) }
}

/// FFI entry point for [`cmpct_get_size`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation currently owned
/// by the caller (returned by `cmpct_alloc` or `cmpct_memalign` and not yet freed).
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc_ffi.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_cmpct_get_size(ptr: *mut c_void) -> u32 {
    // SAFETY: Caller guarantees `ptr` is a live non-null allocation payload pointer.
    unsafe { cmpct_get_size(ptr) }
}
