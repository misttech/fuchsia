// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86 cache operations.

use libarch::x86::cpuid::ProcessorInfo;
use libarch::x86::{device_memory_barrier, serialize_instructions};

use super::feature::{
    X86_FEATURE_CLFLUSH, X86_FEATURE_CLFLUSHOPT, X86_FEATURE_CLWB, X86CpuidLeafNum,
    x86_feature_test, x86_get_cpuid_leaf,
};

#[inline(always)]
fn cpuid_cache_line_size_bytes() -> usize {
    let ebx = x86_get_cpuid_leaf(X86CpuidLeafNum::ModelFeatures).map_or(0, |l| l.b);
    ProcessorInfo::from(ebx).cache_line_size_bytes()
}

/// Returns the L1 data cache line size in bytes.
#[inline]
pub fn arch_dcache_line_size() -> u32 {
    cpuid_cache_line_size_bytes() as u32
}

/// Returns the L1 instruction cache line size in bytes.
#[inline]
pub fn arch_icache_line_size() -> u32 {
    cpuid_cache_line_size_bytes() as u32
}

/// Synchronizes instruction and data caches across `[start, start + len)`.
#[inline]
pub fn arch_sync_cache_range(_start: usize, _len: usize) {
    // Invoke cpuid to act as a serializing instruction.  This will ensure we
    // see modifications to future parts of the instruction stream.  See
    // Intel Volume 3, 8.1.3 "Handling Self- and Cross-Modifying Code".  cpuid
    // is the more conservative approach suggested in this section.
    serialize_instructions();
}

/// Invalidates the data cache over `[start, start + len)`.
#[inline]
pub fn arch_invalidate_cache_range(_start: usize, _len: usize) {}

#[inline(always)]
fn for_each_cacheline(start: usize, len: usize, mut function: impl FnMut(usize)) {
    if len == 0 {
        return;
    }
    let clsize = cpuid_cache_line_size_bytes();
    if clsize == 0 {
        return;
    }
    let end = start.wrapping_add(len);
    let mut ptr = start & clsize.wrapping_neg();
    while ptr < end {
        function(ptr);
        ptr = ptr.wrapping_add(clsize);
    }
}

/// Cleans (writes back) the data cache over `[start, start + len)`.
///
/// # Safety
///
/// If `len > 0`, the caller must ensure that `[start, start + len)` is a valid mapped address
/// range and that `start + len` does not overflow or wrap around the address space.
#[inline]
pub unsafe fn arch_clean_cache_range(start: usize, len: usize) {
    if x86_feature_test(X86_FEATURE_CLWB) {
        for_each_cacheline(start, len, |ptr| {
            // SAFETY: `for_each_cacheline` only invokes this closure when `len > 0`, and the
            // caller guarantees `[start, start + len)` is a valid mapped address range without
            // wrapping around, so `ptr` addresses a cacheline overlapping that range.
            unsafe {
                core::arch::asm!("clwb [{}]", in(reg) ptr, options(nostack, preserves_flags));
            }
        });
        // SAFETY: `sfence` orders preceding `clwb` operations.
        unsafe {
            core::arch::asm!("sfence", options(nostack, preserves_flags));
        }
    } else {
        core::hint::cold_path();
        // SAFETY: If `len > 0`, the caller guarantees `[start, start + len)` is a valid mapped
        // address range that does not overflow or wrap around the address space.
        unsafe {
            arch_clean_invalidate_cache_range(start, len);
        }
    }
}

/// Cleans and invalidates the data cache over `[start, start + len)`.
///
/// # Safety
///
/// If `len > 0`, the caller must ensure that `[start, start + len)` is a valid mapped address
/// range and that `start + len` does not overflow or wrap around the address space.
#[inline]
pub unsafe fn arch_clean_invalidate_cache_range(start: usize, len: usize) {
    if !x86_feature_test(X86_FEATURE_CLFLUSH) {
        core::hint::cold_path();
        // SAFETY: `wbinvd` writes back and invalidates all processor caches in ring 0.
        unsafe {
            core::arch::asm!("wbinvd", options(nostack, preserves_flags));
        }
        return;
    }

    if x86_feature_test(X86_FEATURE_CLFLUSHOPT) {
        for_each_cacheline(start, len, |ptr| {
            // SAFETY: `for_each_cacheline` only invokes this closure when `len > 0`, and the
            // caller guarantees `[start, start + len)` is a valid mapped address range without
            // wrapping around, so `ptr` addresses a cacheline overlapping that range.
            unsafe {
                core::arch::asm!(
                    "clflushopt [{}]",
                    in(reg) ptr,
                    options(nostack, preserves_flags),
                );
            }
        });
        // SAFETY: `sfence` orders preceding `clflushopt` operations.
        unsafe {
            core::arch::asm!("sfence", options(nostack, preserves_flags));
        }
    } else {
        core::hint::cold_path();
        for_each_cacheline(start, len, |ptr| {
            // SAFETY: `for_each_cacheline` only invokes this closure when `len > 0`, and the
            // caller guarantees `[start, start + len)` is a valid mapped address range without
            // wrapping around, so `ptr` addresses a cacheline overlapping that range.
            unsafe {
                core::arch::asm!("clflush [{}]", in(reg) ptr, options(nostack, preserves_flags));
            }
        });
        // `clflush` is unordered with respect to subsequent memory operations unless fenced by
        // `mfence` on both Intel and AMD.
        device_memory_barrier();
    }
}
