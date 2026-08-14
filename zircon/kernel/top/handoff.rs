// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Physical handoff interface.

unsafe extern "C" {
    fn cpp_physhandoff_get_smbios_phys(smbios_phys: *mut u64) -> bool;
}

/// Returns the physical address of the SMBIOS entry point table, if available in the phys handoff.
pub fn smbios_phys() -> Option<u64> {
    let mut phys = 0u64;
    // SAFETY: `cpp_physhandoff_get_smbios_phys` writes to `smbios_phys` if valid handoff is present.
    let has_smbios = unsafe { cpp_physhandoff_get_smbios_phys(&mut phys) };
    if has_smbios { Some(phys) } else { None }
}
