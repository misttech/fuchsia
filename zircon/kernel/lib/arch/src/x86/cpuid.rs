// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::str::Utf8Error;

use bitrs::{bitfield_repr, layout};
use regio::x86::{Cpuid, CpuidValue, EAX, EBX, ECX, EDX};

use super::Vendor;

/// Leaf/Function 0x0, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_EAX Largest Standard Function Number.
pub const MAX_LEAF: CpuidValue<0x0, 0x0, EAX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_B: CpuidValue<0x0, 0x0, EBX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, ECX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_C: CpuidValue<0x0, 0x0, ECX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, EDX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_D: CpuidValue<0x0, 0x0, EDX, u32> = CpuidValue::new();

/// A vendor string derived from CPUID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VendorString([u8; 12]);

impl VendorString {
    /// Intel's vendor string.
    pub const INTEL: Self = Self(*b"GenuineIntel");

    /// AMD's vendor string.
    pub const AMD: Self = Self(*b"AuthenticAMD");

    /// Returns the CPUID-based vendor string.
    pub fn from_cpuid(cpuid: impl Cpuid) -> Self {
        let ebx = cpuid.read(VENDOR_STRING_B).to_le_bytes();
        let ecx = cpuid.read(VENDOR_STRING_C).to_le_bytes();
        let edx = cpuid.read(VENDOR_STRING_D).to_le_bytes();
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

/// Leaf/Function 0x7, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EBX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_B: CpuidValue<0x7, 0x0, EBX, ExtendedFeatureFlagsB> = CpuidValue::new();

layout!({
    /// The layout of EBX in [`EXTENDED_FEATURES_B`].
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

/// Leaf/Function 0x7, ECX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_ECX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_C: CpuidValue<0x7, 0x0, ECX, ExtendedFeatureFlagsC> = CpuidValue::new();

layout!({
    /// The layout of ECX in [`EXTENDED_FEATURES_C`].
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

/// Leaf/Function 0x7, EDX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EDX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_D: CpuidValue<0x7, 0x0, EDX, ExtendedFeatureFlagsD> = CpuidValue::new();

layout!({
    /// The layout of EDX in [`EXTENDED_FEATURES_D`].
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

/// Cache type for Cache Topology leaves.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum CacheType {
    Null = 0,
    Data = 1,
    Instruction = 2,
    Unified = 3,
}

// TODO(https://github.com/rust-lang/rust/issues/113521): These values should
// be generic over subleaf.

/// Leaf/Function 0x4, EAX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const INTEL_CACHE_TOPOLOGY_A: CpuidValue<0x4, 0, EAX, CacheTopologyA> = CpuidValue::new();

/// Leaf/Function 0x8000_001d, EAX (subleaf 0)
///
/// [amd/vol3]: E.4.15  Function 8000_001Dh—Cache Topology Information.
pub const AMD_CACHE_TOPOLOGY_A: CpuidValue<0x8000_001d, 0, EAX, CacheTopologyA> = CpuidValue::new();

layout!({
    /// The layout of EAX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyA(u32);
    {
        let max_cores @ 31..26; // Reserved on AMD.
        let max_sharing_logical_processors @ 25..14;
        let __ @ 13..10;
        let fully_associative @ 9;
        let self_initializing @ 8;
        let cache_level @ 7..5;
        let cache_type @ 4..0: CacheType;
    }
});

layout!({
    /// The layout of EBX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyB(u32);
    {
        let ways @ 31..22;
        let physical_line_partitions @ 21..12;
        let system_coherency_line_size @ 11..0;
    }
});

layout!({
    /// The layout of ECX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyC(u32);
    {
        let sets @ 31..0;
    }
});

layout!({
    /// The layout of EDX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyD(u32);
    {
        let __ @ 31..3;
        let complex_cache_indexing @ 2;
        let inclusive @ 1;
        let wbinvd @ 0;
    }
});

/// Leaf/Function 0x8000_0005, ECX
///
/// [amd/vol3]: E.4.4  Function 8000_0005h — L1 Cache and TLB Information.
pub const AMD_L1_DATA_CACHE_INFO: CpuidValue<0x8000_0005, 0, ECX, AmdL1CacheInformation> =
    CpuidValue::new();

/// Leaf/Function 0x8000_0005, EDX
///
/// [amd/vol3]: E.4.4  Function 8000_0005h — L1 Cache and TLB Information.
pub const AMD_L1_INSTRUCTION_CACHE_INFO: CpuidValue<0x8000_0005, 0, EDX, AmdL1CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of [`AMD_L1_DATA_CACHE_INFO`] and
    /// [`AMD_L1_INSTRUCTION_CACHE_INFO`].
    pub struct AmdL1CacheInformation(u32);
    {
        let size_kb @ 31..24;
        let assoc @ 23..16;
        let lines_per_tag @ 15..8;
        let line_size @ 7..0;
    }
});

impl AmdL1CacheInformation {
    pub const FULLY_ASSOCIATIVE: u8 = 0xff;

    pub fn ways_of_associativity(&self) -> usize {
        if self.assoc() as u8 == Self::FULLY_ASSOCIATIVE { 0 } else { self.assoc() as usize }
    }

    pub fn fully_associative(&self) -> Option<bool> {
        match self.assoc() as u8 {
            0 => None,
            Self::FULLY_ASSOCIATIVE => Some(true),
            _ => Some(false),
        }
    }
}

/// Associativity encoding for L2 and L3 cache information leaves.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum AmdL2L3Associativity {
    Disabled = 0x0,
    DirectMapped = 0x1,
    Ways2 = 0x2,
    Ways3 = 0x3,
    Ways4 = 0x4,
    Ways6 = 0x5,
    Ways8 = 0x6,
    // 0x7 is reserved.
    Ways16 = 0x8,
    SeeLeaf0x8000001d = 0x9,
    Ways32 = 0xa,
    Ways48 = 0xb,
    Ways64 = 0xc,
    Ways96 = 0xd,
    Ways128 = 0xe,
    FullyAssociative = 0xf,
}

impl AmdL2L3Associativity {
    /// Indeterminate if zero.
    pub fn ways_of_associativity(&self) -> usize {
        match self {
            Self::Disabled | Self::SeeLeaf0x8000001d | Self::FullyAssociative => 0,
            Self::DirectMapped => 1,
            Self::Ways2 => 2,
            Self::Ways3 => 3,
            Self::Ways4 => 4,
            Self::Ways6 => 6,
            Self::Ways8 => 8,
            Self::Ways16 => 16,
            Self::Ways32 => 32,
            Self::Ways48 => 48,
            Self::Ways64 => 64,
            Self::Ways96 => 96,
            Self::Ways128 => 128,
        }
    }

    /// Indeterminate if std::nullopt.
    pub fn fully_associative(&self) -> Option<bool> {
        match self {
            Self::Disabled => None,
            Self::FullyAssociative => Some(true),
            _ => Some(false),
        }
    }
}

/// Leaf/Function 0x8000_0006, ECX
///
/// [amd/vol3]: E.4.5  Function 8000_0006h—L2 Cache and TLB and L3 Cache Information.
pub const AMD_L2_CACHE_INFO: CpuidValue<0x8000_0006, 0, ECX, AmdL2CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of ECX in [`AMD_L2_CACHE_INFO`].
    pub struct AmdL2CacheInformation(u32);
    {
        let size_kb @ 31..16;
        let assoc @ 15..12: AmdL2L3Associativity;
        let lines_per_tag @ 11..8;
        let line_size @ 7..0;
    }
});

impl AmdL2CacheInformation {
    pub fn ways_of_associativity(&self) -> usize {
        self.assoc().ways_of_associativity()
    }

    pub fn fully_associative(&self) -> Option<bool> {
        self.assoc().fully_associative()
    }
}

/// Leaf/Function 0x8000_0006, EDX
///
/// [amd/vol3]: E.4.5  Function 8000_0006h—L2 Cache and TLB and L3 Cache Information.
pub const AMD_L3_CACHE_INFO: CpuidValue<0x8000_0006, 0, EDX, AmdL3CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of EDX in [`AMD_L3_CACHE_INFO`].
    pub struct AmdL3CacheInformation(u32);
    {
        let size @ 31..18;
        let __ @ 17..16;
        let assoc @ 15..12: AmdL2L3Associativity;
        let lines_per_tag @ 11..8;
        let line_size @ 7..0;
    }
});

impl AmdL3CacheInformation {
    pub fn ways_of_associativity(&self) -> usize {
        self.assoc().ways_of_associativity()
    }

    pub fn fully_associative(&self) -> Option<bool> {
        self.assoc().fully_associative()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Basic exercise of the above utilities.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn feature_detection() {
        use regio::x86::DirectCpuid;

        let vendor = VendorString::from_cpuid(DirectCpuid {});
        println!("Vendor string: {}", vendor.as_str().unwrap_or("invalid vendor string"));
    }

    #[test]
    fn amd_vendor_string() {
        use regio::testing::x86::FakeCpuid;

        // EBX, ECX, EDX copied verbatim from the manual.
        let mut cpuid = FakeCpuid::new();
        cpuid
            .set(VENDOR_STRING_B, 0x6874_7541)
            .set(VENDOR_STRING_C, 0x444d_4163)
            .set(VENDOR_STRING_D, 0x6974_6e65);
        let vendor_str = VendorString::from_cpuid(&cpuid);
        assert_eq!(vendor_str, VendorString::AMD);
    }
}
