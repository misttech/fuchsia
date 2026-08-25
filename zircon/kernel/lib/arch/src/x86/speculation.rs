// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::cpuid::{EXTENDED_AMD_FEATURES_B, EXTENDED_FEATURES_D};
use bitrs::layout;
use regio::RwSafe;
use regio::x86::{Cpuid, Msr};

/// Speculation control.
///
/// [intel/vol4]: Table 2-2.  IA-32 Architectural MSRs (Contd.).
/// [amd/ibc]: PRESENCE.
/// [amd/ssbd]: PRESENCE.
pub const SPECULATION_CONTROL: Msr<0x0000_0048, SpeculationControlMsr, RwSafe> = Msr::new();

layout!({
    /// Layout for [`SPECULATION_CONTROL`].
    pub struct SpeculationControlMsr(u64);
    {
        let __ @ 63..3;
        let ssbd @ 2;
        let stibp @ 1;
        let ibrs @ 0;
    }
});

impl SpeculationControlMsr {
    pub fn is_supported(cpuid: impl Cpuid) -> bool {
        // Intel documents that the MSR is supported only if one of the kinds of
        // speculation it can control is itself enumerated; AMD does similarly, but
        // that information must be cobbled together from [amd/ibc] and [amd/ssbd].

        // The Intel way:
        let intel_features = cpuid.read(EXTENDED_FEATURES_D);
        if intel_features.ibrs_ibpb() || intel_features.stibp() || intel_features.ssbd() {
            return true;
        }

        // The AMD way:
        if !cpuid.supports(EXTENDED_AMD_FEATURES_B) {
            return false;
        }
        let amd_features = cpuid.read(EXTENDED_AMD_FEATURES_B);
        amd_features.ibrs() || amd_features.stibp() || amd_features.ssbd()
    }
}

/// Virtual speculation control (e.g., for hypervisor usage).
///
/// [amd/ssbd]: PRESENCE.
pub const VIRTUAL_SPECULATION_CONTROL: Msr<0xc001_011f, VirtualSpeculationControlMsr, RwSafe> =
    Msr::new();

layout!({
    /// Layout for [`VIRTUAL_SPECULATION_CONTROL`].
    pub struct VirtualSpeculationControlMsr(u64);
    {
        let __ @ 63..3;
        let ssbd @ 2;
        let __ @ 1..0;
    }
});

impl VirtualSpeculationControlMsr {
    pub fn is_supported(cpuid: impl Cpuid) -> bool {
        // [amd/ssbd]: HYPERVISOR USAGE MODELS.
        cpuid.supports(EXTENDED_AMD_FEATURES_B) && cpuid.read(EXTENDED_AMD_FEATURES_B).virt_ssbd()
    }
}
