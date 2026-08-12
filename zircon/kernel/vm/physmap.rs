// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::{PAddr, VAddr};
use physmap_bindings as bindings;

/// Converts a physical address to a virtual address in the kernel physmap.
pub fn paddr_to_physmap(paddr: PAddr) -> VAddr {
    // SAFETY: FFI call passing physical address to get virtual address in physmap.
    VAddr(unsafe { bindings::cpp_paddr_to_physmap(paddr.0) })
}
