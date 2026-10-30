// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::userabi_ffi::{HandoffEndElf, ZERO_FILL};
use crate::arch_rs;
#[cfg(target_arch = "aarch64")]
use crate::arch_rs::arm64::arch_quirks_needs_arm_erratum_858921_mitigation;
use crate::arch_rs::ops;
#[cfg(target_arch = "aarch64")]
use crate::arm_generic_timer::{
    arm_use_phys_timer_in_vdso, platform_usermode_can_access_tick_registers,
};
#[cfg(target_arch = "riscv64")]
use crate::generic_riscv64::timer::platform_usermode_can_access_tick_registers;
use crate::instrumentation::kernel_mapped_vmo::KernelMappedVmo;
#[cfg(target_arch = "aarch64")]
use crate::kernel::deadline::{Deadline, InstantUnknown};
#[cfg(target_arch = "aarch64")]
use crate::kernel::mp;
use crate::object::{InitialMutability, KernelHandle, VmObjectDispatcher};
#[cfg(target_arch = "x86_64")]
use crate::platform_pc::timer::platform_usermode_can_access_tick_registers;
use crate::platform_rs::timer::{
    ticks_per_second, timer_get_boot_ticks_offset, timer_get_mono_ticks_offset,
    timer_get_ticks_to_time_ratio,
};
use crate::top::boot_constants::system_version_string;
use crate::vm::pmm;
use crate::vm::vm_aspace::VmAspaceLockClass;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use affine::Ratio;
use boot_options::BootOptions;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::ptr::{NonNull, from_ref, null_mut};
use core::sync::atomic::{AtomicI64, AtomicPtr, Ordering};
#[cfg(target_arch = "aarch64")]
use debug::dprintf;
use debug::ltracef;
use fbl::RefPtr;
use kalloc::Box;
use ksync::LockToken;
use page::SIZE as PAGE_SIZE;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zr::{static_assert, static_assert_size_and_align};
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_EXECUTE, ZX_RIGHT_WRITE, ZX_VMO_CHILD_SLICE, ZX_VMO_CHILD_SNAPSHOT, zx_rights_t,
    zx_ticks_t,
};

const LOCAL_TRACE: u32 = 0;

// TODO(https://fxbug.dev/517301686): `VdsoFeatures`, `VdsoConstants`, `TimeValues` and
// `TIME_VALUES_VMO_NAME` are hand-written mirrors of `vdso_constants` in
// lib/userabi/vdso-constants.h and of `fasttime::internal::TimeValues` /
// `kTimeValuesVmoName` in lib/fasttime/internal/abi.h, kept in sync by the layout assertions
// below and in userabi_ffi.cc. They should come from bindgen crates owned by those libraries.

/// Bit maps indicating features (the `features` member of [`VdsoConstants`]).
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
struct VdsoFeatures {
    cpu: u32,

    /// Total amount of debug registers available in the system.
    hw_breakpoint_count: u32,
    hw_watchpoint_count: u32,

    /// Bitmask indicating which address tagging features are available.
    address_tagging: u32,

    /// Bitmask for vm related features.
    vm: u32,

    /// Bitmask for arm64 SVE CPU features.
    cpu_sve: u32,
}

static_assert_size_and_align!(VdsoFeatures, 24, 4);
static_assert!(offset_of!(VdsoFeatures, cpu) == 0);
static_assert!(offset_of!(VdsoFeatures, hw_breakpoint_count) == 4);
static_assert!(offset_of!(VdsoFeatures, hw_watchpoint_count) == 8);
static_assert!(offset_of!(VdsoFeatures, address_tagging) == 12);
static_assert!(offset_of!(VdsoFeatures, vm) == 16);
static_assert!(offset_of!(VdsoFeatures, cpu_sve) == 20);

/// This struct contains constants that are initialized by the kernel
/// once at boot time.  From the vDSO code's perspective, they are
/// read-only data that can never change.  Hence, no synchronization is
/// required to read them.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
struct VdsoConstants {
    /// Maximum number of CPUs that might be online during the lifetime
    /// of the booted system.
    max_num_cpus: u32,

    /// Bit map indicating features.  For specific feature bits, see
    /// zircon/features.h.
    /// TODO(https://fxbug.dev/42105279): This struct may need to grow over time as new features
    /// are added and/or supported.  A mask may be needed to indicate which
    /// bits are valid.
    features: VdsoFeatures,

    /// Number of bytes in a data cache line.
    dcache_line_size: u32,

    /// Number of bytes in an instruction cache line.
    icache_line_size: u32,

    /// System page size in bytes. Guaranteed to be a power of 2.
    page_size: u32,

    /// Total amount of physical memory in the system, in bytes.
    physmem: u64,

    /// Actual length of .version_string, not including the NUL terminator.
    version_string_len: u64,
    // A NUL-terminated UTF-8 string returned by zx_system_get_version_string
    // follows immediately in the VMO after this header.
}

static_assert!(offset_of!(VdsoConstants, physmem).is_multiple_of(size_of::<u64>()));
static_assert_size_and_align!(VdsoConstants, 56, 8);
static_assert!(offset_of!(VdsoConstants, max_num_cpus) == 0);
static_assert!(offset_of!(VdsoConstants, features) == 4);
static_assert!(offset_of!(VdsoConstants, dcache_line_size) == 28);
static_assert!(offset_of!(VdsoConstants, icache_line_size) == 32);
static_assert!(offset_of!(VdsoConstants, page_size) == 36);
static_assert!(offset_of!(VdsoConstants, physmem) == 40);
static_assert!(offset_of!(VdsoConstants, version_string_len) == 48);

/// Time values shared with user mode (libfasttime) through the vDSO and the `time_values` VMO.
///
/// All fields except `mono_ticks_offset` are written once, before the structure is mapped into
/// any user process, and are read-only afterwards. `mono_ticks_offset` is only ever accessed
/// atomically, by both the kernel and user mode.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout)]
struct TimeValues {
    /// A version number to check against the version of libfasttime.
    version: u64,

    /// Conversion factor for zx_ticks_get return values to seconds.
    ticks_per_second: zx_ticks_t,

    /// Offset for converting from the raw system timer to boot ticks.
    boot_ticks_offset: zx_ticks_t,

    /// Offset for converting from the raw system timer to monotonic ticks.
    mono_ticks_offset: AtomicI64,

    /// Ratio which relates ticks (zx_ticks_get) to clock monotonic (zx_clock_get_monotonic).
    /// Specifically...
    ///
    /// ClockMono(ticks) = (ticks * N) / D
    ticks_to_time_numerator: u32,
    ticks_to_time_denominator: u32,

    /// True if usermode can access ticks.
    usermode_can_access_ticks: u8,

    /// Whether the A73 errata mitigation must be used when getting ticks.
    /// Should always be false on x86 and RISC-V.
    use_a73_errata_mitigation: u8,

    /// Whether (on ARM64) the physical counter should be sampled instead of the
    /// virtual counter.
    ///
    /// Should always be false on x86 and RISC-V.
    use_pct_instead_of_vct: u8,

    /// Explicitly pad this structure out to an 8 byte boundary and fill this
    /// padding with 0 by default.
    ///
    /// During VDSO initialization, this structure will be instantiated on the
    /// stack and populated with proper values, and then copied into the shared
    /// page which will be used to publish the values to user-mode.  It is
    /// important that all of the bytes of the object have been explicitly
    /// initialized in order to avoid any chance of accidentally leaking
    /// information about the kernel stack at the time of construction.
    _padding: [u8; 5],
}

static_assert_size_and_align!(TimeValues, 48, 8);
static_assert!(offset_of!(TimeValues, version) == 0);
static_assert!(offset_of!(TimeValues, ticks_per_second) == 8);
static_assert!(offset_of!(TimeValues, boot_ticks_offset) == 16);
static_assert!(offset_of!(TimeValues, mono_ticks_offset) == 24);
static_assert!(offset_of!(TimeValues, ticks_to_time_numerator) == 32);
static_assert!(offset_of!(TimeValues, ticks_to_time_denominator) == 36);
static_assert!(offset_of!(TimeValues, usermode_can_access_ticks) == 40);
static_assert!(offset_of!(TimeValues, use_a73_errata_mitigation) == 41);
static_assert!(offset_of!(TimeValues, use_pct_instead_of_vct) == 42);
static_assert!(offset_of!(TimeValues, _padding) == 43);

/// PA_VMO_KERNEL_FILE with this name holds the global instance of the TimeValuesVmo.
const TIME_VALUES_VMO_NAME: &[u8] = b"time_values";

macro_rules! redirect_syscall {
    ($mutator:expr, $symbol:ident, $target:ident) => {
        paste::paste! {
            $mutator.redirect_symbol(
                stringify!($symbol),
                stringify!($target),
                vdso_code::[<VDSO_DYNSYM_ $symbol>],
                vdso_code::[<VDSO_DYNSYM__ $symbol>],
                vdso_code::[<VDSO_CODE_ $target>],
            )
        }
    };
}

/// Block the named zx_* function.  The symbol table entry will
/// become invisible to runtime symbol resolution, and the code of
/// the function will be clobbered with trapping instructions.
macro_rules! block_syscall {
    ($mutator:expr, $symbol:ident) => {
        paste::paste! {
            $mutator.block_symbol(
                stringify!($symbol),
                vdso_code::[<VDSO_ $symbol>],
                vdso_code::[<VDSO_ $symbol _SIZE>],
                vdso_code::[<VDSO_DYNSYM_ $symbol>],
                vdso_code::[<VDSO_DYNSYM__ $symbol>],
            )
        }
    };
}

/// Random attributes in zx fidl files become "categories" of syscalls.
/// For each category, define a function `block_<category>_syscalls` to block all the
/// syscalls in that category.  These functions can be used in
/// `VDso::create_variant` (below) to block a category of syscalls for a particular
/// variant vDSO.
macro_rules! syscall_category {
    ($category:ident, [ $($syscall:ident),* $(,)? ]) => {
        paste::paste! {
            pub(super) fn [<block_ $category _syscalls>](_mutator: &mut VDsoMutator<'_>) {
                $(
                    block_syscall!(_mutator, [<zx_ $syscall>]);
                )*
            }
        }
    };
}

#[allow(dead_code, non_upper_case_globals)]
mod vdso_code {
    // The generated file invokes `syscall_category!`, which expands to `block_syscall!`, whose
    // `vdso_code::...` constant paths must resolve from inside this module too, so the module
    // imports its own name.
    use super::{VDsoMutator, vdso_code};
    include!(rustenv_path::envpath!("VDSO_CODE_PATH"));
}

use vdso_code::{
    block_next_syscalls, block_test_category1_syscalls, block_test_category2_syscalls,
};

const VDSO_CODE_START: u64 = vdso_code::VDSO_CODE_START as u64;
const VDSO_CODE_SIZE: usize = vdso_code::VDSO_CODE_END - vdso_code::VDSO_CODE_START;

const VDSO_MAX_PAGE_SIZE: usize = match usize::from_str_radix(env!("VDSO_MAX_PAGE_SIZE"), 10) {
    Ok(val) => val,
    Err(_) => panic!("Invalid VDSO_MAX_PAGE_SIZE"),
};

// Need to adjust VDSO_MAX_PAGE_SIZE if any of these fail.
static_assert!(VDSO_MAX_PAGE_SIZE >= PAGE_SIZE);
static_assert!(VDSO_MAX_PAGE_SIZE > size_of::<VdsoConstants>());
static_assert!(VDSO_MAX_PAGE_SIZE >= align_of::<VdsoConstants>());

/// This always leaves space for the NUL terminator.
const MAX_VERSION_STRING: usize = VDSO_MAX_PAGE_SIZE - size_of::<VdsoConstants>() - 1;

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct ElfSym {
    info: usize,
    value: usize,
    size: usize,
}

/// Fill with the single-byte HLT instruction, so any place
/// user-mode jumps into this code, it gets a trap.
#[cfg(target_arch = "x86_64")]
type Insn = u8;
/// Trap fill instruction (`hlt`) on x86_64.
#[cfg(target_arch = "x86_64")]
const TRAP_FILL: Insn = 0xf4;

/// Fixed-size instructions.  Use 'brk #1' (what `__builtin_trap()` emits).
#[cfg(target_arch = "aarch64")]
type Insn = u32;
/// Trap fill instruction (`brk #1`) on aarch64.
#[cfg(target_arch = "aarch64")]
const TRAP_FILL: Insn = 0xd4200020;

/// Instructions are 16 or 32 bits.  All zeros is the 16-bit `unimp` instruction.
#[cfg(target_arch = "riscv64")]
type Insn = u16;
/// Trap fill instruction (`unimp`) on riscv64.
#[cfg(target_arch = "riscv64")]
const TRAP_FILL: Insn = 0;

struct VDsoMutator<'a> {
    vmo: &'a RefPtr<VmObject>,
    trap_fill: Box<[Insn]>,
}

impl<'a> VDsoMutator<'a> {
    fn new(vmo: &'a RefPtr<VmObject>) -> Self {
        Self { vmo, trap_fill: Box::empty_slice() }
    }

    fn redirect_symbol(&mut self, from: &str, to: &str, idx1: usize, idx2: usize, value: usize) {
        let [mut sym1, mut sym2] = self.read_symbol_pair(idx1, idx2);

        // Just change the st_value of the symbol.
        sym1.value = value;
        sym2.value = value;
        self.write_symbol(idx1, &sym1);
        self.write_symbol(idx2, &sym2);

        ltracef!("{} -> {} @ {:#x}\n", from, to, value);
    }

    fn block_symbol(&mut self, name: &str, value: usize, size: usize, idx1: usize, idx2: usize) {
        let [mut sym1, mut sym2] = self.read_symbol_pair(idx1, idx2);

        // First change the symbol to have local binding so it can't be resolved.
        // The high nybble is the STB_* bits; STB_LOCAL is 0.
        sym1.info &= 0xf;
        sym2.info &= 0xf;
        self.write_symbol(idx1, &sym1);
        self.write_symbol(idx2, &sym2);

        // Now fill the code region (a whole function) with safely invalid code.
        // This code should never be run, and any attempt to use it should crash.
        // This uses the compile-time st_value and st_size passed in by the
        // block_syscall! macro, in case the symbol table entry's previous value was
        // already from a previous redirect_symbol.
        assert!(value >= vdso_code::VDSO_CODE_START);
        assert!(value + size < vdso_code::VDSO_CODE_END);
        let vmo = self.vmo;
        let status = vmo.write(value as u64, self.get_trap_fill(size));
        assert!(status.is_ok(), "vDSO VMO Write failed: {}", Status::result_into_raw(status));

        ltracef!("{} @ [{:#x}, {:#x})\n", name, value, value + size);
    }

    fn get_trap_fill(&mut self, mut fill_size: usize) -> &[u8] {
        assert!(fill_size.is_multiple_of(size_of::<Insn>()));
        fill_size /= size_of::<Insn>();
        if fill_size > self.trap_fill.len() {
            let mut trap_fill =
                Box::<[Insn]>::try_new_zeroed_slice(fill_size).expect("trap_fill alloc failed");
            for slot in &mut *trap_fill {
                *slot = TRAP_FILL;
            }
            self.trap_fill = trap_fill;
        }
        self.trap_fill[..fill_size].as_bytes()
    }

    fn symtab_address(idx: usize) -> usize {
        assert!(idx < vdso_code::VDSO_DYNSYM_COUNT);
        vdso_code::VDSO_DATA_START_dynsym + (idx * size_of::<ElfSym>())
    }

    fn read_symbol(&self, idx: usize) -> ElfSym {
        let mut buf = [MaybeUninit::uninit(); size_of::<ElfSym>()];
        let status = self.vmo.read(Self::symtab_address(idx) as u64, &mut buf);
        assert!(
            status.is_ok(),
            "vDSO VMO Read failed: {}",
            status.as_ref().unwrap_err().into_raw()
        );
        ElfSym::read_from_bytes(status.unwrap()).unwrap()
    }

    fn read_symbol_pair(&self, idx1: usize, idx2: usize) -> [ElfSym; 2] {
        let sym1 = self.read_symbol(idx1);
        let sym2 = self.read_symbol(idx2);
        assert!(
            sym1.value == sym2.value,
            "dynsym {} vs {} value {:#x} vs {:#x}",
            idx1,
            idx2,
            sym1.value,
            sym2.value
        );
        assert!(
            sym1.size == sym2.size,
            "dynsym {} vs {} size {:#x} vs {:#x}",
            idx1,
            idx2,
            sym1.size,
            sym2.size
        );
        [sym1, sym2]
    }

    fn write_symbol(&self, idx: usize, sym: &ElfSym) {
        let status = self.vmo.write(Self::symtab_address(idx) as u64, sym.as_bytes());
        assert!(status.is_ok(), "vDSO VMO Write failed: {}", Status::result_into_raw(status));
    }
}

/// This is extracted from the vDSO image at build time.
type VdsoBuildIdNote = [u8; vdso_code::VDSO_BUILD_ID_NOTE_SIZE];
const VDSO_BUILD_ID_NOTE: VdsoBuildIdNote = vdso_code::VDSO_BUILD_ID_NOTE_BYTES;

/// That should exactly match the note read from the vDSO image at runtime.
fn check_build_id(vmo: &RefPtr<VmObject>) {
    let mut note = [MaybeUninit::uninit(); size_of::<VdsoBuildIdNote>()];
    let status = vmo.read(vdso_code::VDSO_BUILD_ID_NOTE_ADDRESS as u64, &mut note);
    assert!(status.is_ok(), "vDSO VMO Read failed: {}", status.as_ref().unwrap_err().into_raw());
    assert!(status.unwrap() == VDSO_BUILD_ID_NOTE);
}

/// Fill out the contents of the time_values struct.
fn set_time_values(vmo: &RefPtr<VmObject>) {
    let per_second: zx_ticks_t = ticks_per_second();

    // Grab a copy of the ticks to time ratio; we need this to initialize the
    // constants window.
    let ticks_to_time_ratio: Ratio = timer_get_ticks_to_time_ratio();

    // At this point in time, we absolutely must know the rate that our tick
    // counter is ticking at.  If we don't, then something has gone horribly
    // wrong.
    assert!(per_second != 0);
    assert!(ticks_to_time_ratio.numerator() != 0);
    assert!(ticks_to_time_ratio.denominator() != 0);

    // Check if usermode can access ticks.
    let usermode_can_access_ticks = platform_usermode_can_access_tick_registers()
        && !BootOptions::get().vdso_ticks_get_force_syscall;
    #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
    let mut needs_a73_mitigation = false;
    #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
    let mut use_pct_instead_of_vct = false;
    #[cfg(target_arch = "aarch64")]
    {
        // We only need to install the A73 quirks for zx_ticks_get if we can access ticks from
        // usermode.
        if usermode_can_access_ticks {
            // Before we got here (during an INIT_HOOK run at LK_INIT_LEVEL_USER - 1),
            // we should have already waited for all of the CPUs in the system to have
            // started up and checked in.
            //
            // Now that all CPUs have started, it should be safe to check whether or not
            // we need to deploy the ARM A73 timer read mitigation. In the case that the
            // CPUs did not all manage to start, go ahead and install the mitigation
            // anyway. This would be a bad situation, and the mitigation is slower then
            // the alternative if it is not needed, but at least it will read correctly
            // on all cores.
            //
            // see arch/quirks.h for details about the quirk itself.
            let wait_status = mp::wait_for_all_cpus_ready(Deadline::no_slack(InstantUnknown(0)));
            if wait_status.is_err()
                // SAFETY: Only evaluated when `mp::wait_for_all_cpus_ready` succeeded, i.e. every
                // CPU has started and identified itself.
                || unsafe { arch_quirks_needs_arm_erratum_858921_mitigation() }
            {
                if wait_status.is_err() {
                    dprintf!(
                        ALWAYS,
                        "WARNING: Timed out waiting for all CPUs to start.  \
                         Using A73 quirks for zx_ticks_get in VDSO as a defensive measure.\n"
                    );
                } else {
                    dprintf!(INFO, "Using A73 quirks for zx_ticks_get in VDSO\n");
                }
                needs_a73_mitigation = true;
            }

            if arm_use_phys_timer_in_vdso() {
                dprintf!(
                    INFO,
                    "Using PCT instead of VCT as the system counter reference for zx_ticks_get in \
                     the VDSO\n"
                );
                use_pct_instead_of_vct = true;
            }
        }
    }

    // Initialize the time values that should be visible to the vDSO.
    let mut values = TimeValues {
        version: 1,
        ticks_per_second: per_second,
        boot_ticks_offset: timer_get_boot_ticks_offset(),
        mono_ticks_offset: AtomicI64::new(timer_get_mono_ticks_offset()),
        ticks_to_time_numerator: ticks_to_time_ratio.numerator(),
        ticks_to_time_denominator: ticks_to_time_ratio.denominator(),
        usermode_can_access_ticks: u8::from(usermode_can_access_ticks),
        use_a73_errata_mitigation: u8::from(needs_a73_mitigation),
        use_pct_instead_of_vct: u8::from(use_pct_instead_of_vct),
        _padding: [0; 5],
    };

    // Write the time values to the appropriate section in the vDSO.
    // `as_mut_bytes` (and hence the `FromBytes` derive) is needed only because `AtomicI64` is
    // not `Immutable`, so `as_bytes` is unavailable.
    let bytes: &[u8] = values.as_mut_bytes();
    let status = vmo.write(vdso_code::VDSO_DATA_TIME_VALUES as u64, bytes);
    assert!(
        status.is_ok(),
        "vDSO Time Values VMO Write of {} bytes at {:#x} failed: {}",
        bytes.len(),
        vdso_code::VDSO_DATA_TIME_VALUES,
        Status::result_into_raw(status)
    );
}

/// Fill out the contents of the vdso_constants struct.
fn set_constants(vmo: &RefPtr<VmObject>) {
    let version_string = system_version_string();
    assert!(
        version_string.len() <= MAX_VERSION_STRING,
        "version string size {} > max {}: \"{}\"",
        version_string.len(),
        MAX_VERSION_STRING,
        core::str::from_utf8(version_string).unwrap_or("<invalid utf8>")
    );

    // Initialize the constants that should be visible to the vDSO.
    // This uses a struct literal, so the compiler rejects any omitted field.
    let constants = VdsoConstants {
        max_num_cpus: arch_rs::max_num_cpus(),
        features: VdsoFeatures {
            cpu: ops::cpu_features(),
            hw_breakpoint_count: u32::from(ops::get_hw_breakpoint_count()),
            hw_watchpoint_count: u32::from(ops::get_hw_watchpoint_count()),
            address_tagging: ops::address_tagging_features(),
            vm: ops::arch_vm_features(),
            cpu_sve: ops::cpu_sve_features(),
        },
        dcache_line_size: ops::dcache_line_size(),
        icache_line_size: ops::icache_line_size(),
        page_size: PAGE_SIZE as u32,
        physmem: pmm::count_total_bytes(),
        version_string_len: version_string.len() as u64,
    };

    let mut offset = vdso_code::VDSO_DATA_CONSTANTS as u64;
    let mut write_vmo = |bytes: &[u8]| {
        let status = vmo.write(offset, bytes);
        assert!(
            status.is_ok(),
            "vDSO VMO Write of {} bytes at {:#x} failed: {}",
            bytes.len(),
            offset,
            Status::result_into_raw(status)
        );
        offset += bytes.len() as u64;
    };

    // Write the constants initialized above, without the flexible array member.
    write_vmo(constants.as_bytes());

    // Store the version string and NUL terminator in the flexible array member.
    // The MAX_VERSION_STRING check ensures there is enough space for all that.
    write_vmo(version_string);
    write_vmo(&[0]);
}

/// Conditionally patch some of the entry points related to time based on
/// platform details which get determined at runtime.
fn patch_time_syscalls(mut mutator: VDsoMutator<'_>) {
    // If user mode cannot access the tick counter registers, or kernel command
    // line arguments demand that we access the tick counter via a syscall
    // instead of direct observation, then we need to make sure to redirect
    // symbol in the vDSO such that we always syscall in order to query ticks.
    //
    // Since this can effect how clock monotonic is calculated as well, we may
    // need to redirect zx_clock_get_monotonic as well.
    let need_syscall_for_ticks = !platform_usermode_can_access_tick_registers()
        || BootOptions::get().vdso_ticks_get_force_syscall;

    if need_syscall_for_ticks {
        redirect_syscall!(mutator, zx_ticks_get, SYSCALL_zx_ticks_get_via_kernel);
        redirect_syscall!(mutator, zx_ticks_get_boot, SYSCALL_zx_ticks_get_boot_via_kernel);
        redirect_syscall!(mutator, zx_clock_read_mapped, clock_read_mapped_via_kernel);
        redirect_syscall!(
            mutator,
            zx_clock_get_details_mapped,
            clock_get_details_mapped_via_kernel
        );
    }

    if BootOptions::get().vdso_clock_get_force_syscall {
        // Force a syscall for zx_clock_get_monotonic and zx_clock_get_boot if instructed to do so
        // by the kernel command line arguments.  Make sure to swap out the implementation
        // of zx_deadline_after as well.
        redirect_syscall!(mutator, zx_clock_get_boot, SYSCALL_zx_clock_get_boot_via_kernel);
        redirect_syscall!(
            mutator,
            zx_clock_get_monotonic,
            SYSCALL_zx_clock_get_monotonic_via_kernel
        );
        redirect_syscall!(mutator, zx_deadline_after, deadline_after_via_kernel_mono);
    } else if need_syscall_for_ticks {
        // If ticks must be accessed via syscall, then choose the alternate form
        // for clock_get_monotonic and clock_get_boot which performs the scaling in user mode, but
        // thunks into the kernel to read the ticks register.
        redirect_syscall!(mutator, zx_clock_get_monotonic, clock_get_monotonic_via_kernel_ticks);
        redirect_syscall!(mutator, zx_clock_get_boot, clock_get_boot_via_kernel_ticks);
        redirect_syscall!(mutator, zx_deadline_after, deadline_after_via_kernel_ticks);
    }
}

/// vDSO variant selector.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Stable = 0,
    Next = 1,
    Test1 = 2,
    Test2 = 3,
}

impl Variant {
    /// Every variant, in index order.
    const ALL: [Variant; 4] = [Variant::Stable, Variant::Next, Variant::Test1, Variant::Test2];
}

/// The `VDso` instance, published exactly once by [`VDso::create`] and never freed.
///
/// This is an `AtomicPtr` rather than a `LazyInit` because [`VDso::vmo_is_vdso`] and
/// [`VDso::set_monotonic_ticks_offset`] may run before `create` and must then see "no vDSO"
/// without panicking, and `LazyInit` has no non-panicking "is initialized" query.
static INSTANCE: AtomicPtr<VDso> = AtomicPtr::new(null_mut());

/// Kernel vDSO manager (`VDso`).
pub struct VDso {
    vmo: RefPtr<VmObject>,
    variant_vmo: [RefPtr<VmObjectDispatcher>; Self::NUM_VDSO_VARIANTS],
    variant_time_mappings: [KernelMappedVmo; Self::NUM_VDSO_VARIANTS],
    time_values: [NonNull<TimeValues>; Self::NUM_VDSO_VARIANTS],
    vmo_rights: zx_rights_t,
}

// SAFETY: Once `VDso::create` publishes the `VDso`, none of its fields are modified. The only
// shared mutation is through the `time_values` pointers, which refer to pinned kernel mappings
// owned by `variant_time_mappings`: there, only `TimeValues::mono_ticks_offset` (an `AtomicI64`)
// is ever written, and the other `TimeValues` fields are never written after the mappings are
// created.
unsafe impl Sync for VDso {}

impl VDso {
    /// Total number of vDSO variants managed by [`VDso`].
    pub const NUM_VDSO_VARIANTS: usize = Variant::ALL.len();

    /// This is called exactly once, at boot time.
    ///
    /// The created `VDso` will retain `RefPtr`s to the created `VmObjectDispatcher`s,
    /// but ownership of the wrapping handles are given to the caller.
    ///
    /// The RoDso VMO is created in `vmo_kernel_handles[Variant::Next]`
    /// with the `VDso` variants in the other slots.
    pub fn create(
        elf_image: &HandoffEndElf,
    ) -> (
        &'static Self,
        [KernelHandle<VmObjectDispatcher>; Self::NUM_VDSO_VARIANTS],
        KernelHandle<VmObjectDispatcher>,
    ) {
        // Fail fast on a second call, before any VMOs are patched. Single initialization is
        // ultimately enforced by the `compare_exchange` that publishes `INSTANCE` at the end of
        // this function.
        assert!(INSTANCE.load(Ordering::Relaxed).is_null(), "VDso::create called more than once");

        // Check the ELF segments are valid for the vDSO.
        for segment in &*elf_image.mappings {
            assert!(
                !segment.perms.writable() && segment.paddr != ZERO_FILL,
                "vDSO cannot have writable segment [{:#x}, {:#x})",
                segment.vaddr,
                segment.vaddr + segment.size
            );
            if segment.perms.executable() {
                assert!(
                    segment.vaddr as u64 == VDSO_CODE_START && segment.size == VDSO_CODE_SIZE,
                    "vDSO code segment [{:#x}, {:#x}) doesn't match expected [{:#x}, {:#x})",
                    segment.vaddr,
                    segment.vaddr + segment.size,
                    VDSO_CODE_START,
                    VDSO_CODE_SIZE
                );
            }
        }

        let vmo = elf_image.vmo.clone().expect("vDSO VMO missing");
        let mut vmo_kernel_handles: [Option<KernelHandle<VmObjectDispatcher>>;
            Self::NUM_VDSO_VARIANTS] = [const { None }; Self::NUM_VDSO_VARIANTS];

        // build and point a dispatcher at it
        let (next_handle, vmo_rights) = VmObjectDispatcher::create(
            &vmo,
            elf_image.stream_size as u64,
            InitialMutability::Mutable,
        )
        .expect("VmObjectDispatcher::create failed");
        let vmo_rights = (vmo_rights & !ZX_RIGHT_WRITE) | ZX_RIGHT_EXECUTE;

        let next_vmo = next_handle.dispatcher().clone();
        vmo_kernel_handles[Self::variant_index(Variant::Next)] = Some(next_handle);

        // Sanity-check that it's the exact vDSO image the kernel was compiled for.
        check_build_id(&vmo);

        // Fill out the contents of the vdso_constants struct.
        set_constants(&vmo);

        patch_time_syscalls(VDsoMutator::new(&vmo));

        // Fill out the contents of the time_values struct.
        set_time_values(&vmo);

        // Create the standalone time values VMO for use by fasttime.
        let time_values_handle = Self::create_time_values_vmo(&next_vmo);

        // Create the vDSO variants.
        let variant_vmo = Variant::ALL.map(|v| {
            Self::create_variant(&next_vmo, v, &mut vmo_kernel_handles[Self::variant_index(v)])
        });

        // Map and pin the time values VMO for each variant. We do this after having created all of
        // the variants to avoid any issues with pinning pages in a VMO prior to snapshotting it.
        let mut variant_time_mappings = [const { KernelMappedVmo::new() }; Self::NUM_VDSO_VARIANTS];
        let time_values = Variant::ALL.map(|v| {
            let idx = Self::variant_index(v);
            Self::map_time_values_vmo(&mut variant_time_mappings[idx], variant_vmo[idx].vmo())
                .expect("vDSO time values mapping failed")
        });

        let vdso =
            Box::try_new(Self { vmo, variant_vmo, variant_time_mappings, time_values, vmo_rights })
                .expect("VDso allocation failed");
        let vdso_ptr = Box::into_raw(vdso);
        // Release ordering pairs with the Acquire loads in `vmo_is_vdso` and
        // `set_monotonic_ticks_offset` so all initialized `VDso` fields are visible to readers.
        // The failure ordering can be Relaxed because a failure only panics.
        INSTANCE
            .compare_exchange(null_mut(), vdso_ptr, Ordering::Release, Ordering::Relaxed)
            .expect("VDso::create called more than once");
        // SAFETY: `vdso_ptr` comes from `Box::into_raw` and is never freed, so it is valid for
        // `'static`, and no mutable reference to it is created after this point.
        let instance = unsafe { &*vdso_ptr };
        (instance, vmo_kernel_handles.map(|h| h.unwrap()), time_values_handle)
    }

    /// Returns true if the given VMO is a vDSO VMO.
    #[inline]
    pub fn vmo_is_vdso(vmo: &VmObject) -> bool {
        // Acquire ordering pairs with the Release publication in `VDso::create` so all
        // initialized `VDso` fields are visible when `instance` is non-null.
        let instance = INSTANCE.load(Ordering::Acquire);
        if instance.is_null() {
            // No vDSO has been created yet (or ever, with userabi disabled).
            core::hint::cold_path();
            return false;
        }
        // SAFETY: A non-null `INSTANCE` points to the leaked `VDso` published by `VDso::create`,
        // which is never freed or mutated after publication.
        unsafe { &*instance }.vmo_is_vdso_impl(vmo)
    }

    /// Returns true if `[vmo_offset, vmo_offset + size)` matches the vDSO code segment.
    #[inline]
    pub fn valid_code_mapping(vmo_offset: u64, size: usize) -> bool {
        vmo_offset == VDSO_CODE_START && size == VDSO_CODE_SIZE
    }

    /// Given `VmAspace::vdso_code_mapping_locked`, return the vDSO base address.
    #[inline]
    pub fn base_address(
        code_mapping: &VmMapping,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> usize {
        code_mapping.base().0 - vdso_code::VDSO_CODE_START
    }

    /// Sets the monotonic ticks offset in all of the vDSO variants' `TimeValues` structure. This
    /// will be called on system resume to update the amount of time the system spent in a
    /// suspended state.
    pub fn set_monotonic_ticks_offset(new_offset: zx_ticks_t) {
        // Acquire ordering pairs with the Release publication in `VDso::create` so all
        // initialized `VDso` fields are visible when `instance` is non-null.
        let instance = INSTANCE.load(Ordering::Acquire);
        if instance.is_null() {
            core::hint::cold_path();
            return;
        }
        // SAFETY: A non-null `INSTANCE` points to the leaked `VDso` published by `VDso::create`,
        // which is never freed or mutated after publication.
        let instance = unsafe { &*instance };
        for time_values in &instance.time_values {
            // TODO(https://fxbug.dev/341785588): This code should be made resilient to a changing
            // mono_ticks_offset once we start pausing the clock during system suspension.
            let p = time_values.as_ptr();
            // SAFETY: `p` points to a `TimeValues` in a page-aligned kernel mapping (so the
            // field is suitably aligned) that is pinned and stays mapped for `'static`, since the
            // owning `VDso` is never freed. This projects to the field without creating a
            // reference to the whole `TimeValues`, which is shared with user mode;
            // `mono_ticks_offset` is only ever accessed atomically, by both the kernel and user
            // mode.
            let mono_ticks_offset = unsafe { &(*p).mono_ticks_offset };
            // Relaxed is sufficient: this is a single independent word read by user-mode
            // fasttime, and no other data is published along with it.
            mono_ticks_offset.store(new_offset, Ordering::Relaxed);
        }
    }

    /// Returns the handle rights used for vDSO VMO handles.
    pub fn vmo_rights(&self) -> zx_rights_t {
        self.vmo_rights
    }

    /// The `time_values_vmo` is a child slice of the read-only section of the vDSO that contains
    /// just the `TimeValues` structure.
    ///
    /// `dispatcher` is the dispatcher for the `Next` variant.
    fn create_time_values_vmo(
        dispatcher: &RefPtr<VmObjectDispatcher>,
    ) -> KernelHandle<VmObjectDispatcher> {
        let new_vmo = dispatcher
            .create_child(
                ZX_VMO_CHILD_SLICE,
                vdso_code::VDSO_DATA_TIME_VALUES as u64,
                vdso_code::VDSO_DATA_TIME_VALUES_SIZE as u64,
                false,
            )
            .expect("create_child failed");

        let (time_values_handle, _rights) = VmObjectDispatcher::create(
            &new_vmo,
            vdso_code::VDSO_DATA_TIME_VALUES_SIZE as u64,
            InitialMutability::Mutable,
        )
        .expect("VmObjectDispatcher::create failed");

        let status = time_values_handle.dispatcher().set_name(TIME_VALUES_VMO_NAME);
        assert!(status.is_ok());

        time_values_handle
    }

    /// Initializes `time_mapping` with a pinned kernel mapping of the `TimeValues` in `vdso_vmo`
    /// and returns a pointer to the mapped `TimeValues`.
    fn map_time_values_vmo(
        time_mapping: &mut KernelMappedVmo,
        vdso_vmo: &RefPtr<VmObject>,
    ) -> Result<NonNull<TimeValues>, Status> {
        time_mapping.init(
            vdso_vmo.clone(),
            vdso_code::VDSO_DATA_TIME_VALUES,
            vdso_code::VDSO_DATA_TIME_VALUES_SIZE,
            b"vdso time values",
        )?;

        // Store the pointer to the actual TimeValues structure to avoid having to call
        // base_locking every time. We can do this because the mappings are not changed once
        // created.
        Ok(NonNull::new(core::ptr::with_exposed_provenance_mut(time_mapping.base_locking().0))
            .expect("vDSO time values mapping at address 0"))
    }

    /// Each vDSO variant VMO is made via a COW clone of the next vDSO
    /// VMO.  A variant can block some system calls, by syscall category.
    /// This works by modifying the symbol table entries to make the symbols
    /// invisible to dynamic linking (`STB_LOCAL`) and then clobbering the code
    /// with trapping instructions.  In this way, all the code locations are the
    /// same across variants and the syscall entry enforcement doesn't have to
    /// care which variant is in use.  The places where the blocked
    /// syscalls' syscall entry instructions would be no longer have the syscall
    /// instructions, so a process using the variant can never get into syscall
    /// entry with that PC value and hence can never pass the vDSO enforcement
    /// test.
    ///
    /// `next_vmo` is the dispatcher for the `Next` variant. This returns the dispatcher for
    /// `variant`, whose handle is stored in `vmo_kernel_handle`.
    fn create_variant(
        next_vmo: &RefPtr<VmObjectDispatcher>,
        variant: Variant,
        vmo_kernel_handle: &mut Option<KernelHandle<VmObjectDispatcher>>,
    ) -> RefPtr<VmObjectDispatcher> {
        if variant == Variant::Next {
            // The next variant already has a VMO.
            debug_assert!(
                RefPtr::as_ptr(next_vmo)
                    == RefPtr::as_ptr(vmo_kernel_handle.as_ref().unwrap().dispatcher())
            );
            return next_vmo.clone();
        }

        debug_assert!(vmo_kernel_handle.is_none());

        let new_vmo = next_vmo
            .create_child(ZX_VMO_CHILD_SNAPSHOT, 0, next_vmo.vmo().size(), false)
            .expect("create_child failed");

        ltracef!("variant {}\n", variant as usize);

        let mut mutator = VDsoMutator::new(&new_vmo);

        let name: &[u8] = match variant {
            Variant::Stable => {
                let name = "vdso/stable";
                ltracef!("variant {}\n", name);
                block_next_syscalls(&mut mutator);
                name.as_bytes()
            }
            Variant::Test1 => {
                let name = "vdso/test1";
                ltracef!("variant {}\n", name);
                block_test_category1_syscalls(&mut mutator);
                name.as_bytes()
            }
            Variant::Test2 => {
                let name = "vdso/test2";
                ltracef!("variant {}\n", name);
                block_test_category2_syscalls(&mut mutator);
                name.as_bytes()
            }
            // No wildcard arm so the compiler will reject new enum entries.
            Variant::Next => {
                panic!("VDso::create_variant called with bad variant");
            }
        };

        let (handle, _rights) = VmObjectDispatcher::create(
            &new_vmo,
            next_vmo.get_stream_size(),
            InitialMutability::Mutable,
        )
        .expect("VmObjectDispatcher::create failed");

        let status = handle.dispatcher().set_name(name);
        assert!(status.is_ok());

        let dispatcher = handle.dispatcher().clone();
        *vmo_kernel_handle = Some(handle);
        dispatcher
    }

    #[inline]
    fn vmo_is_vdso_impl(&self, vmo: &VmObject) -> bool {
        let vmo = from_ref(vmo);
        if vmo == RefPtr::as_ptr(&self.vmo) {
            return true;
        }
        for v in &self.variant_vmo {
            if vmo == RefPtr::as_ptr(v.vmo()) {
                return true;
            }
        }
        false
    }

    const fn variant_index(v: Variant) -> usize {
        v as usize
    }
}
