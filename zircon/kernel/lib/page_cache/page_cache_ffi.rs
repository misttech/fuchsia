// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{PageCache, PageList};
use core::ffi::c_int;
use core::mem::MaybeUninit;
use core::pin::Pin;
use zx_types::zx_status_t;

unsafe extern "C" {
    pub(super) fn cpp_global_prng_draw(buffer: *mut u8, len: usize);
    pub(super) fn rand_r(seed: *mut usize) -> c_int;
}

// C++ FFI entry points for `page_cache::PageCache` in `<lib/page_cache.h>`.

/// Initializes a `PageCache` in-place at `out_cache`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_cache_create(
    reserve_pages: usize,
    out_cache: &mut MaybeUninit<PageCache>,
) -> zx_status_t {
    match PageCache::new(reserve_pages) {
        Ok(cache) => {
            out_cache.write(cache);
            zx_status::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Drops a `PageCache` in-place at `cache`.
///
/// # Safety
///
/// `cache` must point to a valid, initialized `PageCache` that has not yet been dropped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_destroy(cache: *mut PageCache) {
    debug_assert!(!cache.is_null());
    // SAFETY: Caller guarantees `cache` is a valid initialized `PageCache`.
    unsafe {
        core::ptr::drop_in_place(cache);
    }
}

/// Seeds the random should-wait state for all per-CPU caches in `cache`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_cache_seed_random_should_wait(cache: &PageCache) {
    cache.seed_random_should_wait();
}

/// Allocates `page_count` pages from `cache` into `out_pages` and writes the
/// remaining available pages to `out_available_pages`.
///
/// # Safety
///
/// `out_pages` must be pinned in memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_allocate(
    cache: &PageCache,
    page_count: usize,
    alloc_flags: u32,
    out_pages: &mut PageList,
    out_available_pages: &mut usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees `out_pages` is pinned.
    let out_pages = unsafe { Pin::new_unchecked(out_pages) };
    match cache.allocate(page_count, alloc_flags, out_pages) {
        Ok(available_pages) => {
            *out_available_pages = available_pages;
            zx_status::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Frees `page_list` into `cache`, returning any excess pages to the PMM.
///
/// # Safety
///
/// `page_list` must be pinned in memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_cache_free(cache: &PageCache, page_list: &mut PageList) {
    // SAFETY: Caller guarantees `page_list` is pinned.
    let page_list = unsafe { Pin::new_unchecked(page_list) };
    cache.free(page_list);
}
