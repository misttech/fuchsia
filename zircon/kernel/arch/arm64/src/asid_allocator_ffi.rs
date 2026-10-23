// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI entry points for the ARM64 ASID allocator.

use super::asid_allocator::{asid_alloc, asid_allocator_init, asid_free};
use zx_status::Status;

/// # Safety
///
/// Must be called at most once during early boot MMU initialization (`arm64_mmu_early_init`) after
/// CPU features have been probed and before any concurrent calls to
/// `rust_arm64_asid_allocator_alloc` or `rust_arm64_asid_allocator_free`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_asid_allocator_init() {
    // SAFETY: Upheld by caller contract.
    unsafe { asid_allocator_init() };
}

/// # Safety
///
/// `out_asid` must be a valid, non-null, properly aligned exclusive `uint16_t&` reference, and
/// `rust_arm64_asid_allocator_init` must have completed prior to this call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_asid_allocator_alloc(out_asid: &mut u16) -> Result<(), Status> {
    *out_asid = asid_alloc()?;
    Ok(())
}

#[unsafe(no_mangle)]
extern "C" fn rust_arm64_asid_allocator_free(asid: u16) -> Result<(), Status> {
    asid_free(asid)
}
