// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::VAddr;

/// VM feature bit indicating that execute-only memory (XOM) mappings are supported.
pub const ZX_VM_FEATURE_CAN_MAP_XOM: u32 = 1 << 0;

unsafe extern "C" {
    fn arch_zero_page(va: *mut core::ffi::c_void);
    fn arch_clean_cache_range(start: usize, len: usize);
    fn cpp_arch_vm_features() -> u32;
    fn cpp_arch_num_spinlocks_held() -> u32;
    fn cpp_arch_cpu_features() -> u32;
    fn cpp_arch_cpu_sve_features() -> u32;
    fn cpp_arch_dcache_line_size() -> u32;
    fn cpp_arch_icache_line_size() -> u32;
}

// These are implemented in C++ on x86; arm64 and riscv64 call the Rust implementations directly.
#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn arch_get_hw_breakpoint_count() -> u8;
    fn arch_get_hw_watchpoint_count() -> u8;
    fn arch_address_tagging_features() -> u32;
}

/// Arch optimized version of a page zero routine against a page aligned buffer.
/// Usually implemented in or called from assembly.
///
/// # Safety
///
/// Caller knows it is safe to zero the given virtual address.
pub unsafe fn zero_page(va: VAddr) {
    // SAFETY: it is safe to zero the given address.
    unsafe {
        arch_zero_page(core::ptr::with_exposed_provenance_mut(va.0));
    }
}

/// Returns the architectural CPU feature flags (`ZX_ARM64_FEATURE_*` / `ZX_X86_64_FEATURE_*` /
/// `ZX_RISCV64_FEATURE_*`).
#[inline(always)]
pub fn cpu_features() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_cpu_features()` in arch_ffi.cc,
    // which has no preconditions.
    unsafe { cpp_arch_cpu_features() }
}

/// Returns the architectural SVE CPU feature flags (`ZX_ARM64_FEATURE_ISA_SVE*`, or 0 on
/// non-ARM64).
#[inline(always)]
pub fn cpu_sve_features() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_cpu_sve_features()` in
    // arch_ffi.cc, which has no preconditions.
    unsafe { cpp_arch_cpu_sve_features() }
}

/// Returns the number of hardware breakpoint registers available on the system.
#[inline(always)]
pub fn get_hw_breakpoint_count() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The declaration matches the C++ `uint8_t arch_get_hw_breakpoint_count()`,
        // which has no preconditions.
        unsafe { arch_get_hw_breakpoint_count() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arm64_hw_breakpoint_count()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::debugger::arch_get_hw_breakpoint_count()
    }
}

/// Returns the number of hardware watchpoint registers available on the system.
#[inline(always)]
pub fn get_hw_watchpoint_count() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The declaration matches the C++ `uint8_t arch_get_hw_watchpoint_count()`,
        // which has no preconditions.
        unsafe { arch_get_hw_watchpoint_count() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arm64_hw_watchpoint_count()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::debugger::arch_get_hw_watchpoint_count()
    }
}

/// Returns the architectural address tagging feature flags (`ZX_ARM64_FEATURE_ADDRESS_TAGGING_*`).
#[inline(always)]
pub fn address_tagging_features() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The declaration matches the C++ `uint32_t arch_address_tagging_features()`,
        // which has no preconditions.
        unsafe { arch_address_tagging_features() }
    }
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::arch_address_tagging_features()
    }
    #[cfg(target_arch = "riscv64")]
    {
        super::riscv64::mmu::arch_address_tagging_features()
    }
}

/// Returns the architectural data cache line size in bytes.
#[inline(always)]
pub fn dcache_line_size() -> u32 {
    // SAFETY: `cpp_arch_dcache_line_size` is a pure query of the architectural cache line size.
    unsafe { cpp_arch_dcache_line_size() }
}

/// Returns the architectural instruction cache line size in bytes.
#[inline(always)]
pub fn icache_line_size() -> u32 {
    // SAFETY: The declaration matches the C++ `uint32_t cpp_arch_icache_line_size()` in
    // arch_ffi.cc, which has no preconditions.
    unsafe { cpp_arch_icache_line_size() }
}

/// Cleans the data cache for `[start, start + len)`.
///
/// # Safety
///
/// `[start, start + len)` must be a valid mapped kernel address range for the duration of the call.
#[inline(always)]
pub unsafe fn clean_cache_range(start: *const u8, len: usize) {
    // SAFETY: Caller guarantees `[start, start + len)` is a valid mapped kernel address range.
    // Exposing provenance allows the C/assembly implementation to cast the integer address back to
    // a pointer and observe prior stores.
    unsafe {
        arch_clean_cache_range(start.expose_provenance(), len);
    }
}

/// Returns the architecture-specific VM feature flags (`ZX_VM_FEATURE_*`).
#[inline(always)]
pub fn arch_vm_features() -> u32 {
    // SAFETY: `cpp_arch_vm_features` is a pure query of architectural VM features.
    unsafe { cpp_arch_vm_features() }
}

/// Returns the number of spinlocks currently held on the calling CPU.
#[inline(always)]
pub fn num_spinlocks_held() -> u32 {
    // SAFETY: `cpp_arch_num_spinlocks_held` is a pure query of per-CPU spinlock state.
    unsafe { cpp_arch_num_spinlocks_held() }
}
