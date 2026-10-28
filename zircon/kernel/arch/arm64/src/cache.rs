// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM64 cache operations.

unsafe extern "C" {
    /// Clean the data cache to the Point of Coherency (PoC) over a range of memory.
    ///
    /// Implemented in assembly (`zircon/kernel/arch/arm64/cache-ops.S`).
    ///
    /// # Safety
    /// `[start, start + len)` must be a valid mapped virtual address range.
    pub fn arch_clean_cache_range(start: usize, len: usize);

    /// Clean and invalidate the data cache over a range of memory.
    ///
    /// Implemented in assembly (`zircon/kernel/arch/arm64/cache-ops.S`).
    ///
    /// # Safety
    /// `[start, start + len)` must be a valid mapped virtual address range.
    pub fn arch_clean_invalidate_cache_range(start: usize, len: usize);

    /// Clean the data cache to the Point of Unification (PoU) over a range of memory.
    ///
    /// Implemented in assembly (`zircon/kernel/arch/arm64/cache-ops.S`).
    ///
    /// # Safety
    /// `[start, start + len)` must be a valid mapped virtual address range.
    pub fn arm64_clean_cache_range_pou(start: usize, len: usize);

    /// Invalidate the instruction cache over a virtual address range.
    ///
    /// Implemented in assembly (`zircon/kernel/lib/arch/arm64/cache-ops.S`).
    ///
    /// # Safety
    /// `[addr, addr + size)` must be a valid mapped virtual address range.
    pub fn InvalidateInstructionCacheRange(addr: usize, size: usize);
}

/// Cleans the data cache over `[start, start + len)` to the Point of Coherency (PoC).
///
/// # Safety
///
/// `[start, start + len)` must be a valid mapped kernel virtual address range (or `len == 0`).
#[inline]
pub unsafe fn clean_dcache_range(start: usize, len: usize) {
    // SAFETY: Caller guarantees `[start, start + len)` is a valid mapped kernel address range.
    unsafe {
        arch_clean_cache_range(start, len);
    }
}

/// Invalidate the entire instruction cache.
#[inline]
pub fn invalidate_global_instruction_cache() {
    // Instruction cache: invalidate all ("iall") inner-sharable ("is") caches
    // to point of unification ("u").
    // SAFETY: `ic ialluis` and `dsb ish` synchronize instruction cache invalidation across the
    // inner-shareable domain without affecting Rust memory safety invariants.
    unsafe {
        core::arch::asm!("ic ialluis", options(nostack, preserves_flags));
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
    libarch::arm64::isb();
}
