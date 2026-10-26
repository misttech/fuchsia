// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! 16-bit SMP bootstrap buffer initialization.

use crate::kernel::types::PAddr;

// LINT.IfChange
/// Size of the 16-bit SMP bootstrap buffer in bytes.
pub const X86_BOOTSTRAP16_BUFFER_SIZE: usize = 3 * page::SIZE;
// LINT.ThenChange(//zircon/kernel/arch/x86/include/arch/x86/bootstrap16.h)

unsafe extern "C" {
    /// Initialize the bootstrap16 subsystem by giving it pages to work with.
    ///
    /// # Safety
    ///
    /// `bootstrap_base` must refer to `X86_BOOTSTRAP16_BUFFER_SIZE` bytes of RAM aligned on a page
    /// boundary less than 1M that are available for the OS to use, and this function must be
    /// called at most once during early boot before `x86_bootstrap16_acquire`.
    pub fn x86_bootstrap16_init(bootstrap_base: PAddr);
}
