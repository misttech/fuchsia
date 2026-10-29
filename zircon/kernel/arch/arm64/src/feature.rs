// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! CPU feature detection: the feature globals, the per-CPU cache info, and the feature dump.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

use arm64_feature_bindings::{arm64_asid_width, arm64_microarch};
use debug::dprintf;
use kprint::kprint;
use libarch::arm64::{
    AsidSize, CCSIDR_EL1, CLIDR_EL1, CSSELR_EL1, CTR_EL0, CacheSizeSelectionRegister, DCZID_EL0,
    ID_AA64DFR0_EL1, ID_AA64ISAR0_EL1, ID_AA64ISAR1_EL1, ID_AA64ISAR2_EL1, ID_AA64MMFR0_EL1,
    ID_AA64MMFR1_EL1, ID_AA64MMFR2_EL1, ID_AA64MMFR4_EL1, ID_AA64PFR0_EL1, ID_AA64ZFR0_EL1,
    IdAa64IsaR0Aes, IdAa64IsaR0Atomic, IdAa64IsaR0Crc32, IdAa64IsaR0DotProd, IdAa64IsaR0Fhm,
    IdAa64IsaR0Rdm, IdAa64IsaR0Rndr, IdAa64IsaR0Sha1, IdAa64IsaR0Sha2, IdAa64IsaR0Sha3,
    IdAa64IsaR0Sm3, IdAa64IsaR0Sm4, IdAa64IsaR0Ts, IdAa64IsaR1Dpb, IdAa64IsaR1I8mm,
    IdAa64IsaR2Mops, IdAa64Pfr0El, IdAa64Pfr0Fp, IdAa64Pfr0Sve, IdAa64Zfr0Aes, IdAa64Zfr0Bf16,
    IdAa64Zfr0BitPerm, IdAa64Zfr0F32mm, IdAa64Zfr0I8mm, IdAa64Zfr0Sha3, IdAa64Zfr0Sm4,
    IdAa64Zfr0SveVer, L1ICachePolicy, MIDR_EL1, MainIdRegister, MemoryModelFeatureRegister0,
    MultiprocessorAffinityRegister as Mpidr,
};
use libc::stdio::FILE;
use regio::arm64::{SysReg, spec};
use zx_types::{
    ZX_ARM64_FEATURE_ISA_AES, ZX_ARM64_FEATURE_ISA_ARM32, ZX_ARM64_FEATURE_ISA_ASIMD,
    ZX_ARM64_FEATURE_ISA_ATOMICS, ZX_ARM64_FEATURE_ISA_CRC32, ZX_ARM64_FEATURE_ISA_DP,
    ZX_ARM64_FEATURE_ISA_DPB, ZX_ARM64_FEATURE_ISA_FHM, ZX_ARM64_FEATURE_ISA_FP,
    ZX_ARM64_FEATURE_ISA_I8MM, ZX_ARM64_FEATURE_ISA_MOPS, ZX_ARM64_FEATURE_ISA_PMULL,
    ZX_ARM64_FEATURE_ISA_RDM, ZX_ARM64_FEATURE_ISA_RNDR, ZX_ARM64_FEATURE_ISA_SHA1,
    ZX_ARM64_FEATURE_ISA_SHA3, ZX_ARM64_FEATURE_ISA_SHA256, ZX_ARM64_FEATURE_ISA_SHA512,
    ZX_ARM64_FEATURE_ISA_SM3, ZX_ARM64_FEATURE_ISA_SM4, ZX_ARM64_FEATURE_ISA_SVE,
    ZX_ARM64_FEATURE_ISA_SVE2, ZX_ARM64_FEATURE_ISA_SVEAES, ZX_ARM64_FEATURE_ISA_SVEBF16,
    ZX_ARM64_FEATURE_ISA_SVEBITPERM, ZX_ARM64_FEATURE_ISA_SVEF32MM, ZX_ARM64_FEATURE_ISA_SVEF64MM,
    ZX_ARM64_FEATURE_ISA_SVEI8MM, ZX_ARM64_FEATURE_ISA_SVEPMULL, ZX_ARM64_FEATURE_ISA_SVESHA3,
    ZX_ARM64_FEATURE_ISA_SVESM4, ZX_ARM64_FEATURE_ISA_TS, ZX_HAS_CPU_FEATURES,
};

use super::mp::arch_curr_cpu_num;
use super::{arm64_feature_test, isb};
use crate::kernel::bits::{bit, bits, bits_shift};
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::types::cpu_num_t;

/// Cache size parameters (`struct arm64_cache_features`).
type Arm64CacheFeatures = arm64_feature_bindings::arm64_cache_features;

/// MMU features (`struct arm64_mmu_features`).
type Arm64MmuFeatures = arm64_feature_bindings::arm64_mmu_features;

zr::static_assert!(size_of::<Arm64CacheFeatures>() == 16);
zr::static_assert!(align_of::<Arm64CacheFeatures>() == 4);
zr::static_assert!(size_of::<Arm64MmuFeatures>() == 20);
zr::static_assert!(align_of::<Arm64MmuFeatures>() == 4);

/// Saved instruction set feature bitmap.
#[unsafe(no_mangle)]
pub static mut arm64_isa_features: u32 = 0;

/// SVE subfeature bitmap. Initialized on the boot CPU in `arm64_feature_init` and intersected
/// across secondary CPUs during SMP bringup before userboot populates
/// `DATA_CONSTANTS.features.cpu_sve`. C++ reads it through `ktl::atomic_ref`.
#[unsafe(no_mangle)]
pub static arm64_sve_features: AtomicU32 = AtomicU32::new(0);

/// Whether FEAT_PMUv3 is implemented.
#[unsafe(no_mangle)]
pub static mut feat_pmuv3_enabled: bool = false;

/// MMU features.
#[unsafe(no_mangle)]
pub static mut arm64_mmu_features: Arm64MmuFeatures = Arm64MmuFeatures {
    asid_width: arm64_asid_width::UNKNOWN,
    s1_page_4k: false,
    s1_page_16k: false,
    s1_page_64k: false,
    pan: false,
    uao: false,
    accessed_bit: false,
    dirty_bit: false,
    ccsidx: false,
    vhe: false,
    e2h0: false,
    vmid_width: arm64_asid_width::UNKNOWN,
};

/// Cache size parameters cpus, default to a reasonable minimum.
#[unsafe(no_mangle)]
pub static mut arm64_cache_features: Arm64CacheFeatures = Arm64CacheFeatures {
    zva_size: 32,
    icache_size: 32,
    dcache_size: 32,

    idc: false,
    dic: false,
    pipt: false,
};

/// Save another global version, used directly in some assembly.
#[unsafe(no_mangle)]
pub static mut arm64_dcache_size: u32 = 32;

/// MPIDR_EL1, read as a plain `u64`: only Aff0-Aff3 and bit 0 are used, and
/// `MultiprocessorAffinityRegister::from` would debug-assert the RES1 bit 31.
pub(super) const MPIDR_EL1_RAW: SysReg<spec::MPIDR_EL1, u64> = SysReg::new();

/// `MPIDR_MT` from `<arch/arm64/mp.h>`, which is defined as `(0x1ULL < 24)`: a comparison, so 1.
const MPIDR_MT: u64 = 1;

/// One cache level's instruction or data cache, from CCSIDR_EL1.
#[derive(Clone, Copy)]
struct Arm64CacheDesc {
    ctype: u8,
    num_sets: u32,
    associativity: u32,
    line_size: u32,
}

impl Arm64CacheDesc {
    const ZERO: Self = Self { ctype: 0, num_sets: 0, associativity: 0, line_size: 0 };
}

/// A CPU's cache hierarchy.
struct Arm64CacheInfo {
    // from CLIDR_EL1
    inner_boundary: u8,
    lou_u: u8,
    loc: u8,
    lou_is: u8,
    // from CTR_EL0
    imin_line: u8,
    dmin_line: u8,
    cache_writeback_granule: u8,
    l1_instruction_cache_policy: u8,
    idc: bool, // requires icache invalidate to pou for instruction to data coherence
    dic: bool, // requires data clean to pou for data to instruction coherence
    // via iterating each cache level
    level_data_type: [Arm64CacheDesc; 7],
    level_inst_type: [Arm64CacheDesc; 7],
}

impl Arm64CacheInfo {
    const ZERO: Self = Self {
        inner_boundary: 0,
        lou_u: 0,
        loc: 0,
        lou_is: 0,
        imin_line: 0,
        dmin_line: 0,
        cache_writeback_granule: 0,
        l1_instruction_cache_policy: 0,
        idc: false,
        dic: false,
        level_data_type: [Arm64CacheDesc::ZERO; 7],
        level_inst_type: [Arm64CacheDesc::ZERO; 7],
    };
}

/// Each CPU's cache info, written by that CPU in `arm64_feature_init`.
static mut CACHE_INFO: [Arm64CacheInfo; SMP_MAX_CPUS] = [Arm64CacheInfo::ZERO; SMP_MAX_CPUS];

fn arm64_get_cache_info(info: &mut Arm64CacheInfo) {
    *info = Arm64CacheInfo::ZERO;

    let clidr = CLIDR_EL1.read().bits();
    info.inner_boundary = bits_shift(clidr, 32, 30) as u8;
    info.lou_u = bits_shift(clidr, 29, 27) as u8;
    info.loc = bits_shift(clidr, 26, 24) as u8;
    info.lou_is = bits_shift(clidr, 23, 21) as u8;

    let ctr = CTR_EL0.read().bits();
    info.imin_line = bits(ctr, 3, 0) as u8;
    info.dmin_line = bits_shift(ctr, 19, 16) as u8;
    info.l1_instruction_cache_policy = bits_shift(ctr, 15, 14) as u8;
    info.cache_writeback_granule = bits_shift(ctr, 27, 24) as u8;
    info.idc = bit(ctr, 28) != 0;
    info.dic = bit(ctr, 29) != 0;

    let mut process_ccsid = |level: usize, ctype: u8| {
        let instruction = ctype & 0x1 != 0;

        // Select cache level
        CSSELR_EL1.write(CacheSizeSelectionRegister::select(level as u8, instruction));
        isb();

        let desc = if instruction {
            &mut info.level_inst_type[level]
        } else {
            &mut info.level_data_type[level]
        };
        desc.ctype = ctype;

        let ccsid = CCSIDR_EL1.read().bits();
        // SAFETY: Only CPU 0's `arm64_feature_init` writes the feature globals, before any other
        // CPU starts and before it calls this, so no write races with this read.
        if unsafe { arm64_mmu_features.ccsidx } {
            // Parse the newer extended ccsid format
            desc.num_sets = (bits_shift(ccsid, 55, 32) + 1) as u32;
            desc.associativity = (bits_shift(ccsid, 23, 3) + 1) as u32;
            desc.line_size = 1u32 << (bits(ccsid, 2, 0) + 4);
        } else {
            desc.num_sets = (bits_shift(ccsid, 27, 13) + 1) as u32;
            desc.associativity = (bits_shift(ccsid, 12, 3) + 1) as u32;
            desc.line_size = 1u32 << (bits(ccsid, 2, 0) + 4);
        }
    };

    for i in 0..7 {
        let ctype = ((clidr >> (3 * i)) & 0x07) as u8;
        if ctype == 0 {
            // No more valid ctypes after this
            break;
        }

        if ctype == 4 {
            // Unified
            process_ccsid(i, ctype);
        } else {
            if ctype & 0x2 != 0 {
                // Data cache
                process_ccsid(i, ctype & 0x2);
            }
            if ctype & 0x1 != 0 {
                // Instruction cache
                process_ccsid(i, ctype & 0x1);
            }
        }
    }
}

/// Prints the cache info that CPU `cpu` saved.
///
/// # Safety
///
/// CPU `cpu` has finished `arm64_feature_init` and does not run it again during the call.
unsafe fn arm64_dump_cache_info(cpu: cpu_num_t) {
    let cache_info = &raw const CACHE_INFO;
    // SAFETY: Only CPU `cpu` writes its slot, in `arm64_feature_init`, which the caller guarantees
    // has finished and does not run again while this shared reference lives.
    let info = unsafe { &(*cache_info)[cpu as usize] };
    kprint!("==== ARM64 CACHE INFO CORE {:u} ====\n", cpu);
    kprint!("Inner Boundary = L{:u}\n", info.inner_boundary);
    kprint!("Level of Unification Uniprocessor = L{:u}\n", info.lou_u);
    kprint!("Level of Coherence = L{:u}\n", info.loc);
    kprint!("Level of Unification Inner Shareable = L{:u}\n", info.lou_is);
    kprint!(
        "Instruction/Data cache minimum line = {:u}/{:u}\n",
        (1u32 << info.imin_line) * 4,
        (1u32 << info.dmin_line) * 4
    );
    kprint!("Cache Writeback Granule = {:u}\n", (1u32 << info.cache_writeback_granule) * 4);
    // SAFETY: Only CPU 0's `arm64_feature_init` writes the feature globals, before any other CPU
    // starts; the caller guarantees CPU `cpu` finished it, so CPU 0's has finished too.
    if unsafe { arm64_mmu_features.ccsidx } {
        dprintf!(INFO, "Extended CCSIDR format\n");
    }
    let icp = match info.l1_instruction_cache_policy {
        0 => "VPIPT",
        1 => "AIVIVT",
        2 => "VIPT",
        3 => "PIPT",
        _ => "",
    };
    kprint!("L1 Instruction cache policy = {:s}, ", icp);
    kprint!("IDC = {}, DIC = {}\n", i32::from(info.idc), i32::from(info.dic));
    for (i, (data, inst)) in info.level_data_type.iter().zip(&info.level_inst_type).enumerate() {
        if data.ctype == 0 && inst.ctype == 0 {
            break; // not implemented
        }
        kprint!("L{} Details:", i + 1);
        if data.ctype == 4 {
            kprint!(
                "\tUnified Cache, sets={:u}, associativity={:u}, line size={:u} bytes\n",
                data.num_sets,
                data.associativity,
                data.line_size
            );
        } else {
            if data.ctype & 0x02 != 0 {
                kprint!(
                    "\tData Cache, sets={:u}, associativity={:u}, line size={:u} bytes\n",
                    data.num_sets,
                    data.associativity,
                    data.line_size
                );
            }
            if inst.ctype & 0x01 != 0 {
                if data.ctype & 0x02 != 0 {
                    kprint!("\t");
                }
                kprint!(
                    "\tInstruction Cache, sets={:u}, associativity={:u}, line size={:u} bytes\n",
                    inst.num_sets,
                    inst.associativity,
                    inst.line_size
                );
            }
        }
    }
}

/// Returns the microarchitecture that a MIDR_EL1 value names.
#[unsafe(no_mangle)]
pub extern "C" fn midr_to_microarch(midr: u64) -> arm64_microarch {
    let midr = MainIdRegister::from(midr);
    let implementer = midr.implementer();
    let partnum = midr.part_num();

    if implementer == b'A' {
        // ARM cores
        match partnum {
            0xd01 => arm64_microarch::ARM_CORTEX_A32,
            0xd03 => arm64_microarch::ARM_CORTEX_A53,
            0xd04 => arm64_microarch::ARM_CORTEX_A35,
            0xd05 => arm64_microarch::ARM_CORTEX_A55,
            0xd06 => arm64_microarch::ARM_CORTEX_A65,
            0xd07 => arm64_microarch::ARM_CORTEX_A57,
            0xd08 => arm64_microarch::ARM_CORTEX_A72,
            0xd09 => arm64_microarch::ARM_CORTEX_A73,
            0xd0a => arm64_microarch::ARM_CORTEX_A75,
            0xd0b => arm64_microarch::ARM_CORTEX_A76,
            0xd0c => arm64_microarch::ARM_NEOVERSE_N1,
            0xd0d => arm64_microarch::ARM_CORTEX_A77,
            0xd0e => arm64_microarch::ARM_CORTEX_A76AE,
            0xd40 => arm64_microarch::ARM_NEOVERSE_V1,
            0xd41 => arm64_microarch::ARM_CORTEX_A78,
            0xd42 => arm64_microarch::ARM_CORTEX_A78AE,
            0xd43 => arm64_microarch::ARM_CORTEX_A65AE,
            0xd44 => arm64_microarch::ARM_CORTEX_X1,
            0xd46 => arm64_microarch::ARM_CORTEX_A510,
            0xd47 => arm64_microarch::ARM_CORTEX_A710,
            0xd48 => arm64_microarch::ARM_CORTEX_X2,
            0xd49 => arm64_microarch::ARM_NEOVERSE_N2,
            0xd4a => arm64_microarch::ARM_NEOVERSE_E1,
            0xd4b => arm64_microarch::ARM_CORTEX_A78C,
            0xd4c => arm64_microarch::ARM_CORTEX_X1C,
            0xd4d => arm64_microarch::ARM_CORTEX_A715,
            0xd4e => arm64_microarch::ARM_CORTEX_X3,
            0xd80 => arm64_microarch::ARM_CORTEX_A520,
            0xd81 => arm64_microarch::ARM_CORTEX_A720,
            0xd82 => arm64_microarch::ARM_CORTEX_X4,
            0xd85 => arm64_microarch::ARM_CORTEX_X925,
            0xd87 => arm64_microarch::ARM_CORTEX_A725,
            _ => arm64_microarch::UNKNOWN,
        }
    } else if implementer == b'C' {
        // Cavium
        match partnum {
            0xa1 => arm64_microarch::CAVIUM_CN88XX,
            0xaf => arm64_microarch::CAVIUM_CN99XX,
            _ => arm64_microarch::UNKNOWN,
        }
    } else if implementer == 0x61 {
        // Apple
        // For the moment, qemu via HVF does not seem to return
        // a meaningful part number.
        arm64_microarch::APPLE_UNKNOWN
    } else if implementer == 0xc0 {
        // Ampere
        match partnum {
            0xac3 => arm64_microarch::AMPERE_1,
            0xac4 => arm64_microarch::AMPERE_1A,
            _ => arm64_microarch::AMPERE_UNKNOWN,
        }
    } else if implementer == 0 {
        // software implementation
        match partnum {
            0x51 => arm64_microarch::QEMU_TCG,
            _ => arm64_microarch::UNKNOWN,
        }
    } else {
        arm64_microarch::UNKNOWN
    }
}

/// Formats a value as the kernel printf's `%#lx` does: zero prints as `0`, without the `0x`
/// prefix.
struct AltHex(u64);

impl fmt::Display for AltHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0");
        }
        f.write_str("0x")?;
        fmt::LowerHex::fmt(&self.0, f)
    }
}

/// Displays the core name that `midr_to_core_string` writes for a MIDR_EL1 value.
struct CoreString(u64);

impl fmt::Display for CoreString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        midr_to_core_string(self.0, f)
    }
}

/// Writes the name of the core that a MIDR_EL1 value describes to `out`.
fn midr_to_core_string(midr: u64, out: &mut dyn Write) -> fmt::Result {
    let microarch = midr_to_microarch(midr);
    let midr = MainIdRegister::from(midr);
    let implementer = midr.implementer();
    let variant = midr.variant();
    let partnum = midr.part_num();
    let revision = midr.revision();

    let partnum_str = match microarch {
        arm64_microarch::ARM_CORTEX_A32 => "ARM Cortex-A32",
        arm64_microarch::ARM_CORTEX_A35 => "ARM Cortex-A35",
        arm64_microarch::ARM_CORTEX_A53 => "ARM Cortex-A53",
        arm64_microarch::ARM_CORTEX_A55 => "ARM Cortex-A55",
        arm64_microarch::ARM_CORTEX_A57 => "ARM Cortex-A57",
        arm64_microarch::ARM_CORTEX_A65 => "ARM Cortex-A65",
        arm64_microarch::ARM_CORTEX_A65AE => "ARM Cortex-A65AE",
        arm64_microarch::ARM_CORTEX_A72 => "ARM Cortex-A72",
        arm64_microarch::ARM_CORTEX_A73 => "ARM Cortex-A73",
        arm64_microarch::ARM_CORTEX_A75 => "ARM Cortex-A75",
        arm64_microarch::ARM_CORTEX_A76 => "ARM Cortex-A76",
        arm64_microarch::ARM_CORTEX_A76AE => "ARM Cortex-A76AE",
        arm64_microarch::ARM_CORTEX_A77 => "ARM Cortex-A77",
        arm64_microarch::ARM_CORTEX_A78 => "ARM Cortex-A78",
        arm64_microarch::ARM_CORTEX_A78AE => "ARM Cortex-A78AE",
        arm64_microarch::ARM_CORTEX_A78C => "ARM Cortex-A78C",
        arm64_microarch::ARM_CORTEX_A510 => "ARM Cortex-A510",
        arm64_microarch::ARM_CORTEX_A520 => "ARM Cortex-A520",
        arm64_microarch::ARM_CORTEX_A710 => "ARM Cortex-A710",
        arm64_microarch::ARM_CORTEX_A715 => "ARM Cortex-A715",
        arm64_microarch::ARM_CORTEX_A720 => "ARM Cortex-A720",
        arm64_microarch::ARM_CORTEX_A725 => "ARM Cortex-A725",
        arm64_microarch::ARM_CORTEX_X1 => "ARM Cortex-X1",
        arm64_microarch::ARM_CORTEX_X1C => "ARM Cortex-X1C",
        arm64_microarch::ARM_CORTEX_X2 => "ARM Cortex-X2",
        arm64_microarch::ARM_CORTEX_X3 => "ARM Cortex-X3",
        arm64_microarch::ARM_CORTEX_X4 => "ARM Cortex-X4",
        arm64_microarch::ARM_CORTEX_X925 => "ARM Cortex-X925",
        arm64_microarch::ARM_NEOVERSE_E1 => "ARM Neoverse E1",
        arm64_microarch::ARM_NEOVERSE_N1 => "ARM Neoverse N1",
        arm64_microarch::ARM_NEOVERSE_N2 => "ARM Neoverse N2",
        arm64_microarch::ARM_NEOVERSE_V1 => "ARM Neoverse V1",
        arm64_microarch::CAVIUM_CN88XX => "Cavium CN88XX",
        arm64_microarch::CAVIUM_CN99XX => "Cavium CN99XX",
        arm64_microarch::APPLE_UNKNOWN => "Unknown Apple Silicon",
        arm64_microarch::AMPERE_UNKNOWN => "Unkown Ampere CPU",
        arm64_microarch::AMPERE_1 => "Ampere 1",
        arm64_microarch::AMPERE_1A => "Ampere 1A",
        arm64_microarch::QEMU_TCG => "QEMU TCG",
        arm64_microarch::UNKNOWN => {
            // Printable ASCII, as `isprint` tests for.
            let i = if (0x20..0x7f).contains(&implementer) { char::from(implementer) } else { '0' };
            return write!(
                out,
                "Unknown implementer {} partnum {} r{}p{}",
                i,
                AltHex(partnum.into()),
                variant,
                revision
            );
        }
    };

    write!(out, "{} r{}p{}", partnum_str, variant, revision)
}

/// The boot CPU's SVE subfeatures, which secondary CPUs compare their own against.
static BOOT_CPU_SVE_FEATURES: AtomicU32 = AtomicU32::new(0);

/// This CPU's SVE subfeatures, as `ZX_ARM64_FEATURE_ISA_SVE*` bits.
fn read_sve_features() -> u32 {
    let mut sve_features = 0;
    let zfr0 = ID_AA64ZFR0_EL1.read();
    // Note: FEAT_F64MM (zfr0.f64mm() / ZX_ARM64_FEATURE_ISA_SVEF64MM) is not advertised here
    // because all FEAT_F64MM instructions require CurrentVL >= 256 bits and are UNDEFINED when
    // CurrentVL == 128 bits (ZCR_EL1.LEN == 0).
    if !matches!(zfr0.try_f32mm(), Ok(IdAa64Zfr0F32mm::None)) {
        sve_features |= ZX_ARM64_FEATURE_ISA_SVEF32MM;
    }
    if !matches!(zfr0.try_i8mm(), Ok(IdAa64Zfr0I8mm::None)) {
        sve_features |= ZX_ARM64_FEATURE_ISA_SVEI8MM;
    }
    if !matches!(zfr0.try_bf16(), Ok(IdAa64Zfr0Bf16::None)) {
        sve_features |= ZX_ARM64_FEATURE_ISA_SVEBF16;
    }
    if !matches!(zfr0.try_sve_ver(), Ok(IdAa64Zfr0SveVer::Sve)) {
        sve_features |= ZX_ARM64_FEATURE_ISA_SVE2;
        if !matches!(zfr0.try_sm4(), Ok(IdAa64Zfr0Sm4::None)) {
            sve_features |= ZX_ARM64_FEATURE_ISA_SVESM4;
        }
        if !matches!(zfr0.try_sha3(), Ok(IdAa64Zfr0Sha3::None)) {
            sve_features |= ZX_ARM64_FEATURE_ISA_SVESHA3;
        }
        if !matches!(zfr0.try_bit_perm(), Ok(IdAa64Zfr0BitPerm::None)) {
            sve_features |= ZX_ARM64_FEATURE_ISA_SVEBITPERM;
        }
        match zfr0.try_aes() {
            Err(_) | Ok(IdAa64Zfr0Aes::Pmull) | Ok(IdAa64Zfr0Aes::Aes2) => {
                sve_features |= ZX_ARM64_FEATURE_ISA_SVEPMULL;
                sve_features |= ZX_ARM64_FEATURE_ISA_SVEAES;
            }
            Ok(IdAa64Zfr0Aes::Aes) => sve_features |= ZX_ARM64_FEATURE_ISA_SVEAES,
            Ok(IdAa64Zfr0Aes::None) => {}
        }
    }
    sve_features
}

#[inline]
fn arm64_sve_feature_test(feature: u32) -> bool {
    (arm64_sve_features.load(Ordering::Relaxed) & feature) != 0
}

/// Returns true if the current CPU is the first member of a cluster according to MPIDR.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_feature_current_is_first_in_cluster() -> bool {
    let mpidr = MPIDR_EL1_RAW.read();

    // Note: this test is not strictly correct, since the affinity fields really do not encode the
    // cluster layout of the cores anymore. Since about cortex-a75 and somewhere in the mid armv8
    // spec, the layout of these fields has been somewhat retconned to mean whatever the vendor
    // wants, and true cluster layout comes from external sources (such as device tree). However,
    // the MT bit says whether or not AFF0 is purely listing threads within a cpu, and all new cores
    // have it set all the time, even if they're not SMT capable.
    if mpidr & MPIDR_MT != 0 {
        return ((mpidr & Mpidr::AFF1_MASK) >> Mpidr::AFF1_SHIFT) == 0;
    }
    ((mpidr & Mpidr::AFF0_MASK) >> Mpidr::AFF0_SHIFT) == 0
}

/// Call on every cpu to save features.
///
/// # Safety
///
/// Each CPU calls this as it starts, before anything reads its cache info, and CPU 0 calls it
/// before any other CPU starts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_feature_init() {
    // set up some global constants based on the boot cpu
    let cpu = arch_curr_cpu_num();
    if cpu == 0 {
        // SAFETY: This block is the only writer of the feature globals, and CPU 0 runs it before
        // any other CPU starts, so nothing accesses them concurrently.
        let mut cache_features = unsafe { arm64_cache_features };

        // read the block size of DC ZVA
        let dczid = DCZID_EL0.read();
        cache_features.zva_size = 0;
        if !dczid.dzp() {
            cache_features.zva_size = dczid.zva_line_size() as u32;
        }
        assert!(cache_features.zva_size > 0);

        // Read the dcache and icache line size
        let ctr = CTR_EL0.read();
        cache_features.dcache_size = ctr.dcache_line_size() as u32;
        cache_features.icache_size = ctr.icache_line_size() as u32;
        // SAFETY: Nothing else accesses the feature globals now; see above.
        unsafe { arm64_dcache_size = cache_features.dcache_size };

        // Read instruction and data cache coherence feature bits.
        cache_features.dic = ctr.dic();
        cache_features.idc = ctr.idc();

        // Read the icache type. Note: according to the spec only VIPT and PIPT are valid options,
        // so just store whether or not it's PIPT and if a new value appears, fall back
        // to assuming VIPT.
        cache_features.pipt = matches!(ctr.try_l1_ip(), Ok(L1ICachePolicy::Pipt));

        // SAFETY: Nothing else accesses the feature globals now; see above.
        unsafe { arm64_cache_features = cache_features };

        // parse the ISA feature bits
        // SAFETY: Nothing else accesses the feature globals now; see above.
        let mut isa_features = unsafe { arm64_isa_features };
        isa_features |= ZX_HAS_CPU_FEATURES;

        let isar0 = ID_AA64ISAR0_EL1.read();

        // By D13.1.3 "Principles of the ID scheme for fields in ID registers", it
        // is safe to assume that values assigned in the future will describe
        // supersets of the existing options.
        match isar0.try_aes() {
            Err(_) | Ok(IdAa64IsaR0Aes::Pmull) => {
                isa_features |= ZX_ARM64_FEATURE_ISA_PMULL;
                isa_features |= ZX_ARM64_FEATURE_ISA_AES;
            }
            Ok(IdAa64IsaR0Aes::Aes) => {
                isa_features |= ZX_ARM64_FEATURE_ISA_AES;
            }
            Ok(IdAa64IsaR0Aes::None) => {}
        }

        if !matches!(isar0.try_sha1(), Ok(IdAa64IsaR0Sha1::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_SHA1;
        }
        match isar0.try_sha2() {
            Err(_) | Ok(IdAa64IsaR0Sha2::Sha512) => {
                isa_features |= ZX_ARM64_FEATURE_ISA_SHA512;
                isa_features |= ZX_ARM64_FEATURE_ISA_SHA256;
            }
            Ok(IdAa64IsaR0Sha2::Sha256) => {
                isa_features |= ZX_ARM64_FEATURE_ISA_SHA256;
            }
            Ok(IdAa64IsaR0Sha2::None) => {}
        }
        if !matches!(isar0.try_crc32(), Ok(IdAa64IsaR0Crc32::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_CRC32;
        }
        if !matches!(isar0.try_atomic(), Ok(IdAa64IsaR0Atomic::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_ATOMICS;
        }
        if !matches!(isar0.try_rdm(), Ok(IdAa64IsaR0Rdm::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_RDM;
        }
        if !matches!(isar0.try_sha3(), Ok(IdAa64IsaR0Sha3::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_SHA3;
        }
        if !matches!(isar0.try_sm3(), Ok(IdAa64IsaR0Sm3::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_SM3;
        }
        if !matches!(isar0.try_sm4(), Ok(IdAa64IsaR0Sm4::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_SM4;
        }
        if !matches!(isar0.try_dp(), Ok(IdAa64IsaR0DotProd::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_DP;
        }
        if !matches!(isar0.try_fhm(), Ok(IdAa64IsaR0Fhm::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_FHM;
        }
        if !matches!(isar0.try_ts(), Ok(IdAa64IsaR0Ts::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_TS;
        }
        if !matches!(isar0.try_rndr(), Ok(IdAa64IsaR0Rndr::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_RNDR;
        }

        let isar1 = ID_AA64ISAR1_EL1.read();
        if !matches!(isar1.try_dpb(), Ok(IdAa64IsaR1Dpb::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_DPB;
        }
        if !matches!(isar1.try_i8mm(), Ok(IdAa64IsaR1I8mm::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_I8MM;
        }

        let isar2 = ID_AA64ISAR2_EL1.read();
        if !matches!(isar2.try_mops(), Ok(IdAa64IsaR2Mops::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_MOPS;
        }

        let pfr0 = ID_AA64PFR0_EL1.read();
        if matches!(pfr0.try_el0(), Ok(IdAa64Pfr0El::El32)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_ARM32;
        }
        if !matches!(pfr0.try_fp(), Ok(IdAa64Pfr0Fp::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_FP;
        }
        if !matches!(pfr0.try_advsimd(), Ok(IdAa64Pfr0Fp::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_ASIMD;
        }
        if !matches!(pfr0.try_sve(), Ok(IdAa64Pfr0Sve::None)) {
            isa_features |= ZX_ARM64_FEATURE_ISA_SVE;
            let sve_features = read_sve_features();
            BOOT_CPU_SVE_FEATURES.store(sve_features, Ordering::Relaxed);
            arm64_sve_features.store(sve_features, Ordering::Relaxed);
        }

        // SAFETY: Nothing else accesses the feature globals now; see above.
        unsafe { arm64_isa_features = isa_features };

        // SAFETY: Nothing else accesses the feature globals now; see above.
        let mut mmu_features = unsafe { arm64_mmu_features };
        let mmfr0 = ID_AA64MMFR0_EL1.read();

        // check the size of the asid
        match mmfr0.try_asid_bits() {
            Err(invalid) => {
                kprint!(
                    "ARM: warning, unrecognized ASID width value ({:u}) in ID_AA64MMFR0_EL1\n",
                    invalid.0
                );
                // default to 8 bits
                mmu_features.asid_width = arm64_asid_width::ASID_8;
            }
            Ok(AsidSize::Asid8Bits) => {
                mmu_features.asid_width = arm64_asid_width::ASID_8;
            }
            Ok(AsidSize::Asid16Bits) => {
                mmu_features.asid_width = arm64_asid_width::ASID_16;
            }
        }

        // Read the supported stage 1 page granularities.
        if mmfr0.tgran4() != 0b1111 {
            mmu_features.s1_page_4k = true;
        }
        if mmfr0.tgran16() != 0 {
            mmu_features.s1_page_16k = true;
        }
        if mmfr0.tgran64() != 0b1111 {
            mmu_features.s1_page_64k = true;
        }

        let mmfr1 = ID_AA64MMFR1_EL1.read();

        // See about A and D bits in page tables.
        match mmfr1.hafdbs() {
            2.. => {
                mmu_features.dirty_bit = true;
                mmu_features.accessed_bit = true;
            }
            1 => {
                mmu_features.accessed_bit = true;
            }
            0 => {
                // No A or D feature implemented
            }
        }

        // Check for PAN features
        if mmfr1.pan() != 0 {
            mmu_features.pan = true;

            // >1 values also determine support for various
            // address translation instruction variants with PAN.
        }

        // Check for VHE features.
        if mmfr1.vh() != 0 {
            mmu_features.vhe = true;
        }

        // Check the size of the VMID.
        match mmfr1.try_vmid_bits() {
            Err(_) => {
                // The value printed is ID_AA64MMFR0_EL1's ASID field.
                kprint!(
                    "ARM: warning, unrecognized VMID width value ({:u}) in ID_AA64MMFR1_EL1\n",
                    (mmfr0.bits() & MemoryModelFeatureRegister0::ASID_BITS_MASK)
                        >> MemoryModelFeatureRegister0::ASID_BITS_SHIFT
                );
                // default to 8 bits
                mmu_features.vmid_width = arm64_asid_width::ASID_8;
            }
            Ok(AsidSize::Asid8Bits) => {
                mmu_features.vmid_width = arm64_asid_width::ASID_8;
            }
            Ok(AsidSize::Asid16Bits) => {
                mmu_features.vmid_width = arm64_asid_width::ASID_16;
            }
        }

        let mmfr2 = ID_AA64MMFR2_EL1.read();

        // Check for User Access Override
        if mmfr2.uao() != 0 {
            mmu_features.uao = true;
        }
        if mmfr2.ccidx() != 0 {
            mmu_features.ccsidx = true;
        }

        // Check if FEAT_PMUv3 is enabled.
        let pmu_version = ID_AA64DFR0_EL1.read().pmu_ver();
        // SAFETY: Nothing else accesses the feature globals now; see above.
        unsafe { feat_pmuv3_enabled = pmu_version > 0b0000 && pmu_version < 0b1111 };

        let mmfr4 = ID_AA64MMFR4_EL1.read();

        // Check for E2H0 features.
        if mmfr4.e2h0() == 0 {
            mmu_features.e2h0 = true;
        }

        // SAFETY: Nothing else accesses the feature globals now; see above.
        unsafe { arm64_mmu_features = mmu_features };
    } else {
        let pfr0 = ID_AA64PFR0_EL1.read();
        let has_sve = !matches!(pfr0.try_sve(), Ok(IdAa64Pfr0Sve::None));
        assert!(has_sve == arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE));
        if has_sve {
            // Intersect each secondary CPU's SVE subfeatures into arm64_sve_features before
            // userboot populates the vDSO constants. Compare against the boot-CPU mask, which no
            // secondary CPU writes.
            let cpu_sve_features = read_sve_features();
            let boot_cpu_sve_features = BOOT_CPU_SVE_FEATURES.load(Ordering::Relaxed);
            if cpu_sve_features != boot_cpu_sve_features {
                dprintf!(
                    INFO,
                    "ARM: cpu {} SVE features ({}) differ from boot cpu ({})\n",
                    cpu,
                    AltHex(cpu_sve_features.into()),
                    AltHex(boot_cpu_sve_features.into())
                );
                arm64_sve_features.fetch_and(cpu_sve_features, Ordering::Relaxed);
            }
        }
    }

    // read the cache info for each cpu
    let cache_info = &raw mut CACHE_INFO;
    // SAFETY: `cpu` is the calling CPU, the only writer of its slot. Its one reader,
    // `arm64_dump_cache_info`, runs only once this CPU has finished this call, so nothing else
    // accesses the slot while this exclusive reference lives.
    arm64_get_cache_info(unsafe { &mut (*cache_info)[cpu as usize] });
}

fn print_cpu_info() {
    let midr = MIDR_EL1.read().bits();
    let mpidr = MPIDR_EL1_RAW.read();

    dprintf!(
        INFO,
        "ARM cpu {}: midr {} '{}' mpidr {} aff {}:{}:{}:{}\n",
        arch_curr_cpu_num(),
        AltHex(midr),
        CoreString(midr),
        AltHex(mpidr),
        ((mpidr & Mpidr::AFF3_MASK) >> Mpidr::AFF3_SHIFT) as u32,
        ((mpidr & Mpidr::AFF2_MASK) >> Mpidr::AFF2_SHIFT) as u32,
        ((mpidr & Mpidr::AFF1_MASK) >> Mpidr::AFF1_SHIFT) as u32,
        ((mpidr & Mpidr::AFF0_MASK) >> Mpidr::AFF0_SHIFT) as u32
    );
}

fn print_isa_features() {
    struct Feature {
        bit: u32,
        name: &'static str,
    }
    const FEATURES: [Feature; 22] = [
        Feature { bit: ZX_ARM64_FEATURE_ISA_FP, name: "fp" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_ASIMD, name: "asimd" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_AES, name: "aes" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_PMULL, name: "pmull" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SHA1, name: "sha1" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SHA256, name: "sha256" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SHA512, name: "sha512" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_CRC32, name: "crc32" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_ATOMICS, name: "atomics" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_RDM, name: "rdm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SHA3, name: "sha3" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SM3, name: "sm3" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SM4, name: "sm4" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_DP, name: "dp" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_DPB, name: "dpb" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_FHM, name: "fhm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_TS, name: "ts" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_RNDR, name: "rndr" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_I8MM, name: "i8mm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVE, name: "sve" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_ARM32, name: "arm32" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_MOPS, name: "mops" },
    ];
    const SVE_FEATURES: [Feature; 10] = [
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEI8MM, name: "svei8mm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEF32MM, name: "svef32mm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEF64MM, name: "svef64mm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEBF16, name: "svebf16" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVE2, name: "sve2" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEAES, name: "sveaes" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEPMULL, name: "svepmull" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVEBITPERM, name: "svebitperm" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVESHA3, name: "svesha3" },
        Feature { bit: ZX_ARM64_FEATURE_ISA_SVESM4, name: "svesm4" },
    ];

    let mut line = 0;
    let mut col = 0;
    let mut print_feature = |name: &str| {
        if col == 0 {
            let lead = if line == 0 { "ARM ISA Features: " } else { "                  " };
            kprint!("{:s}", lead);
            col += lead.len();
        }
        kprint!("{:s} ", name);
        col += name.len() + 1;
        if col >= 100 {
            kprint!("\n");
            col = 0;
            line += 1;
        }
    };
    for feature in &FEATURES {
        if arm64_feature_test(feature.bit) {
            print_feature(feature.name);
        }
    }
    for feature in &SVE_FEATURES {
        if arm64_sve_feature_test(feature.bit) {
            print_feature(feature.name);
        }
    }

    if col > 0 {
        kprint!("\n");
    }
}

/// Dumps the feature set; prints additional information if `full` is passed.
///
/// # Safety
///
/// The calling CPU has finished `arm64_feature_init`, and the call stays on that CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_feature_debug(full: bool) {
    print_cpu_info();

    if full {
        print_isa_features();
        // SAFETY: Only CPU 0's `arm64_feature_init` writes the feature globals, before any other
        // CPU starts; the calling CPU has finished its own, so CPU 0's has finished too.
        let (mmu_features, cache_features) = unsafe { (arm64_mmu_features, arm64_cache_features) };
        debug_assert!(mmu_features.asid_width != arm64_asid_width::UNKNOWN);
        dprintf!(
            INFO,
            "ARM ASID width {}\n",
            if mmu_features.asid_width == arm64_asid_width::ASID_16 { "16" } else { "8" }
        );
        dprintf!(
            INFO,
            "ARM Supported S1 Page sizes (4k/16k/64k): {}/{}/{}\n",
            i32::from(mmu_features.s1_page_4k),
            i32::from(mmu_features.s1_page_16k),
            i32::from(mmu_features.s1_page_64k)
        );
        dprintf!(
            INFO,
            "ARM accessed bit {}, dirty bit {}\n",
            i32::from(mmu_features.accessed_bit),
            i32::from(mmu_features.dirty_bit)
        );
        dprintf!(
            INFO,
            "ARM PAN {}, UAO {}\n",
            i32::from(mmu_features.pan),
            i32::from(mmu_features.uao)
        );
        dprintf!(
            INFO,
            "ARM cache line sizes: icache {} dcache {} zva {}\n",
            cache_features.icache_size,
            cache_features.dcache_size,
            cache_features.zva_size
        );
        dprintf!(
            INFO,
            "ARM cache I/D coherence: I2D {} D2I {}\n",
            i32::from(cache_features.idc),
            i32::from(cache_features.dic)
        );
        dprintf!(INFO, "ARM icache type: {}\n", if cache_features.pipt { "PIPT" } else { "VIPT" });
        // The width printed is the ASID width.
        debug_assert!(mmu_features.asid_width != arm64_asid_width::UNKNOWN);
        dprintf!(
            INFO,
            "ARM VMID width {}\n",
            if mmu_features.asid_width == arm64_asid_width::ASID_16 { "16" } else { "8" }
        );
        dprintf!(
            INFO,
            "ARM VHE {}, E2H0 {}\n",
            i32::from(mmu_features.vhe),
            i32::from(mmu_features.e2h0)
        );
        if dprintf::dprintf_enabled(dprintf::INFO) {
            // SAFETY: The argument is the calling CPU, which has finished `arm64_feature_init` and
            // stays the calling CPU for this call, so it does not run that again meanwhile.
            unsafe { arm64_dump_cache_info(arch_curr_cpu_num()) };
        }
    }
}

/// Writes the name of the calling CPU's core to `stream`.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_print_midr_cpu_name(stream: &mut FILE) {
    let midr = MIDR_EL1.read().bits();
    let _ = midr_to_core_string(midr, stream);
}
