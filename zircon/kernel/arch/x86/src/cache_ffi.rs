// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::cache;

/// FFI wrapper for `arch_dcache_line_size`.
#[unsafe(no_mangle)]
pub extern "C" fn arch_dcache_line_size() -> u32 {
    cache::arch_dcache_line_size()
}

/// FFI wrapper for `arch_icache_line_size`.
#[unsafe(no_mangle)]
pub extern "C" fn arch_icache_line_size() -> u32 {
    cache::arch_icache_line_size()
}

/// FFI wrapper for `arch_sync_cache_range`.
#[unsafe(no_mangle)]
pub extern "C" fn arch_sync_cache_range(start: usize, len: usize) {
    cache::arch_sync_cache_range(start, len);
}

/// FFI wrapper for `arch_invalidate_cache_range`.
#[unsafe(no_mangle)]
pub extern "C" fn arch_invalidate_cache_range(start: usize, len: usize) {
    cache::arch_invalidate_cache_range(start, len);
}

/// FFI wrapper for `arch_clean_cache_range`.
///
/// # Safety
///
/// If `len > 0`, the caller must ensure that `[start, start + len)` is a valid mapped address
/// range and that `start + len` does not overflow or wrap around the address space.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_clean_cache_range(start: usize, len: usize) {
    // SAFETY: If `len > 0`, the caller guarantees `[start, start + len)` is a valid mapped
    // address range that does not overflow or wrap around the address space.
    unsafe {
        cache::arch_clean_cache_range(start, len);
    }
}

/// FFI wrapper for `arch_clean_invalidate_cache_range`.
///
/// # Safety
///
/// If `len > 0`, the caller must ensure that `[start, start + len)` is a valid mapped address
/// range and that `start + len` does not overflow or wrap around the address space.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_clean_invalidate_cache_range(start: usize, len: usize) {
    // SAFETY: If `len > 0`, the caller guarantees `[start, start + len)` is a valid mapped
    // address range that does not overflow or wrap around the address space.
    unsafe {
        cache::arch_clean_invalidate_cache_range(start, len);
    }
}
