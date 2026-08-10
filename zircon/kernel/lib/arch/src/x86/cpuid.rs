// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::str::Utf8Error;

use bitrs::layout;
use regio::x86::{Cpuid, CpuidResult};

use super::Vendor;

/// CPUID Leaf 0x0, Subleaf 0x0: Maximum Basic Leaf Number and Vendor ID String.
///
/// - EAX: Maximum Basic Leaf Number.
/// - EBX, EDX, ECX: Vendor ID String.
pub const MAX_LEAF_AND_VENDOR_STRING: Cpuid<0x0, 0x0, u32, u32, u32, u32> = Cpuid::new();

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn max_leaf() -> u32 {
    MAX_LEAF_AND_VENDOR_STRING.read().eax
}

/// A vendor string derived from CPUID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VendorString([u8; 12]);

impl VendorString {
    /// Intel's vendor string.
    pub const INTEL: Self = Self(*b"GenuineIntel");

    /// AMD's vendor string.
    pub const AMD: Self = Self(*b"AuthenticAMD");

    /// Returns the processor's vendor string.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get() -> Self {
        Self::from_cpuid(MAX_LEAF_AND_VENDOR_STRING.read())
    }

    /// Returns the vendor string from leaf 0.
    pub fn from_cpuid(leaf0: CpuidResult<u32, u32, u32, u32>) -> Self {
        let CpuidResult { ebx, ecx, edx, .. } = leaf0;
        let ebx = ebx.to_le_bytes();
        let ecx = ecx.to_le_bytes();
        let edx = edx.to_le_bytes();
        Self([
            ebx[0], ebx[1], ebx[2], ebx[3], //
            edx[0], edx[1], edx[2], edx[3], //
            ecx[0], ecx[1], ecx[2], ecx[3], //
        ])
    }

    /// Returns the vendor identified by this string.
    ///
    /// Returns [`Vendor::Unknown`] when the vendor string does not correspond
    /// to a known vendor.
    pub fn vendor(&self) -> Vendor {
        match self {
            &Self::INTEL => Vendor::Intel,
            &Self::AMD => Vendor::Amd,
            _ => Vendor::Unknown,
        }
    }

    pub fn as_str(&self) -> Result<&str, Utf8Error> {
        str::from_utf8(&self.0)
    }
}

///---------------------------------------------------------------------------//
/// Leaf/Function 0x7.
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6  Function 7h-Structured Extended Feature Identifier
///---------------------------------------------------------------------------//
pub const EXT_FEATURES: Cpuid<
    0x7,
    0x0,
    u32,
    ExtendedFeatureFlagsB,
    ExtendedFeatureFlagsC,
    ExtendedFeatureFlagsD,
> = Cpuid::new();

layout!({
    /// The layout of EBX in [`EXT_FEATURES`].
    ///
    /// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EBX_x0 Structured Extended Feature
    /// Identifiers (ECX=0).
    pub struct ExtendedFeatureFlagsB(u32);
    {
        let avx512vl @ 31;
        let avx512bw @ 30;
        let sha @ 29;
        let avx512cd @ 28;
        let avx512er @ 27;
        let avx512pf @ 26;
        let intel_pt @ 25;
        let clwb @ 24;
        let clflushopt @ 23;
        let __ @ 22;
        let avx512_ifma @ 21;
        let smap @ 20;
        let adx @ 19;
        let rdseed @ 18;
        let avx512dq @ 17;
        let avx512f @ 16;
        let rdt_a @ 15;
        let mpx @ 14;
        let fpu_cs_ds_deprecated @ 13;
        let rdt_m @ 12;
        let rtm @ 11;
        let invpcid @ 10;
        let erms @ 9;
        let bmi2 @ 8;
        let smep @ 7;
        let fdp_excptn_only_x87 @ 6;
        let avx2 @ 5;
        let hle @ 4;
        let bmi1 @ 3;
        let sgx @ 2;
        let tsc_adjust @ 1;
        let fsgsbase @ 0;
    }
});

layout!({
    /// The layout of ECX in [`EXT_FEATURES`].
    ///
    /// [amd/vol3]: E.3.6, CPUID Fn0000_0007_ECX_x0 Structured Extended Feature
    /// Identifiers (ECX=0).
    pub struct ExtendedFeatureFlagsC(u32);
    {
        let pks @ 31;
        let sgx_lc @ 30;
        let __ @ 29;
        let movdir64b @ 28;
        let movdiri @ 27;
        let __ @ 26;
        let cldemote @ 25;
        let __ @ 24;
        let kl @ 23;
        let rdpid @ 22;
        // Bits [21:17] are 'The value of MAWAU used by the BNDLDX and BNDSTX
        // instructions in 64-bit mode.'
        let __ @ 21..17;
        let la57 @ 16;
        let __ @ 15;
        let avx512_vpopcntdq @ 14;
        let tme_en @ 13;
        let avx512_bitalg @ 12;
        let avx512_vnni @ 11;
        let vpclmulqdq @ 10;
        let vaes @ 9;
        let gfni @ 8;
        let cet_ss @ 7;
        let avx512_vbmi2 @ 6;
        let waitpkg @ 5;
        let ospke @ 4;
        let pku @ 3;
        let umip @ 2;
        let avx512_vbmi @ 1;
        let prefetchwt1 @ 0;
    }
});

layout!({
    /// The layout of EDX in [`EXT_FEATURES`].
    ///
    /// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EDX_x0 Structured Extended Feature
    /// Identifiers (ECX=0).
    pub struct ExtendedFeatureFlagsD(u32);
    {
        let ssbd @ 31;
        let ia32_core_capabilities @ 30;
        let ia32_arch_capabilities @ 29;
        let l1d_flush @ 28;
        let stibp @ 27;
        let ibrs_ibpb @ 26;
        let __ @ 25..21;
        let cet_ibt @ 20;
        let __ @ 19;
        let pconfig @ 18;
        let __ @ 17..16;
        let hybrid @ 15;
        let serialize @ 14;
        let __ @ 13..11;
        let md_clear @ 10;
        let __ @ 9;
        let avx512_vp2intersect @ 8;
        let __ @ 7..5;
        let fsrm @ 4;
        let avx512_4fmaps @ 3;
        let avx512_4vnniw @ 2;
        let __ @ 1..0;
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    // Basic exercise of the above utilities.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn feature_detection() {
        let vendor = VendorString::get();
        println!("Vendor string: {}", vendor.as_str().unwrap_or("invalid vendor string"));
    }

    #[test]
    fn amd_vendor_string() {
        // EBX, ECX, EDX copied verbatim from the manual.
        let leaf0 = CpuidResult { eax: 0x7, ebx: 0x6874_7541, ecx: 0x444d_4163, edx: 0x6974_6e65 };
        let vendor_str = VendorString::from_cpuid(leaf0);
        assert_eq!(vendor_str, VendorString::AMD);
    }
}
