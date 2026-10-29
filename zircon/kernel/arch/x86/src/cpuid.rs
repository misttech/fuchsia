// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::arch::x86_64::__cpuid_count;

use LeafIndex::{Leaf1, Leaf6, Leaf7, Leaf8_01, Leaf8_07, Leaf8_21};

#[inline]
const fn extended_leaf(base: u32) -> u32 {
    0x8000_0000 | base
}

#[inline]
const fn base_family_from_eax(eax: u32) -> u16 {
    ((eax >> 8) & 0xf) as u16
}

#[inline]
fn call_cpu_id(leaf: u32, subleaf: u32) -> Registers {
    // Set EAX and ECX to the initial values, call cpuid and copy results into
    // the result object.
    let result = __cpuid_count(leaf, subleaf);
    Registers { reg: [result.eax, result.ebx, result.ecx, result.edx] }
}

/// Raw EAX/EBX/ECX/EDX register values returned by a single `cpuid` invocation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Registers {
    /// Raw register array indexed by `EAX`, `EBX`, `ECX`, `EDX`.
    pub reg: [u32; 4],
}

impl Registers {
    /// Index of `EAX` in `reg`.
    pub const EAX: usize = 0;
    /// Index of `EBX` in `reg`.
    pub const EBX: usize = 1;
    /// Index of `ECX` in `reg`.
    pub const ECX: usize = 2;
    /// Index of `EDX` in `reg`.
    pub const EDX: usize = 3;

    /// Returns the `EAX` value.
    #[inline]
    pub const fn eax(&self) -> u32 {
        self.reg[Self::EAX]
    }

    /// Returns the `EBX` value.
    #[inline]
    pub const fn ebx(&self) -> u32 {
        self.reg[Self::EBX]
    }

    /// Returns the `EDX` value.
    #[inline]
    pub const fn edx(&self) -> u32 {
        self.reg[Self::EDX]
    }

    /// Returns the `ECX` value.
    #[inline]
    pub const fn ecx(&self) -> u32 {
        self.reg[Self::ECX]
    }
}

/// CPU manufacturer identifier parsed from CPUID leaf 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manufacturer {
    /// GenuineIntel.
    Intel,
    /// AuthenticAMD.
    Amd,
    /// Unrecognized vendor string.
    Other,
}

/// Extracts the manufacturer id string from call with EAX=0.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ManufacturerInfo {
    leaf0: Registers,
    leaf8_0: Registers,
}

impl ManufacturerInfo {
    /// How many chars are in a manufacturer id.
    pub const MANUFACTURER_ID_LENGTH: usize = 12;

    /// Creates a `ManufacturerInfo` from leaf `0` and extended leaf `0x8000_0000`.
    pub const fn new(leaf0: Registers, leaf8_0: Registers) -> Self {
        Self { leaf0, leaf8_0 }
    }

    /// Identifies the CPU manufacturer.
    pub fn manufacturer(&self) -> Manufacturer {
        match &self.manufacturer_id() {
            b"GenuineIntel" => Manufacturer::Intel,
            b"AuthenticAMD" => Manufacturer::Amd,
            _ => Manufacturer::Other,
        }
    }

    /// Returns the 12-byte ASCII manufacturer ID string from CPUID leaf 0.
    pub const fn manufacturer_id(&self) -> [u8; Self::MANUFACTURER_ID_LENGTH] {
        let [b0, b1, b2, b3] = self.leaf0.ebx().to_ne_bytes();
        let [d0, d1, d2, d3] = self.leaf0.edx().to_ne_bytes();
        let [c0, c1, c2, c3] = self.leaf0.ecx().to_ne_bytes();
        [b0, b1, b2, b3, d0, d1, d2, d3, c0, c1, c2, c3]
    }

    /// Highest leaf (EAX parameter to cpuid) that this processor supports.
    pub const fn highest_cpuid_leaf(&self) -> usize {
        self.leaf0.eax() as usize
    }

    /// Highest leaf (EAX parameter to cpuid) that this processor supports in the
    /// extended range (> 0x80000000);
    pub const fn highest_extended_cpuid_leaf(&self) -> usize {
        self.leaf8_0.eax() as usize
    }
}

/// Extracts the processor signature/id from call with EAX=1.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessorId {
    registers: Registers,
}

impl ProcessorId {
    /// Creates a `ProcessorId` from CPUID leaf `1` registers.
    pub const fn new(registers: Registers) -> Self {
        Self { registers }
    }

    /// Stepping, or revision, of this model.
    pub const fn stepping(&self) -> u8 {
        (self.registers.eax() & 0xf) as u8
    }

    /// Model inside of the given family.
    pub const fn model(&self) -> u16 {
        let base = ((self.registers.eax() >> 4) & 0xf) as u16;
        let extended = ((self.registers.eax() >> 16) & 0xf) as u16;

        let family = base_family_from_eax(self.registers.eax());
        if family == 0xf || family == 0x6 { (extended << 4) | base } else { base }
    }

    /// Family of processors to which this chip belongs.
    pub const fn family(&self) -> u16 {
        let base = base_family_from_eax(self.registers.eax());
        let extended = ((self.registers.eax() >> 20) & 0xff) as u16;
        if base == 0xf { base + extended } else { base }
    }

    /// Return the full 32-bit identifier of this chip.
    pub const fn signature(&self) -> u32 {
        self.registers.eax()
    }

    /// APIC ID of the processor on which this object was generated. Note this
    /// struct uses a cached copy of registers so if this object was generated on
    /// a different processor this value could be misleading.
    pub const fn local_apic_id(&self) -> u8 {
        ((self.registers.ebx() >> 24) & 0xff) as u8
    }
}

/// Index into `Features::leaves`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeafIndex {
    /// Feature Information (`EAX=1`).
    Leaf1 = 0,
    /// Thermal and Power Management Leaf (`EAX=6`).
    Leaf6 = 1,
    /// Structured Extended Feature Flags (`EAX=7`).
    Leaf7 = 2,
    /// Extended Processor Info and Feature Bits (`EAX=0x8000_0001`).
    Leaf8_01 = 3,
    /// Advanced Power Management Information (`EAX=0x8000_0007`).
    Leaf8_07 = 4,
    /// AMD defined extended features (`EAX=0x8000_0021`).
    Leaf8_21 = 5,
}

/// Descriptor for a single CPUID feature bit within `Features`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// Index into `Features::leaves`.
    leaf: LeafIndex,
    /// Register index (`Registers::EAX`..=`Registers::EDX`).
    reg: usize,
    /// Bit position (`0..32`).
    bit: u8,
}

/// Extracts feature flags from EAX=1 call and extended feature flags calls.
/// See docs for full listing of possible features, this struct is not
/// comprehensive, things are added as they are required.
///
/// The most recent Intel CPUID bit assignments are in the "Intel® Architecture
/// Instruction Set Extensions and Future Features Programming Reference",
/// <https://software.intel.com/sites/default/files/managed/c5/15/architecture-instruction-set-extensions-programming-reference.pdf>
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    leaves: [Registers; Self::LEAF_COUNT],
}

impl Features {
    const LEAF_COUNT: usize = 6;

    const fn f(leaf: LeafIndex, reg: usize, bit: u8) -> Feature {
        Feature { leaf, reg, bit }
    }

    // Feature "enum".

    pub const FPU: Feature = Self::f(Leaf1, Registers::EDX, 0);
    pub const VME: Feature = Self::f(Leaf1, Registers::EDX, 1);
    pub const DE: Feature = Self::f(Leaf1, Registers::EDX, 2);
    pub const PSE: Feature = Self::f(Leaf1, Registers::EDX, 3);
    pub const TSC: Feature = Self::f(Leaf1, Registers::EDX, 4);
    pub const MSR: Feature = Self::f(Leaf1, Registers::EDX, 5);
    pub const PAE: Feature = Self::f(Leaf1, Registers::EDX, 6);
    pub const MCE: Feature = Self::f(Leaf1, Registers::EDX, 7);
    pub const CX8: Feature = Self::f(Leaf1, Registers::EDX, 8);
    pub const APIC: Feature = Self::f(Leaf1, Registers::EDX, 9);
    pub const SEP: Feature = Self::f(Leaf1, Registers::EDX, 11);
    pub const MTRR: Feature = Self::f(Leaf1, Registers::EDX, 12);
    pub const PGE: Feature = Self::f(Leaf1, Registers::EDX, 13);
    pub const MCA: Feature = Self::f(Leaf1, Registers::EDX, 14);
    pub const CMOV: Feature = Self::f(Leaf1, Registers::EDX, 15);
    pub const PAT: Feature = Self::f(Leaf1, Registers::EDX, 16);
    pub const PSE36: Feature = Self::f(Leaf1, Registers::EDX, 17);
    pub const PSN: Feature = Self::f(Leaf1, Registers::EDX, 18);
    pub const CLFSH: Feature = Self::f(Leaf1, Registers::EDX, 19);
    pub const DS: Feature = Self::f(Leaf1, Registers::EDX, 21);
    pub const ACPI: Feature = Self::f(Leaf1, Registers::EDX, 22);
    pub const MMX: Feature = Self::f(Leaf1, Registers::EDX, 23);
    pub const FXSR: Feature = Self::f(Leaf1, Registers::EDX, 24);
    pub const SSE: Feature = Self::f(Leaf1, Registers::EDX, 25);
    pub const SSE2: Feature = Self::f(Leaf1, Registers::EDX, 26);
    pub const SS: Feature = Self::f(Leaf1, Registers::EDX, 27);
    pub const HTT: Feature = Self::f(Leaf1, Registers::EDX, 28);
    pub const TM: Feature = Self::f(Leaf1, Registers::EDX, 29);
    pub const PBE: Feature = Self::f(Leaf1, Registers::EDX, 31);
    pub const SSE3: Feature = Self::f(Leaf1, Registers::ECX, 0);
    pub const PCLMULQDQ: Feature = Self::f(Leaf1, Registers::ECX, 1);
    pub const DTES64: Feature = Self::f(Leaf1, Registers::ECX, 2);
    pub const MONITOR: Feature = Self::f(Leaf1, Registers::ECX, 3);
    pub const DS_CPL: Feature = Self::f(Leaf1, Registers::ECX, 4);
    pub const VMX: Feature = Self::f(Leaf1, Registers::ECX, 5);
    pub const SMX: Feature = Self::f(Leaf1, Registers::ECX, 6);
    pub const EST: Feature = Self::f(Leaf1, Registers::ECX, 7);
    pub const TM2: Feature = Self::f(Leaf1, Registers::ECX, 8);
    pub const SSSE3: Feature = Self::f(Leaf1, Registers::ECX, 9);
    pub const CNXT_ID: Feature = Self::f(Leaf1, Registers::ECX, 10);
    pub const SDBG: Feature = Self::f(Leaf1, Registers::ECX, 11);
    pub const FMA: Feature = Self::f(Leaf1, Registers::ECX, 12);
    pub const CX16: Feature = Self::f(Leaf1, Registers::ECX, 13);
    pub const XTPR: Feature = Self::f(Leaf1, Registers::ECX, 14);
    pub const PDCM: Feature = Self::f(Leaf1, Registers::ECX, 15);
    pub const PCID: Feature = Self::f(Leaf1, Registers::ECX, 17);
    pub const DCA: Feature = Self::f(Leaf1, Registers::ECX, 18);
    pub const SSE4_1: Feature = Self::f(Leaf1, Registers::ECX, 19);
    pub const SSE4_2: Feature = Self::f(Leaf1, Registers::ECX, 20);
    pub const X2APIC: Feature = Self::f(Leaf1, Registers::ECX, 21);
    pub const MOVBE: Feature = Self::f(Leaf1, Registers::ECX, 22);
    pub const POPCNT: Feature = Self::f(Leaf1, Registers::ECX, 23);
    pub const TSC_DEADLINE: Feature = Self::f(Leaf1, Registers::ECX, 24);
    pub const AES: Feature = Self::f(Leaf1, Registers::ECX, 25);
    pub const XSAVE: Feature = Self::f(Leaf1, Registers::ECX, 26);
    pub const OSXSAVE: Feature = Self::f(Leaf1, Registers::ECX, 27);
    pub const AVX: Feature = Self::f(Leaf1, Registers::ECX, 28);
    pub const F16C: Feature = Self::f(Leaf1, Registers::ECX, 29);
    pub const RDRAND: Feature = Self::f(Leaf1, Registers::ECX, 30);

    pub const TURBO: Feature = Self::f(Leaf6, Registers::EAX, 1);
    pub const HWP: Feature = Self::f(Leaf6, Registers::EAX, 7);
    pub const HWP_PREF: Feature = Self::f(Leaf6, Registers::EAX, 10);
    pub const HWP_PKG: Feature = Self::f(Leaf6, Registers::EAX, 11);
    pub const HWP_REQ_FAST: Feature = Self::f(Leaf6, Registers::EAX, 18);
    pub const MPERFAPERF: Feature = Self::f(Leaf6, Registers::ECX, 0);
    pub const EPB: Feature = Self::f(Leaf6, Registers::ECX, 3);

    pub const FSGSBASE: Feature = Self::f(Leaf7, Registers::EBX, 0);
    pub const SGX: Feature = Self::f(Leaf7, Registers::EBX, 2);
    pub const BMI1: Feature = Self::f(Leaf7, Registers::EBX, 3);
    pub const HLE: Feature = Self::f(Leaf7, Registers::EBX, 4);
    pub const AVX2: Feature = Self::f(Leaf7, Registers::EBX, 5);
    pub const SMEP: Feature = Self::f(Leaf7, Registers::EBX, 7);
    pub const BMI2: Feature = Self::f(Leaf7, Registers::EBX, 8);
    pub const ERMS: Feature = Self::f(Leaf7, Registers::EBX, 9);
    pub const INVPCID: Feature = Self::f(Leaf7, Registers::EBX, 10);
    pub const RTM: Feature = Self::f(Leaf7, Registers::EBX, 11);
    pub const PQM: Feature = Self::f(Leaf7, Registers::EBX, 12);
    pub const PQE: Feature = Self::f(Leaf7, Registers::EBX, 15);
    pub const AVX512F: Feature = Self::f(Leaf7, Registers::EBX, 16);
    pub const AVX512DQ: Feature = Self::f(Leaf7, Registers::EBX, 17);
    pub const RDSEED: Feature = Self::f(Leaf7, Registers::EBX, 18);
    pub const ADX: Feature = Self::f(Leaf7, Registers::EBX, 19);
    pub const SMAP: Feature = Self::f(Leaf7, Registers::EBX, 20);
    pub const AVX512IFMA: Feature = Self::f(Leaf7, Registers::EBX, 21);
    pub const CLFLUSHOPT: Feature = Self::f(Leaf7, Registers::EBX, 23);
    pub const CLWB: Feature = Self::f(Leaf7, Registers::EBX, 24);
    pub const INTEL_PT: Feature = Self::f(Leaf7, Registers::EBX, 25);
    pub const AVX512PF: Feature = Self::f(Leaf7, Registers::EBX, 26);
    pub const AVX512ER: Feature = Self::f(Leaf7, Registers::EBX, 27);
    pub const AVX512CD: Feature = Self::f(Leaf7, Registers::EBX, 28);
    pub const SHA: Feature = Self::f(Leaf7, Registers::EBX, 29);
    pub const AVX512BW: Feature = Self::f(Leaf7, Registers::EBX, 30);
    pub const AVX512VL: Feature = Self::f(Leaf7, Registers::EBX, 31);
    pub const PREFETCHWT1: Feature = Self::f(Leaf7, Registers::ECX, 0);
    pub const AVX512VBMI: Feature = Self::f(Leaf7, Registers::ECX, 1);
    pub const UMIP: Feature = Self::f(Leaf7, Registers::ECX, 2);
    pub const PKU: Feature = Self::f(Leaf7, Registers::ECX, 3);
    pub const AVX512VBMI2: Feature = Self::f(Leaf7, Registers::ECX, 6);
    pub const GFNI: Feature = Self::f(Leaf7, Registers::ECX, 8);
    pub const VAES: Feature = Self::f(Leaf7, Registers::ECX, 9);
    pub const VPCLMULQDQ: Feature = Self::f(Leaf7, Registers::ECX, 10);
    pub const AVX512VNNI: Feature = Self::f(Leaf7, Registers::ECX, 11);
    pub const AVX512BITALG: Feature = Self::f(Leaf7, Registers::ECX, 12);
    pub const AVX512VPOPCNTDQ: Feature = Self::f(Leaf7, Registers::ECX, 14);
    pub const RDPID: Feature = Self::f(Leaf7, Registers::ECX, 22);
    pub const AVX512_4VNNIW: Feature = Self::f(Leaf7, Registers::EDX, 2);
    pub const AVX512_4FMAPS: Feature = Self::f(Leaf7, Registers::EDX, 3);
    pub const MD_CLEAR: Feature = Self::f(Leaf7, Registers::EDX, 10);
    pub const ARCH_CAPABILITIES: Feature = Self::f(Leaf7, Registers::EDX, 29);

    pub const LAHF: Feature = Self::f(Leaf8_01, Registers::ECX, 0);
    pub const RDTSCP: Feature = Self::f(Leaf8_01, Registers::EDX, 27);
    pub const PDPE1GB: Feature = Self::f(Leaf8_01, Registers::EDX, 26);
    pub const XD: Feature = Self::f(Leaf8_01, Registers::EDX, 20);
    pub const SYSCALL: Feature = Self::f(Leaf8_01, Registers::EDX, 11);

    pub const CPB: Feature = Self::f(Leaf8_07, Registers::EDX, 9);

    pub const LFENCE_RDTSC: Feature = Self::f(Leaf8_21, Registers::EAX, 2);

    /// Creates a `Features` set from the 6 feature leaves.
    pub const fn new(
        leaf1: Registers,
        leaf6: Registers,
        leaf7: Registers,
        leaf8_01: Registers,
        leaf8_07: Registers,
        leaf8_21: Registers,
    ) -> Self {
        Self { leaves: [leaf1, leaf6, leaf7, leaf8_01, leaf8_07, leaf8_21] }
    }

    /// Returns whether `feature` is present in the captured leaves.
    #[inline]
    pub fn has_feature(&self, feature: Feature) -> bool {
        debug_assert!(
            (feature.leaf as usize) < Self::LEAF_COUNT
                && feature.reg <= Registers::EDX
                && feature.bit < 32,
            "set: {} reg:{} {} bit: {}",
            feature.leaf as usize,
            feature.reg,
            Registers::EDX,
            feature.bit,
        );
        (self.leaves[feature.leaf as usize].reg[feature.reg] & (1u32 << feature.bit)) != 0
    }

    /// Returns the maximum supported logical processors in a physical package.
    /// This is NOT that same as the number of logical processors present.
    pub const fn max_logical_processors_in_package(&self) -> u8 {
        ((self.leaves[Leaf1 as usize].ebx() >> 16) & 0xff) as u8
    }
}

/// Wraps the CPUID instruction on x86, provides helpers to parse the output and
/// allows unit testing of libraries reading it.
pub trait CpuId {
    /// Reads CPU manufacturer information from leaves `0` and `0x8000_0000`.
    fn read_manufacturer_info(&self) -> ManufacturerInfo;
    /// Return ProcessorId; provides (Extended) Family/Model/Stepping.
    fn read_processor_id(&self) -> ProcessorId;
    /// Reads feature flags across standard and extended feature leaves.
    fn read_features(&self) -> Features;
}

/// Real hardware [`CpuId`] implementation that executes the `cpuid` instruction.
///
/// `RealCpuId` is uncached; every call results in one (or more) invocations of `CPUID`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RealCpuId;

impl CpuId for RealCpuId {
    fn read_manufacturer_info(&self) -> ManufacturerInfo {
        ManufacturerInfo::new(call_cpu_id(0, 0), call_cpu_id(extended_leaf(0), 0))
    }

    fn read_processor_id(&self) -> ProcessorId {
        ProcessorId::new(call_cpu_id(1, 0))
    }

    fn read_features(&self) -> Features {
        let info = self.read_manufacturer_info();
        let max_leaf = info.highest_cpuid_leaf();
        let max_ext_leaf = info.highest_extended_cpuid_leaf();
        let read_basic = |leaf: u32| {
            if (leaf as usize) <= max_leaf { call_cpu_id(leaf, 0) } else { Registers::default() }
        };
        let read_extended = |base: u32| {
            let leaf = extended_leaf(base);
            if (leaf as usize) <= max_ext_leaf {
                call_cpu_id(leaf, 0)
            } else {
                Registers::default()
            }
        };
        Features::new(
            read_basic(1),
            read_basic(6),
            read_basic(7),
            read_extended(1),
            read_extended(7),
            read_extended(0x21),
        )
    }
}

/// Test parsing of cpuid values.
#[cfg(ktest)]
#[unittest::suite(name = "cpuid_rust")]
pub mod tests {
    use kprint::kprintln;
    use unittest::{expect_eq, expect_false, expect_true};

    /// Static CPUID register snapshot for a test processor.
    #[derive(Clone, Copy)]
    pub struct TestDataSet {
        pub features: &'static [Feature],
        pub missing_features: &'static [Feature],
        pub leaf0: Registers,
        pub leaf1: Registers,
        pub leaf6: Registers,
        pub leaf7: Registers,
        pub leaf8_0: Registers,
        pub leaf8_1: Registers,
        pub leaf8_7: Registers,
        pub leaf8_21: Registers,
    }

    /// Fake [`CpuId`] implementation backed by a static [`TestDataSet`].
    #[derive(Clone, Copy)]
    pub struct FakeCpuId {
        pub data: &'static TestDataSet,
        pub features: Features,
    }

    impl FakeCpuId {
        /// Creates a `FakeCpuId` backed by `data`.
        pub const fn new(data: &'static TestDataSet) -> Self {
            Self {
                data,
                features: Features::new(
                    data.leaf1,
                    data.leaf6,
                    data.leaf7,
                    data.leaf8_1,
                    data.leaf8_7,
                    data.leaf8_21,
                ),
            }
        }
    }

    impl CpuId for FakeCpuId {
        fn read_manufacturer_info(&self) -> ManufacturerInfo {
            ManufacturerInfo::new(self.data.leaf0, self.data.leaf8_0)
        }

        fn read_processor_id(&self) -> ProcessorId {
            ProcessorId::new(self.data.leaf1)
        }

        fn read_features(&self) -> Features {
            self.features
        }
    }

    const EMPTY_REGS: Registers = Registers { reg: [0, 0, 0, 0] };

    impl TestDataSet {
        /// An all-zero `TestDataSet` fixture.
        pub const EMPTY: Self = Self {
            features: &[],
            missing_features: &[],
            leaf0: EMPTY_REGS,
            leaf1: EMPTY_REGS,
            leaf6: EMPTY_REGS,
            leaf7: EMPTY_REGS,
            leaf8_0: EMPTY_REGS,
            leaf8_1: EMPTY_REGS,
            leaf8_7: EMPTY_REGS,
            leaf8_21: EMPTY_REGS,
        };
    }

    /// Queried from an Intel Core i5-6260U (NUC6i5SYH)
    pub static TEST_DATA_CORE_I5_6260U: TestDataSet = TestDataSet {
        features: &[
            Features::FPU,
            Features::VME,
            Features::DE,
            Features::PSE,
            Features::TSC,
            Features::MSR,
            Features::PAE,
            Features::MCE,
            Features::CX8,
            Features::APIC,
            Features::SEP,
            Features::MTRR,
            Features::FXSR,
            Features::SSE,
            Features::SSE2,
            Features::SS,
            Features::XD,
            Features::PDPE1GB,
            Features::RDTSCP,
            Features::PCLMULQDQ,
            Features::DTES64,
            Features::MONITOR,
            Features::DS_CPL,
            Features::VMX,
            Features::RDSEED,
            Features::EST,
            Features::TM2,
            Features::SSSE3,
            Features::PDCM,
            Features::PCID,
            Features::SSE4_1,
            Features::MD_CLEAR,
            Features::SSE4_2,
            Features::X2APIC,
            Features::MOVBE,
            Features::POPCNT,
            Features::AES,
            Features::XSAVE,
            Features::AVX,
            Features::F16C,
            Features::RDRAND,
            Features::LAHF,
            Features::BMI1,
            Features::ERMS,
            Features::AVX2,
            Features::SMEP,
            Features::BMI2,
            Features::ADX,
            Features::INVPCID,
            Features::TURBO,
            Features::HWP,
            Features::HWP_PREF,
            Features::EPB,
        ],
        missing_features: &[Features::PSN, Features::AVX512VNNI],
        leaf0: Registers { reg: [0x16, 0x756e6547, 0x6c65746e, 0x49656e69] },
        leaf1: Registers { reg: [0x406e3, 0x100800, 0x7ffafbbf, 0xbfebfbff] },
        leaf6: Registers { reg: [0x4f7, 0x2, 0x9, 0x0] },
        leaf7: Registers { reg: [0x0, 0x29c67af, 0x0, 0x9c002400] },
        leaf8_0: Registers { reg: [0x80000008, 0x0, 0x0, 0x0] },
        leaf8_1: Registers { reg: [0x0, 0x0, 0x121, 0x2c100800] },
        leaf8_7: EMPTY_REGS,
        leaf8_21: EMPTY_REGS,
    };

    /// Queried from a Intel Xeon E5-2690v4.
    pub static TEST_DATA_XEON_2690V4: TestDataSet = TestDataSet {
        features: &[
            Features::FPU,
            Features::VME,
            Features::DE,
            Features::PSE,
            Features::TSC,
            Features::MSR,
            Features::PAE,
            Features::MCE,
            Features::CX8,
            Features::APIC,
            Features::SEP,
            Features::MTRR,
            Features::PGE,
            Features::MCA,
            Features::CMOV,
            Features::PAT,
            Features::PSE36,
            Features::ACPI,
            Features::MMX,
            Features::FSGSBASE,
            Features::FXSR,
            Features::SSE,
            Features::SSE2,
            Features::SS,
            Features::HTT,
            Features::TM,
            Features::PBE,
            Features::SYSCALL,
            Features::XD,
            Features::PDPE1GB,
            Features::RDTSCP,
            Features::PCLMULQDQ,
            Features::DTES64,
            Features::MONITOR,
            Features::DS_CPL,
            Features::VMX,
            Features::SMX,
            Features::EST,
            Features::TM2,
            Features::SSSE3,
            Features::SDBG,
            Features::FMA,
            Features::CX16,
            Features::XTPR,
            Features::PDCM,
            Features::PCID,
            Features::DCA,
            Features::SSE4_1,
            Features::SSE4_2,
            Features::X2APIC,
            Features::MOVBE,
            Features::POPCNT,
            Features::AES,
            Features::XSAVE,
            Features::AVX,
            Features::F16C,
            Features::RDRAND,
            Features::LAHF,
            Features::BMI1,
            Features::HLE,
            Features::AVX2,
            Features::SMEP,
            Features::BMI2,
            Features::ERMS,
            Features::INVPCID,
            Features::RTM,
            Features::RDSEED,
            Features::ADX,
            Features::SMAP,
            Features::INTEL_PT,
        ],
        missing_features: &[Features::PSN, Features::AVX512VNNI],
        leaf0: Registers { reg: [0x14, 0x756E6547, 0x6C65746E, 0x49656E69] },
        leaf1: Registers { reg: [0x406F1, 0x12200800, 0x7FFEFBFF, 0xBFEBFBFF] },
        leaf6: EMPTY_REGS,
        leaf7: Registers { reg: [0x0, 0x21CBFBB, 0x0, 0x9C000000] },
        leaf8_0: Registers { reg: [0x80000008, 0x0, 0x0, 0x0] },
        leaf8_1: Registers { reg: [0x0, 0x0, 0x121, 0x2C100800] },
        leaf8_7: EMPTY_REGS,
        leaf8_21: EMPTY_REGS,
    };

    /// Queried from a AMD ThreadRipper 2970wx.
    pub static TEST_DATA_THREAD_RIPPER_2970WX: TestDataSet = TestDataSet {
        features: &[
            Features::FPU,
            Features::VME,
            Features::DE,
            Features::PSE,
            Features::TSC,
            Features::MSR,
            Features::PAE,
            Features::MCE,
            Features::CX8,
            Features::APIC,
            Features::SEP,
            Features::MTRR,
            Features::PGE,
            Features::MCA,
            Features::CMOV,
            Features::PAT,
            Features::PSE36,
            Features::MMX,
            Features::FSGSBASE,
            Features::FXSR,
            Features::SSE,
            Features::SSE2,
            Features::CPB,
        ],
        missing_features: &[Features::PSN, Features::AVX512VNNI, Features::ACPI, Features::SS],
        leaf0: Registers { reg: [0xD, 0x68747541, 0x444D4163, 0x69746E65] },
        leaf1: Registers { reg: [0x800F82, 0x12300800, 0x7ED8320B, 0x178BFBFF] },
        leaf6: EMPTY_REGS,
        leaf7: Registers { reg: [0x0, 0x209C01A9, 0x0, 0x0] },
        leaf8_0: Registers { reg: [0x8000001F, 0x68747541, 0x444D4163, 0x69746E65] },
        leaf8_1: Registers { reg: [0x800F82, 0x70000000, 0x35C233FF, 0x2FD3FBFF] },
        leaf8_7: Registers { reg: [0x0, 0x1b, 0x0, 0x6799] },
        leaf8_21: EMPTY_REGS,
    };

    /// Queried from "AMD A4-9120C RADEON R4, 5 COMPUTE CORES 2C+3G" (HP Chromebook 14)
    /// 'Stoney Ridge' APU, AMD Excavator CPU
    pub static TEST_DATA_AMD_A4_9120C: TestDataSet = TestDataSet {
        // CPU features we do expect to find.
        features: &[
            Features::FPU,
            Features::VME,
            Features::DE,
            Features::PSE,
            Features::TSC,
            Features::MSR,
            Features::PAE,
            Features::MCE,
            Features::CX8,
            Features::APIC,
            Features::SEP,
            Features::MTRR,
            Features::PGE,
            Features::MCA,
            Features::CMOV,
            Features::PAT,
            Features::PSE36,
            Features::MMX,
            Features::CLFSH,
            Features::FSGSBASE,
            Features::MOVBE,
            Features::MPERFAPERF,
        ],
        // Sample of CPU features we do not expect to find.
        missing_features: &[
            Features::SGX,
            Features::RTM,
            Features::PCID,
            Features::RDPID,
            Features::HWP,
        ],
        leaf0: Registers { reg: [0xd, 0x68747541, 0x444d4163, 0x69746e65] },
        leaf1: Registers { reg: [0x670f00, 0x20800, 0x7ed8320b, 0x178bfbff] },
        leaf6: Registers { reg: [0x0, 0x0, 0x1, 0x0] },
        leaf7: Registers { reg: [0x0, 0x1a9, 0x0, 0x0] },
        leaf8_0: Registers { reg: [0x8000001e, 0x68747541, 0x444d4163, 0x69746e65] },
        leaf8_1: Registers { reg: [0x670f00, 0x40000000, 0x2fabbfff, 0x2fd3fbff] },
        leaf8_7: Registers { reg: [0x0, 0x5, 0x400, 0x37d9] },
        leaf8_21: EMPTY_REGS,
    };

    /// Queried from Intel Celeron J3455 (NUC6CAYH); Apollo Lake NUC (Goldmont)
    pub static TEST_DATA_CELERON_J3455: TestDataSet = TestDataSet {
        features: &[],
        missing_features: &[],
        leaf0: Registers { reg: [0x15, 0x756e6547, 0x6c65746e, 0x49656e69] },
        leaf1: Registers { reg: [0x506c9, 0x2200800, 0x4ff8ebbf, 0xbfebfbff] },
        leaf6: EMPTY_REGS,
        leaf7: Registers { reg: [0x0, 0x2294e283, 0x0, 0x2c000000] },
        leaf8_0: Registers { reg: [0x80000008, 0x0, 0x0, 0x0] },
        leaf8_1: Registers { reg: [0x0, 0x0, 0x101, 0x2c100800] },
        leaf8_7: EMPTY_REGS,
        leaf8_21: EMPTY_REGS,
    };

    /// Queried from a AMD EPYC 7B13.
    pub static TEST_DATA_EPYC_7B13: TestDataSet = TestDataSet {
        features: &[
            Features::FPU,
            Features::VME,
            Features::DE,
            Features::PSE,
            Features::TSC,
            Features::MSR,
            Features::PAE,
            Features::MCE,
            Features::CX8,
            Features::APIC,
            Features::SEP,
            Features::MTRR,
            Features::PGE,
            Features::MCA,
            Features::CMOV,
            Features::PAT,
            Features::PSE36,
            Features::MMX,
            Features::FSGSBASE,
            Features::FXSR,
            Features::SSE,
            Features::SSE2,
            Features::CPB,
            Features::LFENCE_RDTSC,
        ],
        missing_features: &[Features::PSN, Features::AVX512VNNI, Features::ACPI, Features::SS],
        leaf0: Registers { reg: [0x00000010, 0x68747541, 0x444d4163, 0x69746e65] },
        leaf1: Registers { reg: [0x00a00f11, 0x75800800, 0x7efa320b, 0x178bfbff] },
        leaf6: Registers { reg: [0x00000004, 0x00000000, 0x00000001, 0x00000000] },
        leaf7: Registers { reg: [0x00000000, 0x219c97a9, 0x0040069c, 0x00000010] },
        leaf8_0: Registers { reg: [0x80000023, 0x68747541, 0x444d4163, 0x69746e65] },
        leaf8_1: Registers { reg: [0x00a00f11, 0x40000000, 0x75c237ff, 0x2fd3fbff] },
        leaf8_7: Registers { reg: [0x00000000, 0x0000003b, 0x00000000, 0x00006799] },
        leaf8_21: Registers { reg: [0x0101fd3f, 0x00004173, 0x000001fd, 0x000000fe] },
    };

    pub static CPU_ID_CORE_I5_6260U: FakeCpuId = FakeCpuId::new(&TEST_DATA_CORE_I5_6260U);
    pub static CPU_ID_XEON_2690V4: FakeCpuId = FakeCpuId::new(&TEST_DATA_XEON_2690V4);
    pub static CPU_ID_THREAD_RIPPER_2970WX: FakeCpuId =
        FakeCpuId::new(&TEST_DATA_THREAD_RIPPER_2970WX);
    pub static CPU_ID_AMD_A4_9120C: FakeCpuId = FakeCpuId::new(&TEST_DATA_AMD_A4_9120C);
    pub static CPU_ID_CELERON_J3455: FakeCpuId = FakeCpuId::new(&TEST_DATA_CELERON_J3455);
    pub static CPU_ID_EPYC_7B13: FakeCpuId = FakeCpuId::new(&TEST_DATA_EPYC_7B13);

    /// Parse feature flags and logical processor counts from static CPUID data.
    #[test]
    fn test_feature_flags() {
        for cpu in [
            &CPU_ID_CORE_I5_6260U,
            &CPU_ID_XEON_2690V4,
            &CPU_ID_CELERON_J3455,
            &CPU_ID_THREAD_RIPPER_2970WX,
            &CPU_ID_AMD_A4_9120C,
            &CPU_ID_EPYC_7B13,
        ] {
            let data = cpu.data;
            let features = cpu.read_features();

            // Features we know this processor has.
            for &feature in data.features {
                let result = features.has_feature(feature);
                if !result {
                    kprintln!(
                        "Missing Feature: set:{} reg:{} bit:{}",
                        feature.leaf as usize,
                        feature.reg,
                        feature.bit,
                    );
                }
                expect_true!(result);
            }

            // Some features we know it doesn't.
            for &feature in data.missing_features {
                let result = features.has_feature(feature);
                if result {
                    kprintln!(
                        "Extra Feature: set:{} reg:{} bit:{}",
                        feature.leaf as usize,
                        feature.reg,
                        feature.bit,
                    );
                }
                expect_false!(result);
            }
        }

        expect_eq!(32, CPU_ID_XEON_2690V4.read_features().max_logical_processors_in_package());
        expect_eq!(
            48,
            CPU_ID_THREAD_RIPPER_2970WX.read_features().max_logical_processors_in_package()
        );
    }

    /// Parse manufacturer vendor strings and leaf bounds from static CPUID data.
    #[test]
    fn test_manufacturer_info() {
        let intel = CPU_ID_XEON_2690V4.read_manufacturer_info();
        expect_true!(&intel.manufacturer_id() == b"GenuineIntel");
        expect_eq!(Manufacturer::Intel, intel.manufacturer());
        expect_eq!(20usize, intel.highest_cpuid_leaf());
        expect_eq!(0x8000_0008usize, intel.highest_extended_cpuid_leaf());

        let amd = CPU_ID_THREAD_RIPPER_2970WX.read_manufacturer_info();
        expect_true!(&amd.manufacturer_id() == b"AuthenticAMD");
        expect_eq!(Manufacturer::Amd, amd.manufacturer());
        expect_eq!(13usize, amd.highest_cpuid_leaf());
        expect_eq!(0x8000_001Fusize, amd.highest_extended_cpuid_leaf());

        let other = ManufacturerInfo::new(
            Registers { reg: [0x5, 0x3332_3130, 0x3b3a_3938, 0x3736_3534] },
            Registers { reg: [0x8000_0001, 0, 0, 0] },
        );
        expect_true!(&other.manufacturer_id() == b"0123456789:;");
        expect_eq!(Manufacturer::Other, other.manufacturer());
        expect_eq!(5usize, other.highest_cpuid_leaf());
        expect_eq!(0x8000_0001usize, other.highest_extended_cpuid_leaf());
    }

    /// Decode processor family, model, and stepping across extended family/model regimes.
    #[test]
    fn test_processor_id_decoding() {
        let xeon = CPU_ID_XEON_2690V4.read_processor_id();
        expect_eq!(6, xeon.family());
        expect_eq!(79, xeon.model());
        expect_eq!(1, xeon.stepping());

        // Pulled from a Pixelbook 'Google Eve rev8'
        let mut eve_regs = Registers::default();
        eve_regs.reg[Registers::EAX] = 0x806e9;
        let eve = ProcessorId::new(eve_regs);
        expect_eq!(eve.signature(), 0x806e9);
        expect_eq!(eve.family(), 6);
        expect_eq!(eve.model(), 0x8e);
        expect_eq!(eve.stepping(), 9);

        let amd = CPU_ID_THREAD_RIPPER_2970WX.read_processor_id();
        expect_eq!(0x17, amd.family());
        expect_eq!(0x8, amd.model());
        expect_eq!(0x2, amd.stepping());

        let mut legacy_regs = Registers::default();
        legacy_regs.reg[Registers::EAX] = 0x00ff_0523;
        let legacy = ProcessorId::new(legacy_regs);
        expect_eq!(legacy.signature(), 0x00ff_0523);
        expect_eq!(legacy.family(), 5);
        expect_eq!(legacy.model(), 2);
        expect_eq!(legacy.stepping(), 3);
    }
}
