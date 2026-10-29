// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::cpu_mask_t;

#[cfg(console_enabled)]
unsafe extern "C" {
    #[link_name = "g_max_paddr_width"]
    pub(super) static G_MAX_PADDR_WIDTH: u8;
}

/// FFI entry point for [`super::mmu_mem_types::mmu_mem_type_init`].
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during early MMU
/// initialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_mmu_mem_type_init() {
    // SAFETY: Caller guarantees early boot single-threaded BSP context with MTRR and PAT support.
    unsafe { super::mmu_mem_types::mmu_mem_type_init() };
}

/// FFI entry point for [`super::mmu_mem_types::pat_sync`].
///
/// # Safety
///
/// The caller must ensure that no CPUs are concurrently hotplugged and
/// [`x86_mmu_mem_type_init`] has already been called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_pat_sync(targets: cpu_mask_t) {
    // SAFETY: Caller guarantees CPU hotplug will not occur concurrently and target CPUs support
    // PAT/MTRR.
    unsafe { super::mmu_mem_types::pat_sync(targets) };
}
