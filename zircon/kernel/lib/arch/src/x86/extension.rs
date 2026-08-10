// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitrs::layout;
use regio::RwSafe;
use regio::traits::{ReadReg, RwSafeReg};
use regio::x86::Msr;

use super::cpuid::{ExtendedFeatureFlagsB, ExtendedFeatureFlagsD};
use super::feature::ArchCapabilitiesMsr;

/// [intel/vol4]: Table 2-2.  IA-32 Architectural MSRs (Contd.).
///
/// TSX (Transactional Synchronization Extension) controls.
pub const IA32_TSX_CTRL: Msr<0x0000_0122, TsxControlMsr, RwSafe> = Msr::new();

layout!({
    /// Layout for [`IA32_TSX_CTRL`].
    pub struct TsxControlMsr(u64);
    {
        let __ @ 63..2;
        let rtm_disable @ 1;
        let tsx_cpuid_clear @ 0;
    }
});

impl TsxControlMsr {
    fn is_supported<M>(ext_features: ExtendedFeatureFlagsD, msr: &M) -> bool
    where
        M: ReadReg<ArchCapabilitiesMsr>,
    {
        ArchCapabilitiesMsr::is_supported(ext_features) && msr.read().tsx_ctrl()
    }
}

pub fn tsx_is_supported(ext_features: ExtendedFeatureFlagsB) -> bool {
    // [intel/vol3]: 18.3.6.5     Performance Monitoring and Intel® TSX.
    ext_features.hle() || ext_features.rtm()
}

/// Attempts to disable TSX and returns whether it was successful.
pub fn disable_tsx<M1, M2>(
    ext_features: ExtendedFeatureFlagsD,
    arch_capabilities_msr: &M1,
    tsx_control_msr: &M2,
) -> bool
where
    M1: ReadReg<ArchCapabilitiesMsr>,
    M2: RwSafeReg<TsxControlMsr>,
{
    if !TsxControlMsr::is_supported(ext_features, arch_capabilities_msr) {
        return false;
    }

    tsx_control_msr.modify(|val| {
        val.set_rtm_disable(true).set_tsx_cpuid_clear(true);
    });

    true
}
