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
    fn cpp_arch_vm_features() -> u32;
    fn cpp_arch_num_spinlocks_held() -> u32;
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
