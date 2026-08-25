// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This file contains utilities related to probing and mitigating architectural
//! bugs and vulnerabilities.
//!
//! In general, we cannot rely on the official means of enumerating whether a
//! vulnerability is present. For example, it might only be enumerable after
//! certain microcode updates are performed. Accordingly, if we cannot get an
//! definitive "is not vulnerable" from the official means, we fall back to
//! pessimistically assigning vulnerability on the basis of microarchitecture,
//! making implicit reference to the following documents:
//!
//! * Intel:
//!   https://software.intel.com/security-software-guidance/processors-affected-transient-execution-attack-mitigation-product-cpu-model
//!   Discontinued models (e.g., Core 2, Nehalem, and Westmere) are not present
//!   in the table; in those cases, we assume vulnerability by default, unless
//!   otherwise mentions.
//!
//! * AMD: https://www.amd.com/en/corporate/product-security
//!
//! Further pessimistically, we default to assigning vulnerability in the case
//! unknown architectures.
//!
//! For non-architectural MSRs whose fields give workarounds to a specific
//! erratum, if no further qualification is given (e.g., a field name/mnemonic),
//! we name the field "erratum_${ID}_workaround".

use super::cpuid::{
    EXTENDED_AMD_FEATURES_B, EXTENDED_FEATURES_D, Microarchitecture, VERSION_INFO, VendorString,
};
use super::{ArchCapabilitiesMsr, Vendor, has_ibrs, has_stibp, tsx_is_supported};
use regio::traits::ReadReg;
use regio::x86::Cpuid;

/// Whether the CPU is susceptible to swapgs speculation attacks:
/// https://software.intel.com/security-software-guidance/advisory-guidance/speculative-behavior-swapgs-and-segment-registers
///
/// CVE-2019-1125.
pub fn has_x86_swapgs_bug(cpuid: &impl Cpuid) -> bool {
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    match vendor {
        // All Intel CPUs seem to be affected and there is no indication that they
        // intend to fix this.
        Vendor::Unknown | Vendor::Intel => true,
        Vendor::Amd => false,
    }
}

/// Whether the CPU is susceptible to any of the Microarchitectural Data
/// Sampling (MDS) bugs.
///
/// CVE-2018-12126, CVE-2018-12127, CVE-2018-12130, CVE-2019-11091.
pub fn has_x86_mds_bugs(cpuid: &impl Cpuid, msr: &impl ReadReg<ArchCapabilitiesMsr>) -> bool {
    // https://software.intel.com/security-software-guidance/resources/processors-affected-microarchitectural-data-sampling
    if ArchCapabilitiesMsr::is_supported(cpuid) && msr.read().mds_no() {
        return false;
    }
    let version_info = cpuid.read(VERSION_INFO);
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    let march = version_info.microarchitecture(vendor);
    match march {
        Microarchitecture::Unknown
        | Microarchitecture::IntelCore2
        | Microarchitecture::IntelNehalem
        | Microarchitecture::IntelWestmere
        | Microarchitecture::IntelSandyBridge
        | Microarchitecture::IntelIvyBridge
        | Microarchitecture::IntelHaswell
        | Microarchitecture::IntelBroadwell
        | Microarchitecture::IntelSkylake
        | Microarchitecture::IntelSkylakeServer
        | Microarchitecture::IntelCannonLake
        | Microarchitecture::IntelIceLake
        | Microarchitecture::IntelSilvermont
        | Microarchitecture::IntelAirmont => true,

        Microarchitecture::IntelTigerLake
        | Microarchitecture::IntelAlderLake
        | Microarchitecture::IntelRaptorLake
        | Microarchitecture::IntelBonnell
        | Microarchitecture::IntelSaltwell
        | Microarchitecture::IntelGoldmont
        | Microarchitecture::IntelGoldmontPlus
        | Microarchitecture::IntelTremont
        | Microarchitecture::AmdFamilyBulldozer
        | Microarchitecture::AmdFamilyJaguar
        | Microarchitecture::AmdFamilyZen
        | Microarchitecture::AmdFamilyZen3 => false,
    }
}

/// Whether the CPU is susceptible to the TSX Asynchronous Abort (TAA) bug.
///
/// CVE-2019-11135.
pub fn has_x86_taa_bug(cpuid: &impl Cpuid, msr: &impl ReadReg<ArchCapabilitiesMsr>) -> bool {
    // https://software.intel.com/security-software-guidance/advisory-guidance/intel-transactional-synchronization-extensions-intel-tsx-asynchronous-abort
    //
    // A processor is affected by TAA if both of the following are true:
    // * CPU supports TSX (indicated by the HLE or RTM features);
    // * CPU does not enumerate TAA_NO.
    let taa_no = ArchCapabilitiesMsr::is_supported(cpuid) && msr.read().taa_no();
    if !tsx_is_supported(cpuid) || taa_no {
        return false;
    }
    let version_info = cpuid.read(VERSION_INFO);
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    match version_info.microarchitecture(vendor) {
        Microarchitecture::Unknown
        | Microarchitecture::IntelHaswell
        | Microarchitecture::IntelBroadwell
        | Microarchitecture::IntelSkylake
        | Microarchitecture::IntelSkylakeServer
        | Microarchitecture::IntelCannonLake
        | Microarchitecture::IntelIceLake => true,

        /* Does not implement TSX. */
        Microarchitecture::IntelCore2
        /* Does not implement TSX. */
        | Microarchitecture::IntelNehalem
        /* Does not implement TSX. */
        | Microarchitecture::IntelWestmere
        | Microarchitecture::IntelSandyBridge
        | Microarchitecture::IntelIvyBridge
        | Microarchitecture::IntelTigerLake
        | Microarchitecture::IntelAlderLake
        | Microarchitecture::IntelRaptorLake
        | Microarchitecture::IntelBonnell
        | Microarchitecture::IntelSaltwell
        | Microarchitecture::IntelSilvermont
        | Microarchitecture::IntelAirmont
        | Microarchitecture::IntelGoldmont
        | Microarchitecture::IntelGoldmontPlus
        | Microarchitecture::IntelTremont
        | Microarchitecture::AmdFamilyBulldozer
        | Microarchitecture::AmdFamilyJaguar
        | Microarchitecture::AmdFamilyZen
        | Microarchitecture::AmdFamilyZen3 => false,
    }
}

/// Whether the CPU is susceptible to any of the MDS or TAA bugs, which are
/// closely related and similarly mitigated.
pub fn has_x86_mds_taa_bugs(cpuid: &impl Cpuid, msr: &impl ReadReg<ArchCapabilitiesMsr>) -> bool {
    has_x86_mds_bugs(cpuid, msr) || has_x86_taa_bug(cpuid, msr)
}

/// Whether the MDS/TAA bugs can be mitigated, which all make use of the same
/// method (MD_CLEAR):
/// https://software.intel.com/security-software-guidance/deep-dives/deep-dive-intel-analysis-microarchitectural-data-sampling#mitigation4processors
pub fn can_mitigate_x86_mds_taa_bugs(cpuid: &impl Cpuid) -> bool {
    cpuid.read(EXTENDED_FEATURES_D).md_clear()
}

/// Whether the CPU is susceptible to the Speculative Store Bypass (SSB) bug:
/// https://software.intel.com/security-software-guidance/advisory-guidance/speculative-store-bypass
///
/// CVE-2018-3639.
pub fn has_x86_ssb_bug(cpuid: &impl Cpuid, msr: &impl ReadReg<ArchCapabilitiesMsr>) -> bool {
    // Check if the processor explicitly advertises that it is not affected, in
    // both the Intel and AMD ways.
    if ArchCapabilitiesMsr::is_supported(cpuid) && msr.read().ssb_no() {
        return false;
    }
    if cpuid.supports(EXTENDED_AMD_FEATURES_B) && cpuid.read(EXTENDED_AMD_FEATURES_B).ssb_no() {
        return false;
    }
    let version_info = cpuid.read(VERSION_INFO);
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    match version_info.microarchitecture(vendor) {
        Microarchitecture::Unknown
        | Microarchitecture::IntelCore2
        | Microarchitecture::IntelNehalem
        | Microarchitecture::IntelWestmere
        | Microarchitecture::IntelSandyBridge
        | Microarchitecture::IntelIvyBridge
        | Microarchitecture::IntelHaswell
        | Microarchitecture::IntelBroadwell
        | Microarchitecture::IntelSkylake
        | Microarchitecture::IntelSkylakeServer
        | Microarchitecture::IntelCannonLake
        | Microarchitecture::IntelIceLake
        | Microarchitecture::IntelTigerLake
        | Microarchitecture::IntelAlderLake
        | Microarchitecture::IntelRaptorLake
        | Microarchitecture::IntelGoldmont
        | Microarchitecture::IntelGoldmontPlus
        | Microarchitecture::IntelTremont
        | Microarchitecture::AmdFamilyBulldozer
        | Microarchitecture::AmdFamilyJaguar
        | Microarchitecture::AmdFamilyZen
        | Microarchitecture::AmdFamilyZen3 => true,
        Microarchitecture::IntelBonnell
        | Microarchitecture::IntelSaltwell
        | Microarchitecture::IntelSilvermont
        | Microarchitecture::IntelAirmont => false,
    }
}

/// Whether the CPU is susceptible to the Rogue Data Cache Load (Meltdown) bug:
/// https://software.intel.com/security-software-guidance/advisory-guidance/rogue-data-cache-load.
///
/// CVE-2017-5754.
pub fn has_x86_meltdown_bug(cpuid: &impl Cpuid, msr: &impl ReadReg<ArchCapabilitiesMsr>) -> bool {
    // Check if the processor explicitly advertises that it is not affected.
    if ArchCapabilitiesMsr::is_supported(cpuid) && msr.read().rdcl_no() {
        return false;
    }
    let version_info = cpuid.read(VERSION_INFO);
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    let march = version_info.microarchitecture(vendor);
    match march {
        Microarchitecture::Unknown
        | Microarchitecture::IntelCore2
        | Microarchitecture::IntelNehalem
        | Microarchitecture::IntelWestmere
        | Microarchitecture::IntelSandyBridge
        | Microarchitecture::IntelIvyBridge
        | Microarchitecture::IntelHaswell
        | Microarchitecture::IntelBroadwell
        | Microarchitecture::IntelSkylake
        | Microarchitecture::IntelCannonLake => true,
        Microarchitecture::IntelIceLake
        | Microarchitecture::IntelTigerLake
        | Microarchitecture::IntelAlderLake
        | Microarchitecture::IntelRaptorLake
        | Microarchitecture::IntelBonnell
        | Microarchitecture::IntelSaltwell
        | Microarchitecture::IntelSilvermont
        | Microarchitecture::IntelAirmont
        | Microarchitecture::IntelGoldmont
        | Microarchitecture::IntelTremont
        | Microarchitecture::AmdFamilyBulldozer
        | Microarchitecture::AmdFamilyJaguar
        | Microarchitecture::AmdFamilyZen
        | Microarchitecture::AmdFamilyZen3 => false,
        // Special cases from the above table.
        Microarchitecture::IntelSkylakeServer => {
            let info = cpuid.read(VERSION_INFO);
            if info.stepping() >= 0x6 {
                // Cascade Lake server+
                return false;
            }
            true // Skylake server
        }
        Microarchitecture::IntelGoldmontPlus => {
            let info = cpuid.read(VERSION_INFO);
            if info.stepping() == 0x1 {
                // First stepping was suceptable to Meltdown.
                return true;
            }
            false
        }
    }
}

/// An architecturally prescribed mitigation for Spectre v2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpectreV2Mitigation {
    /// Enhanced/always-on IBRS (i.e., IBRS that can be enabled once without
    /// automatic disabling) is preferred alone.
    Ibrs,
    /// IBPB and/or retpoline are recommended.
    IbpbRetpoline,
    /// IBPB and/or retpoline are recommended - and STIPB, though not preferred as
    /// a performant mitigation, is also present and may be used.
    IbpbRetpolineStibp,
}

/// Returns the preferred Spectre v2 mitigation strategy.
pub fn get_preferred_spectre_v2_mitigation(
    cpuid: &impl Cpuid,
    msr: &impl ReadReg<ArchCapabilitiesMsr>,
) -> SpectreV2Mitigation {
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    match vendor {
        Vendor::Unknown => {}
        Vendor::Intel => {
            // https://software.intel.com/security-software-guidance/advisory-guidance/branch-target-injection
            //
            // If enhanced IBRS are supported, it should be used for mitigation
            // instead of retpoline; else retpoline
            if has_ibrs(cpuid, msr, /*always_on_mode=*/ true) {
                return SpectreV2Mitigation::Ibrs;
            }
        }
        Vendor::Amd => {
            // [amd/ibc]: EXTENDED USAGE MODELS.
            // AMD further offers a feature bit to indicate whether IBRS is a
            // preferred mitigation strategy.
            if has_ibrs(cpuid, msr, /*always_on_mode=*/ true)
                && cpuid.supports(EXTENDED_AMD_FEATURES_B)
                && cpuid.read(EXTENDED_AMD_FEATURES_B).prefers_ibrs()
            {
                return SpectreV2Mitigation::Ibrs;
            }
            // [amd/ibc]: USAGE.
            // Though not recommended, STIPB is still a viable mitigation strategy.
            if has_stibp(cpuid, /*always_on_mode=*/ true) {
                return SpectreV2Mitigation::IbpbRetpolineStibp;
            }
        }
    }
    // Retpolines comprise an architecturally agnostic, pure software solution,
    // which makes it a sensible default strategy.
    SpectreV2Mitigation::IbpbRetpoline
}
