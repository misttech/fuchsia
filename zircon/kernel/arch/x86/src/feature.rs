// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    fn cpp_x86_get_vendor() -> X86VendorList;
    fn cpp_x86_feature_test(bit: X86CpuidBit) -> bool;
}

macro_rules! x86_cpuid_bit {
    ($name:ident,$leaf:ident,$word:expr,$bit:expr) => {
        pub const $name: X86CpuidBit =
            X86CpuidBit { leaf_num: X86CpuidLeafNum::$leaf, word: $word, bit: $bit };
    };
}
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CpuidLeaf {
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
}

zr::static_assert!(core::mem::size_of::<CpuidLeaf>() == 16);
zr::static_assert!(core::mem::align_of::<CpuidLeaf>() == 4);

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86CpuidLeafNum {
    Base = 0,
    ModelFeatures = 0x1,
    CacheV1 = 0x2,
    CacheV2 = 0x4,
    Mon = 0x5,
    ThermalAndPower = 0x6,
    ExtendedFeatureFlags = 0x7,
    PerformanceMonitoring = 0xa,
    Topology = 0xb,
    XSave = 0xd,
    Pt = 0x14,
    Tsc = 0x15,

    /// HypBase
    HypVendor = 0x40000000,
    KvmFeatures = 0x40000001,

    ExtBase = 0x80000000,
    CpuidBrand = 0x80000002,
    AmdTopology = 0x8000001e,
}

zr::static_assert!(core::mem::size_of::<X86CpuidLeafNum>() == 4);
zr::static_assert!(core::mem::align_of::<X86CpuidLeafNum>() == 4);

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86CpuidBit {
    pub leaf_num: X86CpuidLeafNum,
    pub word: u8,
    pub bit: u8,
}

zr::static_assert!(core::mem::size_of::<X86CpuidBit>() == 8);
zr::static_assert!(core::mem::align_of::<X86CpuidBit>() == 4);

#[repr(u32)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum X86VendorList {
    #[default]
    Unknown = 0,
    Intel = 1,
    Amd = 2,
}

zr::static_assert!(core::mem::size_of::<X86VendorList>() == 4);
zr::static_assert!(core::mem::align_of::<X86VendorList>() == 4);

pub fn x86_get_vendor() -> X86VendorList {
    unsafe { cpp_x86_get_vendor() }
}

x86_cpuid_bit! {X86_FEATURE_KVM_PV_CLOCK, KvmFeatures, 0, 3}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_EOI, KvmFeatures, 0, 6}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_IPI, KvmFeatures, 0, 11}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_CLOCK_STABLE, KvmFeatures, 0, 24}

pub fn x86_feature_test(bit: X86CpuidBit) -> bool {
    unsafe { cpp_x86_feature_test(bit) }
}
