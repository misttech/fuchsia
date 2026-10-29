// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::aspace_bindings::{
    X86_PAT_INDEX0, X86_PAT_INDEX1, X86_PAT_INDEX2, X86_PAT_INDEX3, X86_PAT_INDEX4, X86_PAT_INDEX5,
    X86_PAT_INDEX6, X86_PAT_INDEX7,
};
use super::registers::{
    X86_CR0_CD, X86_CR0_NW, X86_CR4_PGE, X86_MSR_IA32_MTRR_DEF_TYPE, X86_MSR_IA32_MTRR_FIX4K_C0000,
    X86_MSR_IA32_MTRR_FIX16K_80000, X86_MSR_IA32_MTRR_FIX64K_00000, X86_MSR_IA32_MTRR_PHYSBASE0,
    X86_MSR_IA32_MTRR_PHYSMASK0, X86_MSR_IA32_MTRRCAP, X86_MSR_IA32_PAT,
};
use super::x86::{
    read_msr, wbinvd, write_msr, x86_get_cr0, x86_get_cr3, x86_get_cr4, x86_set_cr0, x86_set_cr3,
    x86_set_cr4,
};
use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num, ints_disabled};
use crate::kernel::mp::{self, MpIpiTarget, cpu_mask_t};

const IA32_MTRR_NUM_FIX16K: u32 = 2;
const IA32_MTRR_NUM_FIX4K: u32 = 8;
const MTRRCAP_VCNT_MAX: usize = 255;

/// Number of variable length MTRRs.
///
/// Initialized along with [`SUPPORTS_FIXED_RANGE`] and [`SUPPORTS_WC`] on CPU 0 during early boot
/// and then read concurrently by APs during [`pat_sync_task`]. AP bringup and [`mp::sync_exec`]
/// IPI dispatch establish the happens-before synchronization that makes `Ordering::Relaxed`
/// sufficient to avoid data races.
static NUM_VARIABLE: AtomicU8 = AtomicU8::new(0);
/// Whether or not fixed range MTRRs are supported.
static SUPPORTS_FIXED_RANGE: AtomicBool = AtomicBool::new(false);
/// Whether write-combining memory type is supported.
static SUPPORTS_WC: AtomicBool = AtomicBool::new(false);

struct VariableMtrr {
    physbase: AtomicU64,
    physmask: AtomicU64,
}

struct Mtrrs {
    mtrr_def: AtomicU64,
    mtrr_fix64k: AtomicU64,
    mtrr_fix16k: [AtomicU64; IA32_MTRR_NUM_FIX16K as usize],
    mtrr_fix4k: [AtomicU64; IA32_MTRR_NUM_FIX4K as usize],
    mtrr_var: [VariableMtrr; MTRRCAP_VCNT_MAX],
}

/// Global MTRR state initialized on CPU 0 and concurrently read by all online CPUs in
/// [`pat_sync_task`]. AP bringup and [`mp::sync_exec`] IPI dispatch establish the happens-before
/// synchronization that makes `Ordering::Relaxed` sufficient to avoid data races.
static THE_MTRRS: Mtrrs = Mtrrs {
    mtrr_def: AtomicU64::new(0),
    mtrr_fix64k: AtomicU64::new(0),
    mtrr_fix16k: [const { AtomicU64::new(0) }; IA32_MTRR_NUM_FIX16K as usize],
    mtrr_fix4k: [const { AtomicU64::new(0) }; IA32_MTRR_NUM_FIX4K as usize],
    mtrr_var: [const { VariableMtrr { physbase: AtomicU64::new(0), physmask: AtomicU64::new(0) } };
        MTRRCAP_VCNT_MAX],
};

struct PatSyncTaskContext {
    /// Barrier counters for the two barriers described in Intel's algorithm.
    barrier1: AtomicU32,
    barrier2: AtomicU32,
}

#[inline]
const fn mtrrcap_vcnt(caps: u64) -> u8 {
    (caps & 0xff) as u8
}

#[inline]
const fn mtrrcap_fix(caps: u64) -> bool {
    (caps & (1 << 8)) != 0
}

#[inline]
const fn mtrrcap_wc(caps: u64) -> bool {
    (caps & (1 << 10)) != 0
}

#[inline]
fn mmu_num_variable() -> u8 {
    NUM_VARIABLE.load(Ordering::Relaxed)
}

#[inline]
const fn ia32_mtrr_fix16k_80000(x: u32) -> u32 {
    X86_MSR_IA32_MTRR_FIX16K_80000 + x
}

#[inline]
const fn ia32_mtrr_fix4k_c0000(x: u32) -> u32 {
    X86_MSR_IA32_MTRR_FIX4K_C0000 + x
}

#[inline]
const fn ia32_mtrr_physbase(x: u32) -> u32 {
    X86_MSR_IA32_MTRR_PHYSBASE0 + 2 * x
}

#[inline]
const fn ia32_mtrr_physmask(x: u32) -> u32 {
    X86_MSR_IA32_MTRR_PHYSMASK0 + 2 * x
}

/// Initializes x86 MTRR and PAT memory type state on the bootstrap processor.
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during early MMU
/// initialization when MTRR and PAT MSRs are accessible at CPL 0.
pub(super) unsafe fn mmu_mem_type_init() {
    // SAFETY: Caller guarantees CPL 0 and valid architectural MTRRCAP MSR on x86_64.
    let caps = unsafe { read_msr(X86_MSR_IA32_MTRRCAP) };
    NUM_VARIABLE.store(mtrrcap_vcnt(caps), Ordering::Relaxed);
    SUPPORTS_FIXED_RANGE.store(mtrrcap_fix(caps), Ordering::Relaxed);
    SUPPORTS_WC.store(mtrrcap_wc(caps), Ordering::Relaxed);

    // SAFETY: Caller guarantees CPL 0 and valid architectural MTRR MSRs on x86_64.
    let (mtrr_def, mtrr_fix64k) =
        unsafe { (read_msr(X86_MSR_IA32_MTRR_DEF_TYPE), read_msr(X86_MSR_IA32_MTRR_FIX64K_00000)) };
    THE_MTRRS.mtrr_def.store(mtrr_def, Ordering::Relaxed);
    THE_MTRRS.mtrr_fix64k.store(mtrr_fix64k, Ordering::Relaxed);
    for i in 0..IA32_MTRR_NUM_FIX16K {
        // SAFETY: `i < IA32_MTRR_NUM_FIX16K` selects a valid architectural 16K fixed-range MTRR.
        let val = unsafe { read_msr(ia32_mtrr_fix16k_80000(i)) };
        THE_MTRRS.mtrr_fix16k[i as usize].store(val, Ordering::Relaxed);
    }
    for i in 0..IA32_MTRR_NUM_FIX4K {
        // SAFETY: `i < IA32_MTRR_NUM_FIX4K` selects a valid architectural 4K fixed-range MTRR.
        let val = unsafe { read_msr(ia32_mtrr_fix4k_c0000(i)) };
        THE_MTRRS.mtrr_fix4k[i as usize].store(val, Ordering::Relaxed);
    }
    for i in 0..(mmu_num_variable() as u32) {
        // SAFETY: `i < NUM_VARIABLE` selects a valid variable-range MTRR pair supported by the CPU.
        let (physbase, physmask) =
            unsafe { (read_msr(ia32_mtrr_physbase(i)), read_msr(ia32_mtrr_physmask(i))) };
        THE_MTRRS.mtrr_var[i as usize].physbase.store(physbase, Ordering::Relaxed);
        THE_MTRRS.mtrr_var[i as usize].physmask.store(physmask, Ordering::Relaxed);
    }

    // Update the PAT on the bootstrap processor (and sync any changes to the
    // MTRR that may have been made above).
    // SAFETY: Called on CPU 0 after `THE_MTRRS` has been populated above and before APs are online.
    unsafe { pat_sync(mp::cpu_num_to_mask(0)) };
}

/// Gives the specified CPUs our Page Attribute Tables and Memory Type Range Registers.
///
/// This operation is not safe to perform while a CPU may be hotplugged. This should be called with
/// `mp::get_online_mask()` as the targets if we ever want to update the PAT or MTRRs after boot.
///
/// This algorithm is based on section 11.11.8 of Intel 3A.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0, no CPUs are concurrently
/// hotplugged, and [`mmu_mem_type_init`] has already populated `THE_MTRRS` and `NUM_VARIABLE`.
pub(super) unsafe fn pat_sync(mut targets: cpu_mask_t) {
    targets &= mp::get_online_mask();

    let context =
        PatSyncTaskContext { barrier1: AtomicU32::new(targets), barrier2: AtomicU32::new(targets) };
    // Step 1: Broadcast to all processors to execute the sequence
    if targets == mp::cpu_num_to_mask(curr_cpu_num()) {
        // Directly call the task without IPIs as its redundant and we might still
        // be in early init where we cannot call mp_sync_exec
        let _irqd = InterruptDisableGuard::new();

        // SAFETY: Interrupts are disabled on the current CPU at CPL 0, `THE_MTRRS` is initialized,
        // and `context` barriers match the single participating CPU.
        unsafe { pat_sync_task(&context) };
    } else {
        mp::sync_exec(MpIpiTarget::Mask, targets, || {
            // SAFETY: `mp::sync_exec` runs with interrupts disabled at CPL 0 on all target CPUs in
            // `targets`, `THE_MTRRS` is initialized, and `mp::sync_exec` blocks until all complete.
            unsafe { pat_sync_task(&context) };
        });
    }
}

/// Function called by all participating CPUs to set up their PAT and MTRRs.
///
/// # Safety
///
/// The caller must ensure that:
/// - The current privilege level is CPL 0 and interrupts are disabled on each participating CPU.
/// - `context.barrier1` and `context.barrier2` are initialized to the exact bitmask of CPUs
///   concurrently executing `pat_sync_task` (including the calling CPU).
/// - `THE_MTRRS` and `NUM_VARIABLE` (`<= MTRRCAP_VCNT_MAX`) have been initialized with valid MTRR
///   values supported by the CPU via [`mmu_mem_type_init`].
#[expect(clippy::identity_op)]
unsafe fn pat_sync_task(context: &PatSyncTaskContext) {
    // Step 2: Disable interrupts
    debug_assert!(ints_disabled());

    let cpu_mask = mp::cpu_num_to_mask(curr_cpu_num());

    // Step 3: Wait for all processors to reach this point.
    context.barrier1.fetch_and(!cpu_mask, Ordering::SeqCst);
    while context.barrier1.load(Ordering::SeqCst) != 0 {
        libarch::r#yield();
    }

    // Step 4: Enter the no-fill cache mode (cache-disable and writethrough)
    // SAFETY: Caller guarantees CPL 0 with interrupts disabled during the synchronized sequence.
    let mut cr0 = unsafe { x86_get_cr0() };
    debug_assert!((cr0 & X86_CR0_CD) == 0 && (cr0 & X86_CR0_NW) == 0);
    cr0 |= X86_CR0_CD;
    cr0 &= !X86_CR0_NW;
    // SAFETY: Sets CR0.CD=1 and CR0.NW=0 at CPL 0 while preserving paging and protected mode.
    unsafe { x86_set_cr0(cr0) };

    // Step 5: Flush all caches
    // SAFETY: Caller guarantees CPL 0.
    unsafe { wbinvd() };

    // Step 6: If the PGE flag is set, clear it to flush the TLB
    // SAFETY: Caller guarantees CPL 0.
    let mut cr4 = unsafe { x86_get_cr4() };
    let pge_was_set = (cr4 & X86_CR4_PGE) != 0;
    cr4 &= !X86_CR4_PGE;
    // SAFETY: Clears CR4.PGE at CPL 0 to flush global TLB entries.
    unsafe { x86_set_cr4(cr4) };

    // Step 7: If the PGE flag wasn't set, flush the TLB via CR3
    if !pge_was_set {
        // SAFETY: Reloading the current CR3 value at CPL 0 flushes non-global TLB entries.
        unsafe { x86_set_cr3(x86_get_cr3()) };
    }

    // Step 8: Disable MTRRs
    // SAFETY: Disables MTRRs at CPL 0 while caches are in no-fill mode.
    unsafe { write_msr(X86_MSR_IA32_MTRR_DEF_TYPE, 0) };

    // Step 9: Sync up the MTRR entries
    let fix64k = THE_MTRRS.mtrr_fix64k.load(Ordering::Relaxed);
    // SAFETY: Writes valid cached 64K fixed-range MTRR value at CPL 0.
    unsafe { write_msr(X86_MSR_IA32_MTRR_FIX64K_00000, fix64k) };
    for i in 0..IA32_MTRR_NUM_FIX16K {
        let val = THE_MTRRS.mtrr_fix16k[i as usize].load(Ordering::Relaxed);
        // SAFETY: `i < IA32_MTRR_NUM_FIX16K` selects a valid 16K fixed-range MTRR MSR at CPL 0.
        unsafe { write_msr(ia32_mtrr_fix16k_80000(i), val) };
    }
    for i in 0..IA32_MTRR_NUM_FIX4K {
        let val = THE_MTRRS.mtrr_fix4k[i as usize].load(Ordering::Relaxed);
        // SAFETY: `i < IA32_MTRR_NUM_FIX4K` selects a valid 4K fixed-range MTRR MSR at CPL 0.
        unsafe { write_msr(ia32_mtrr_fix4k_c0000(i), val) };
    }
    for i in 0..(mmu_num_variable() as u32) {
        let physbase = THE_MTRRS.mtrr_var[i as usize].physbase.load(Ordering::Relaxed);
        let physmask = THE_MTRRS.mtrr_var[i as usize].physmask.load(Ordering::Relaxed);
        // SAFETY: `i < NUM_VARIABLE` selects a valid variable-range MTRR MSR pair at CPL 0.
        unsafe {
            write_msr(ia32_mtrr_physbase(i), physbase);
            write_msr(ia32_mtrr_physmask(i), physmask);
        }
    }

    // For now, we leave the MTRRs as the firmware gave them to us, except for
    // setting the default memory type to uncached

    // Starting from here, we diverge from the algorithm in 11.11.8.  That
    // algorithm is for MTRR changes, and 11.12.4 suggests using a variant of
    // it.

    // Perform PAT changes now that caches aren't being filled and the
    // TLB is flushed.
    let mut pat_val: u64 = 0;
    pat_val |= u64::from(X86_PAT_INDEX0) << 0;
    pat_val |= u64::from(X86_PAT_INDEX1) << 8;
    pat_val |= u64::from(X86_PAT_INDEX2) << 16;
    pat_val |= u64::from(X86_PAT_INDEX3) << 24;
    pat_val |= u64::from(X86_PAT_INDEX4) << 32;
    pat_val |= u64::from(X86_PAT_INDEX5) << 40;
    pat_val |= u64::from(X86_PAT_INDEX6) << 48;
    pat_val |= u64::from(X86_PAT_INDEX7) << 56;
    // SAFETY: Writes valid PAT memory type encodings to `IA32_PAT` at CPL 0.
    unsafe { write_msr(X86_MSR_IA32_PAT, pat_val) };

    // Step 10: Re-enable MTRRs (and set the default type)
    let mtrr_def = THE_MTRRS.mtrr_def.load(Ordering::Relaxed);
    // SAFETY: Restores the valid `IA32_MTRR_DEF_TYPE` value captured on CPU 0 at CPL 0.
    unsafe { write_msr(X86_MSR_IA32_MTRR_DEF_TYPE, mtrr_def) };

    // Step 11: Flush all cache and the TLB again
    // SAFETY: Flushes caches (`wbinvd`) and reloads CR3 at CPL 0.
    unsafe {
        wbinvd();
        x86_set_cr3(x86_get_cr3());
    }

    // Step 12: Enter the normal cache mode
    // SAFETY: Clears CR0.CD and CR0.NW at CPL 0 to re-enable normal caching.
    unsafe {
        cr0 = x86_get_cr0();
        cr0 &= !(X86_CR0_CD | X86_CR0_NW);
        x86_set_cr0(cr0);
    }

    // Step 13: Re-enable PGE if it was previously set
    if pge_was_set {
        // SAFETY: Restores CR4.PGE at CPL 0.
        unsafe {
            cr4 = x86_get_cr4();
            cr4 |= X86_CR4_PGE;
            x86_set_cr4(cr4);
        }
    }

    // Step 14: Wait for all processors to reach this point.
    context.barrier2.fetch_and(!cpu_mask, Ordering::SeqCst);
    while context.barrier2.load(Ordering::SeqCst) != 0 {
        libarch::r#yield();
    }
}

#[cfg(console_enabled)]
mod console {
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_status::Status;
    use zx_types::ZX_OK;

    use super::super::mmu_mem_types_ffi::G_MAX_PADDR_WIDTH;
    use super::{
        IA32_MTRR_NUM_FIX4K, IA32_MTRR_NUM_FIX16K, Ordering, SUPPORTS_FIXED_RANGE,
        X86_MSR_IA32_MTRR_DEF_TYPE, X86_MSR_IA32_MTRR_FIX64K_00000, X86_MSR_IA32_PAT,
        ia32_mtrr_fix4k_c0000, ia32_mtrr_fix16k_80000, ia32_mtrr_physbase, ia32_mtrr_physmask,
        mmu_num_variable, read_msr,
    };
    use crate::arch_rs::max_num_cpus;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::mp::{self, MpIpiTarget, sync_exec};

    /// Returns whether fixed-range MTRRs are supported on the CPU.
    #[inline]
    fn mmu_supports_fixed_range() -> bool {
        SUPPORTS_FIXED_RANGE.load(Ordering::Relaxed)
    }

    /// Returns the maximum physical address width (`g_max_paddr_width`) on the CPU.
    #[inline]
    fn mmu_max_paddr_width() -> u8 {
        // SAFETY: `g_max_paddr_width` is initialized once during single-threaded early boot
        // before console commands can run.
        unsafe { G_MAX_PADDR_WIDTH }
    }

    /// Global enable flag for MTRRs.
    #[inline]
    const fn mtrr_def_type_enable(x: u64) -> bool {
        (x & (1 << 11)) != 0
    }

    /// Enable flag for fixed-range MTRRs.
    #[inline]
    const fn mtrr_def_type_fixed_enable(x: u64) -> bool {
        (x & (1 << 10)) != 0
    }

    #[inline]
    const fn mtrr_def_type_type(x: u64) -> u8 {
        x as u8
    }

    #[inline]
    fn mtrr_physbase_base(x: u64) -> u64 {
        let width = mmu_max_paddr_width();
        x & !((1u64 << 12) - 1) & ((1u64 << width) - 1)
    }

    #[inline]
    const fn mtrr_physbase_type(x: u64) -> u8 {
        x as u8
    }

    #[inline]
    fn mtrr_physmask_mask(x: u64) -> u64 {
        let width = mmu_max_paddr_width();
        x & !((1u64 << 12) - 1) & ((1u64 << width) - 1)
    }

    #[inline]
    const fn mtrr_physmask_valid(x: u64) -> bool {
        (x & (1 << 11)) != 0
    }

    /// Helper for decoding and printing MTRRs.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `msr` is a valid fixed-range MTRR MSR supported by the current
    /// CPU at CPL 0.
    unsafe fn print_fixed_range_mtrr(msr: u32, mut base: u32, record_size: u32) {
        // SAFETY: Caller guarantees `msr` is a valid fixed-range MTRR MSR at CPL 0.
        let mut val = unsafe { read_msr(msr) };
        for _i in 0..8 {
            kprintln!("  f {:#05x}-{:#05x}: {:#02x}", base, base + record_size - 1, val as u8);
            base += record_size;
            val >>= 8;
        }
    }

    fn print_pat_entries() {
        // SAFETY: `X86_MSR_IA32_PAT` is an architectural MSR on x86_64.
        let mut pat = unsafe { read_msr(X86_MSR_IA32_PAT) };
        for i in 0..8 {
            kprintln!("  Index {}: {:#02x}", i, pat as u8);
            pat >>= 8;
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that if `argc >= 1` and `argv` is non-null, `argv` is properly
    /// aligned and points to at least `argc` initialized `CmdArgs` elements that remain valid
    /// for shared reads for the duration of this call.
    unsafe extern "C" fn cmd_memtype(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc < 1 || argv.is_null() {
            return Status::INVALID_ARGS.into_raw();
        }
        // SAFETY: `argc >= 1` and `argv` is non-null; the caller guarantees `argv` is aligned
        // and points to `argc` initialized `CmdArgs` elements valid for the duration of this call.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} mtrr", argv[0].as_str());
            kprintln!("{:s} pat", argv[0].as_str());
            Status::INTERNAL.into_raw()
        };
        'usage: {
            if argv.len() < 2 {
                kprintln!("not enough arguments");
                break 'usage;
            }

            if argv[1].as_str() == "mtrr" {
                let mut print_fixed = false;
                if argv.len() > 2 {
                    if argv[2].as_str() == "-f" {
                        print_fixed = true;
                    } else {
                        kprintln!("usage: {:s} mtrr [-f]", argv[0].as_str());
                        kprintln!("  -f    Display fixed registers");
                        return Status::INTERNAL.into_raw();
                    }
                }
                // SAFETY: `X86_MSR_IA32_MTRR_DEF_TYPE` is a valid architectural MTRR MSR on x86_64.
                let default_type = unsafe { read_msr(X86_MSR_IA32_MTRR_DEF_TYPE) };
                kprintln!(
                    "MTRR state: master {:s}, fixed {:s}",
                    if mtrr_def_type_enable(default_type) { "enable" } else { "disable" },
                    if mtrr_def_type_fixed_enable(default_type) { "enable" } else { "disable" }
                );
                kprintln!("  default: {:#02x}", mtrr_def_type_type(default_type));
                if mmu_supports_fixed_range() && print_fixed {
                    // SAFETY: `mmu_supports_fixed_range()` is true, so the 64K fixed-range MTRR
                    // is valid.
                    unsafe {
                        print_fixed_range_mtrr(X86_MSR_IA32_MTRR_FIX64K_00000, 0x00000, 1 << 16);
                    }
                    for i in 0..IA32_MTRR_NUM_FIX16K {
                        let msr = ia32_mtrr_fix16k_80000(i);
                        let base = 0x80000 + i * (1 << 17);
                        // SAFETY: `mmu_supports_fixed_range()` is true and
                        // `i < IA32_MTRR_NUM_FIX16K`.
                        unsafe { print_fixed_range_mtrr(msr, base, 1 << 14) };
                    }
                    for i in 0..IA32_MTRR_NUM_FIX4K {
                        let msr = ia32_mtrr_fix4k_c0000(i);
                        let base = 0xC0000 + i * (1 << 15);
                        // SAFETY: `mmu_supports_fixed_range()` is true and
                        // `i < IA32_MTRR_NUM_FIX4K`.
                        unsafe { print_fixed_range_mtrr(msr, base, 1 << 12) };
                    }
                }

                for i in 0..(mmu_num_variable() as u32) {
                    // SAFETY: `i < mmu_num_variable()` guarantees `ia32_mtrr_physbase(i)` and
                    // `ia32_mtrr_physmask(i)` are valid variable-range MTRR MSRs.
                    let (base, mask) = unsafe {
                        (read_msr(ia32_mtrr_physbase(i)), read_msr(ia32_mtrr_physmask(i)))
                    };
                    kprintln!(
                        "  v ({:s}) base {:#016x}, mask {:#016x}: {:#02x}",
                        if mtrr_physmask_valid(mask) { "valid" } else { "invalid" },
                        mtrr_physbase_base(base),
                        mtrr_physmask_mask(mask),
                        mtrr_physbase_type(base)
                    );
                }
            } else if argv[1].as_str() == "pat" {
                let num_cpus = max_num_cpus();
                for i in 0..num_cpus {
                    kprintln!("CPU {i} Page Attribute Table types:");
                    sync_exec(MpIpiTarget::Mask, mp::cpu_num_to_mask(i), print_pat_entries);
                }
            } else {
                kprintln!("unknown command");
                break 'usage;
            }

            return ZX_OK;
        }

        usage()
    }

    static_command!(
        CMD_MEMTYPE,
        c"memtype".as_ptr(),
        c"memory type commands".as_ptr(),
        cmd_memtype,
        CMD_AVAIL_NORMAL
    );
}
