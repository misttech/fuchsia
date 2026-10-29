// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 CPU early setup, the `arch_*` init entry points, user-mode entry and the idle path.

use core::sync::atomic::{AtomicBool, Ordering};

use arch_arm64_aspace_bindings::kMmuTcrFlagsKernel;
use arm64_mp_bindings::ARM64_MPIDR_MASK;
use arm64_thread_bindings::MSDCR_EL1_INITIAL_VALUE;
use boot_options::{Arm64AlternateVbar as BootOptionsAlternateVbar, BootOptions};
use debug::{dprintf, ltracef};
use handoff::{ArchPhysHandoff, Arm64AlternateVbar, PhysHandoff};
use kprint::kprintln;
use lazy_init::LazyInit;
use libarch::arm64::{
    ArchitecturalFeatureAccessControlRegister, CPACR_EL1, CURRENT_EL, CpacrTrap,
    ExtendedTranslationControlRegister, ID_AA64MMFR3_EL1, MDSCR_EL1,
    MonitorDebugSystemControlRegister, SCTLR_EL1, SCTLR2_EL1, SystemControlRegister2El1,
    SystemControlRegisterEl1, TCR_EL1, TCR2_EL1, TranslationControlRegisterEl1, VBAR_EL1,
    VectorBaseAddressRegister, ZCR_EL1, invalidate_local_tlbs,
};
use regio::arm64::{SysReg, spec};
use zx_types::{ZX_ARM64_FEATURE_ISA_MOPS, ZX_ARM64_FEATURE_ISA_SVE};

use super::feature::{
    MPIDR_EL1_RAW, arm64_feature_debug, arm64_feature_init, arm64_isa_features, arm64_mmu_features,
    feat_pmuv3_enabled,
};
use super::mp::{
    arch_curr_cpu_num, arch_max_num_cpus, arch_mp_init_percpu, arch_register_mpid,
    arm64_init_percpu_early, write_percpu_field,
};
use super::smccc::{ArmSmcccCall, ArmSmcccFunction};
use super::thread::{TPIDR_EL0, thread_arch_const};
use super::uarch::arm64_uarch_needs_spectre_v2_mitigation;
use super::user_copy::arm64_enable_pan;
use super::vm::is_valid_user_pc;
use super::{Iframe, UserEntryState, arm64_feature_test, device_memory_barrier, isb, wfi};
use crate::kernel::thread::{self, get_shadow_call_base, get_stack_top};

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
#[cfg(console_enabled)]
use core::ffi::c_int;
#[cfg(console_enabled)]
use zx_status::Status;
#[cfg(console_enabled)]
use zx_types::ZX_OK;

const LOCAL_TRACE: u32 = 0;

// Kernel Rust always has the shadow call stack on, so only the form of `arm64_uspace_entry` that
// takes the shadow call stack base is declared.
zr::static_assert!(cfg!(sanitize = "shadow-call-stack"));

// Implemented in or called from assembly.
unsafe extern "C" {
    fn arm64_uspace_entry(iframe: *const Iframe, kstack: usize, scsp: usize) -> !;

    // Exception vector tables, from exceptions.S. Only their addresses are used.
    static arm64_el1_exception: [u8; 0];
    static arm64_el1_exception_smccc11_workaround: [u8; 0];
    static arm64_el1_exception_smccc10_workaround: [u8; 0];
}

unsafe extern "C" {
    fn cpp_phys_handoff_get() -> *const PhysHandoff;
    fn lk_init_secondary_cpus(secondary_cpu_count: u32);
}

const CNTKCTL_EL1: SysReg<spec::CNTKCTL_EL1, u64> = SysReg::new();
const OSLAR_EL1: SysReg<spec::OSLAR_EL1, u64> = SysReg::new();
const PMCNTENSET_EL0: SysReg<spec::PMCNTENSET_EL0, u64> = SysReg::new();
const PMCR_EL0: SysReg<spec::PMCR_EL0, u64> = SysReg::new();
const PMUSERENR_EL0: SysReg<spec::PMUSERENR_EL0, u64> = SysReg::new();

/// The alternate exception vector setting that physboot chose. `arch_early_init` sets it once, on
/// the boot CPU before any other CPU starts, and every CPU reads it in `arch_late_init_percpu`
/// after that.
static ALTERNATE_VBAR: LazyInit<Arm64AlternateVbar> = LazyInit::uninit();

/// Performance Monitors Count Enable Set, EL0.
const PMCNTENSET_EL0_ENABLE: u64 = 1 << 31; // Enable cycle count register.

/// Performance Monitor Control Register, EL0.
const PMCR_EL0_ENABLE_BIT: u64 = 1 << 0;
const PMCR_EL0_LONG_COUNTER_BIT: u64 = 1 << 6;

/// Performance Monitors User Enable Register, EL0.
const PMUSERENR_EL0_CR_ENABLE: u64 = 1 << 2; // Enable EL0 access to cycle counter.

/// Whether or not to allow access to the PCT (physical counter) from EL0, in
/// addition to allowing access to the VCT (virtual counter).  This decision
/// needs to be programmed into each CPU's copy of the CNTKCTL_EL1 register
/// during initialization.  By default, we deny access to the PCT and allow
/// access to the VCT, but if we determine that we _have_ to use PCT during clock
/// selection, we will come back and change this.  Clock selection happens before
/// the secondaries have started, so if we change our minds, we only need to
/// re-program the boot CPU's register and set this flag.  The secondaries will
/// Do The Right Thing during their early init.
///
/// Note: this variable is atomic and accessed with relaxed semantics, but it may
/// not even need to be that.  Counter selection (the only time this variable is
/// mutated) on ARM happens before the secondary CPUs are started and perform
/// their early init (the only time they will read this variable).  There should
/// be no real chance of a data race here.
static ALLOW_PCT_IN_EL0: AtomicBool = AtomicBool::new(false);

fn setup_cntkctl_el1() {
    // If the process of clock reference selection has forced us to use the
    // physical counter as our reference, make sure we give EL0 permission to
    // access it.  For now, we still allow access to the virtual counter because
    // there exists some code out there which actually tries to read the VCT in
    // user-mode directly.
    //
    // If/when this eventually changes, we should come back here and lock out
    // access to the VCT when we decide to use the PCT.
    const CNTKCTL_EL1_ENABLE_PHYSICAL_COUNTER: u64 = 1 << 0;
    const CNTKCTL_EL1_ENABLE_VIRTUAL_COUNTER: u64 = 1 << 1;
    let val = CNTKCTL_EL1_ENABLE_VIRTUAL_COUNTER
        | if ALLOW_PCT_IN_EL0.load(Ordering::SeqCst) {
            CNTKCTL_EL1_ENABLE_PHYSICAL_COUNTER
        } else {
            0
        };
    CNTKCTL_EL1.write(val);
    isb();
}

fn arm64_select_vbar_via_smccc11(function: ArmSmcccFunction) -> usize {
    let no_workaround = || -> usize {
        // No mitigation is needed on this CPU.
        // SAFETY: x20 points at the calling CPU's `arm64_percpu`, as it has since
        // `arm64_cpu_early_init`. C++ reads `should_invalidate_bp_on_el0_exception` as a `bool`
        // (READ_PERCPU_FIELD in exceptions_c.cc), and the value is a `bool`. Rust forms no
        // reference to the field.
        unsafe { write_percpu_field!(should_invalidate_bp_on_el0_exception, false) };
        // SAFETY: x20 as above. The context switch reads `should_invalidate_bp_on_context_switch`
        // as a `bool` through `read_percpu_field!`, and the value is a `bool`. Rust forms no
        // reference to the field.
        unsafe { write_percpu_field!(should_invalidate_bp_on_context_switch, false) };
        0
    };

    let use_workaround = || -> usize {
        // The workaround replaces the other EL0 entry mitigations.
        // SAFETY: x20 points at the calling CPU's `arm64_percpu`. C++ reads this field as a
        // `bool` (READ_PERCPU_FIELD in exceptions_c.cc), and the value is a `bool`.
        unsafe { write_percpu_field!(should_invalidate_bp_on_el0_exception, false) };

        // The EL0->EL1 entry mitigation is sufficient without the context-switch
        // mitigation too.
        // SAFETY: x20 points at the calling CPU's `arm64_percpu`. The context switch reads this
        // field as a `bool` through `read_percpu_field!`, and the value is a `bool`.
        unsafe { write_percpu_field!(should_invalidate_bp_on_context_switch, false) };

        (&raw const arm64_el1_exception_smccc11_workaround).addr()
    };

    let cpu_num = arch_curr_cpu_num();

    if BootOptions::get().arm64_alternate_vbar != BootOptionsAlternateVbar::Auto {
        dprintf!(
            INFO,
            "CPU {} using SMCCC_ARCH_WORKAROUND function {:#x} by boot option override\n",
            cpu_num,
            function as u32
        );
        return use_workaround();
    }

    // The workaround call is supported by the firmware on all CPUs.
    // Check on each individual CPU whether it needs to be used or not.
    // SAFETY: SMCCC_ARCH_FEATURES only asks the firmware whether it implements `function`, and the
    // firmware saves the registers the call does not return in.
    let value = unsafe { ArmSmcccCall(ArmSmcccFunction::SmcccArchFeatures, function as u64, 0, 0) };

    match value {
        0 => {
            dprintf!(
                INFO,
                "CPU {} firmware requires SMCCC_ARCH_WORKAROUND function {:#x}\n",
                cpu_num,
                function as u32
            );
            return use_workaround();
        }

        1 => {
            dprintf!(
                INFO,
                "CPU {} firmware reports SMCCC_ARCH_WORKAROUND function {:#x} not needed\n",
                cpu_num,
                function as u32
            );
            return no_workaround();
        }

        _ => {
            dprintf!(
                CRITICAL,
                "WARNING: Possible SMCCC firmware bug:  SMCCC_ARCH_FEATURES reports {} for {:#x} \
                 on CPU {} but boot CPU reported it supported!\n",
                value as i64,
                function as u32,
                cpu_num
            );
        }
    }

    0
}

/// Select the alternate exception vector to use for the current CPU.
/// Returns 0 to keep using the default one.
fn arm64_select_vbar() -> usize {
    // In auto mode, the physboot detection code has "selected" a firmware option
    // if it's available generally.  The logic here then chooses whether this
    // particular CPU needs to use that firmware option by asking the firmware.
    match ALTERNATE_VBAR.get() {
        Arm64AlternateVbar::ArchWorkaround3 => {
            return arm64_select_vbar_via_smccc11(ArmSmcccFunction::SmcccArchWorkaround3);
        }
        Arm64AlternateVbar::ArchWorkaround1 => {
            return arm64_select_vbar_via_smccc11(ArmSmcccFunction::SmcccArchWorkaround1);
        }
        Arm64AlternateVbar::PsciVersion => {
            // TODO(https://fxbug.dev/322202704): Auto-select based on core IDs?
            dprintf!(
                INFO,
                "CPU {} using SMCCC 1.1 PSCI_VERSION in lieu of SMCCC_ARCH_WORKAROUND\n",
                arch_curr_cpu_num()
            );
            return (&raw const arm64_el1_exception_smccc11_workaround).addr();
        }
        Arm64AlternateVbar::Smccc10 => {
            // TODO(https://fxbug.dev/322202704): Auto-select based on core IDs?
            dprintf!(
                INFO,
                "CPU {} using SMCCC 1.0 PSCI_VERSION in lieu of SMCCC 1.1 support\n",
                arch_curr_cpu_num()
            );
            return (&raw const arm64_el1_exception_smccc10_workaround).addr();
        }
        Arm64AlternateVbar::None => {
            if BootOptions::get().arm64_alternate_vbar == BootOptionsAlternateVbar::None {
                dprintf!(
                    INFO,
                    "CPU {} not using any workaround by explicit boot option\n",
                    arch_curr_cpu_num()
                );
            } else {
                assert!(BootOptions::get().arm64_alternate_vbar == BootOptionsAlternateVbar::Auto);
                // TODO(https://fxbug.dev/322202704): fall back to branch loop?
                // Just panic on known cores with issues when firmware is lacking?
                dprintf!(
                    INFO,
                    "CPU {} has no SMCCC workaround function configured\n",
                    arch_curr_cpu_num()
                );
            }
        }
        Arm64AlternateVbar::Auto => {
            panic!("physboot handoff should have performed auto-selection!");
        }
    }
    0
}

/// Set the vector base.
fn arm64_install_vbar(table: usize) {
    VBAR_EL1.write(VectorBaseAddressRegister::from(table as u64));
    isb();
}

#[inline(always)]
fn arm64_get_boot_el() -> u64 {
    u64::from(CURRENT_EL.read().el())
}

/// Puts the calling cpu in a working state and reads its feature flags.
///
/// # Safety
///
/// Each CPU calls this as it starts. Its mpid is registered with `arch_register_mpid`, and
/// nothing uses per-CPU state through x20 before the CPU's first `arm64_init_percpu_early`
/// returns: on the boot CPU, `ArchPostHandoffBootstrap` made that call. The boot CPU calls this
/// before any other CPU starts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_cpu_early_init() {
    // Make sure the per cpu pointer is set up.
    // SAFETY: The caller guarantees that this CPU's mpid is registered and that nothing uses
    // per-CPU state through x20 before its first call returns. A later call, as on the boot CPU,
    // rewrites the same x20 value and `microarch`.
    unsafe { arm64_init_percpu_early() };

    // Initially use the primary vector table.
    // arch_late_init_percpu may change its mind.
    arm64_install_vbar((&raw const arm64_el1_exception).addr());

    // Set up main control bits for this cpu.
    // Taken from ARM DDI 0487 rev L.a section D24.2.167.
    let mut sctlr = SystemControlRegisterEl1::from(0);
    sctlr
        .set_lsmaoe(true) // Ordering of arm32 load/store multiple is defined for armv8.0.
        .set_ntlsmd(true) // Do not trap arm32 load/store multiple instructions on device mem.
        .set_uci(true) // Do not trap DC cache instructions in EL0.
        .set_eis(true) // Exception entries are synchronizing events.
        .set_span(true) // Do not change PSTATE.PAN on exception.
        .set_tscxt(true) // EL0 access to SCXTNUM_EL0 is disabled.
        .set_ntwe(true) // Do not trap WFE in EL0.
        .set_uct(true) // Do not trap CTR_EL0 in EL0
        .set_dze(true) // Do not trap DZ ZVA in EL0.
        .set_i(true) // Instruction cache enable.
        .set_eos(true) // Exception exits are synchronizing events.
        .set_sed(true) // arm32 EL0 cannot use SETEND instruction.
        .set_sa0(true) // Stack pointer alignment in EL0.
        .set_sa(true) // Stack pointer alignment in EL1.
        .set_c(true) // Data cache enable.
        .set_m(true); // MMU Enable.
    // SAFETY: This is the configuration every CPU runs the kernel with (it also leaves EE, EnIA,
    // EnIB and BT clear). The MMU and caches stay on, translating through the kernel page tables
    // this CPU already runs on.
    unsafe { SCTLR_EL1.write(sctlr) };
    isb();

    // Enable the FPU, disabling any other additional vector units.
    let mut cpacr = ArchitecturalFeatureAccessControlRegister::from(0);
    cpacr.set_fpen(CpacrTrap::None);
    CPACR_EL1.write(cpacr);
    isb();

    // Save all of the features of the cpu.
    // SAFETY: The caller guarantees that this CPU is starting, and that the boot CPU gets here
    // before any other CPU starts.
    unsafe { arm64_feature_init() };

    // If FEAT_SVE is available, enable it and configure a 128-bit vector length.
    if arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE) {
        cpacr.set_zen(CpacrTrap::None);
        CPACR_EL1.write(cpacr);
        isb();

        let mut zcr = ZCR_EL1.read();
        zcr.set_len(0); // 128-bit vector length
        ZCR_EL1.write(zcr);
        isb();
    }

    // If FEAT_MOPS is available, enable it for EL0.
    // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_isa_features`, before any
    // other CPU starts. On the boot CPU, that call just returned.
    if unsafe { arm64_isa_features } & ZX_ARM64_FEATURE_ISA_MOPS != 0 {
        sctlr = SCTLR_EL1.read();
        sctlr.set_mscen(true);
        // SAFETY: Setting MSCEn only stops EL0 FEAT_MOPS instructions from trapping.
        unsafe { SCTLR_EL1.write(sctlr) };
        isb();
    }

    // Check for TCR2 and SCTLR2 and zero since none of their features are used.
    let mmfr3 = ID_AA64MMFR3_EL1.read();
    if mmfr3.tcrx() != 0 {
        let tcr2 = ExtendedTranslationControlRegister::from(0);
        // SAFETY: FEAT_TCR2 is implemented, and zero leaves every extension it controls off,
        // which is what the kernel page tables assume.
        unsafe { TCR2_EL1.write(tcr2) };
        isb();
    }
    if mmfr3.sctlrx() != 0 {
        let sctlr2 = SystemControlRegister2El1::from(0);
        // SAFETY: FEAT_SCTLR2 is implemented, and zero leaves every extension it controls off.
        unsafe { SCTLR2_EL1.write(sctlr2) };
        isb();
    }

    // Enable PAN if present.
    // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_mmu_features`, before any
    // other CPU starts. On the boot CPU, that call just returned.
    if unsafe { arm64_mmu_features.pan } {
        // SAFETY: `arm64_mmu_features.pan` was just checked.
        unsafe { arm64_enable_pan() };

        // By setting SCTLR.SPAN to 0, the cpu will set PSTATE.PAN on every exception.
        sctlr = SCTLR_EL1.read();
        sctlr.set_span(false);
        // SAFETY: FEAT_PAN is implemented, and the user copy routines clear PSTATE.PAN around
        // each access to user memory.
        unsafe { SCTLR_EL1.write(sctlr) };
        isb();
    }

    // Enable cycle counter, if FEAT_PMUv3 is enabled.
    // SAFETY: Only the boot CPU's `arm64_feature_init` writes `feat_pmuv3_enabled`, before any
    // other CPU starts. On the boot CPU, that call just returned.
    if unsafe { feat_pmuv3_enabled } {
        PMCR_EL0.write(PMCR_EL0_ENABLE_BIT | PMCR_EL0_LONG_COUNTER_BIT);
        isb();
        PMCNTENSET_EL0.write(PMCNTENSET_EL0_ENABLE);
        isb();

        // Enable user space access to cycle counter.
        PMUSERENR_EL0.write(PMUSERENR_EL0_CR_ENABLE);
        isb();
    }

    // Enable Debug Exceptions by Disabling the OS Lock. The OSLAR_EL1 is a WO
    // register with only the low bit defined as OSLK. Write 0 to disable.
    OSLAR_EL1.write(0x0);
    isb();

    // Give EL0 access to the chosen reference counter, but nothing else.
    setup_cntkctl_el1();

    MDSCR_EL1.write(MonitorDebugSystemControlRegister::from(u64::from(MSDCR_EL1_INITIAL_VALUE)));
    isb();
}

/// Early platform initialization, before UART, MMU, kernel command line args, etc.
#[unsafe(no_mangle)]
pub extern "C" fn arch_early_init() {
    // Collect the setting that physboot determined.  arch_late_init_percpu()
    // will call arm64_select_vbar to use it later, when gPhysHandoff may no
    // longer be available.
    // SAFETY: `cpp_phys_handoff_get` takes no arguments and only reads `gPhysHandoff`.
    debug_assert!(!unsafe { cpp_phys_handoff_get() }.is_null());
    // SAFETY: The kernel calls `arch_early_init` once, on the boot CPU, before any other CPU
    // starts, so nothing reads `ALTERNATE_VBAR` concurrently. Physboot stored a valid
    // `Arm64AlternateVbar` in the hand-off.
    unsafe { ALTERNATE_VBAR.init(PhysHandoff::get().arch_handoff.alternate_vbar) };

    // put the cpu in a working state and read the feature flags
    // SAFETY: `ArchPostHandoffBootstrap` registered the boot CPU's mpid and made its first
    // `arm64_init_percpu_early` call, and no other CPU has started.
    unsafe { arm64_cpu_early_init() };
}

/// Perform any set up required before virtual memory is enabled, or the heap is set up.
#[unsafe(no_mangle)]
pub extern "C" fn arch_prevm_init() {}

/// Perform any set up required after heap/MMU is available.
#[unsafe(no_mangle)]
pub extern "C" fn arch_init() {
    arch_mp_init_percpu();

    dprintf!(INFO, "ARM boot EL{}\n", arm64_get_boot_el());

    // SAFETY: The kernel calls `arch_init` on the boot CPU, which finished `arm64_feature_init`
    // in `arch_early_init`.
    unsafe { arm64_feature_debug(true) };

    let max_cpus = arch_max_num_cpus();
    let mut cmdline_max_cpus = BootOptions::get().smp_max_cpus;
    if cmdline_max_cpus > max_cpus || cmdline_max_cpus == 0 {
        kprintln!("invalid kernel.smp.maxcpus value, defaulting to {}", max_cpus);
        cmdline_max_cpus = max_cpus;
    }

    // SAFETY: The function takes a plain count, at most the number of CPUs the platform found.
    unsafe { lk_init_secondary_cpus(cmdline_max_cpus - 1) };
}

/// Perform any per-CPU set up required.
#[unsafe(no_mangle)]
pub extern "C" fn arch_late_init_percpu() {
    let need_spectre_v2_mitigation = !BootOptions::get().arm64_disable_spec_mitigations
        && arm64_uarch_needs_spectre_v2_mitigation();

    // These may be reset in arm64_select_vbar() when something better is chosen.
    // SAFETY: x20 points at the calling CPU's `arm64_percpu`, as it has since
    // `arm64_cpu_early_init`. The context switch reads `should_invalidate_bp_on_context_switch` as
    // a `bool` through `read_percpu_field!`, and the value is a `bool`. Rust forms no reference to
    // the field.
    unsafe {
        write_percpu_field!(should_invalidate_bp_on_context_switch, need_spectre_v2_mitigation)
    };
    // SAFETY: x20 as above. C++ reads `should_invalidate_bp_on_el0_exception` as a `bool`
    // (READ_PERCPU_FIELD in exceptions_c.cc), and the value is a `bool`. Rust forms no reference
    // to the field.
    unsafe {
        write_percpu_field!(should_invalidate_bp_on_el0_exception, need_spectre_v2_mitigation)
    };

    // Decide if this CPU needs an alternative exception vector table.
    let vector_table = arm64_select_vbar();
    if vector_table != 0 {
        arm64_install_vbar(vector_table);
    }
}

/// Puts the calling CPU in a low-power state until an interrupt arrives. Backs
/// `ArchIdlePowerThread::EnterIdleState()`, whose contract the caller follows.
#[unsafe(no_mangle)]
pub extern "C" fn arch_enter_idle_state() {
    // section K14.2.3 of the ARM ARM (DDI 0487K.a) says:
    //
    // ```
    // The Wait For Event and Wait For Interrupt instructions permit the PE to
    // suspend execution and enter a low-power state. An explicit DSB barrier
    // instruction is required if it is necessary to ensure memory accesses made
    // before the WFI or WFE are visible to other observers, unless some other
    // mechanism has ensured this visibility.
    // ```
    //
    // Our PE is entering the idle/suspend state; don't take any chances.  Make
    // certain that all of the writes we have performed (such as reporting that we
    // are entering the idle state) are visible to all other PEs by executing an
    // explicit DSB.
    //
    device_memory_barrier();
    wfi();
}

/// Fill in `*out` with the `Iframe` that should be passed to `arch_enter_uspace()` on the current
/// thread, and set the user thread pointer.
///
/// # Safety
///
/// `out` is valid for writes of an `Iframe`. It need not be initialized; every field is written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_prepare_uspace(state: &UserEntryState, out: *mut Iframe) {
    let mut iframe = Iframe {
        elr: state.pc,

        // Set up a default spsr to get into 64bit user space:
        //  - Zeroed NZCV.
        //  - No SS, no IL, no D.
        //  - All interrupts enabled.
        //  - Mode 0: EL0t.
        spsr: 0,

        usp: state.sp,
        ..Default::default()
    };

    iframe.r[0] = state.arg1;
    iframe.r[1] = state.arg2;
    iframe.r[18] = state.abi_reg;

    // The thread pointer isn't in the iframe.  It's directly in the TPIDR_EL0
    // system register for the current thread.
    TPIDR_EL0.write(state.tp);

    // SAFETY: The caller guarantees that `out` is valid for writes of an `Iframe`.
    unsafe { out.write(iframe) };
}

/// Switch to user mode, set the user stack pointer to user_stack_top, put the svc stack pointer to
/// the top of the kernel stack.
///
/// # Safety
///
/// `iframe` points to an `Iframe` that `arch_prepare_uspace` (or restricted mode) filled in for
/// the current thread. Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_enter_uspace(iframe: *const Iframe) -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());

    let ct = thread::current_get();
    // SAFETY: The caller guarantees `iframe`.
    let iframe_ref = unsafe { &*iframe };
    // SAFETY: `ct` is the current thread, whose stack is allocated.
    let stack_top = unsafe { get_stack_top(ct) };

    ltracef!(
        "r0 {:#x} r1 {:#x} spsr {:#x} st {:#x} usp {:#x} pc {:#x}\n",
        iframe_ref.r[0],
        iframe_ref.r[1],
        iframe_ref.spsr,
        stack_top,
        iframe_ref.usp,
        iframe_ref.elr
    );
    // Kernel Rust always has the shadow call stack, as asserted at the top of this file, so this
    // and the entry below are the C++ `__has_feature(shadow_call_stack)` paths.
    // SAFETY: As above.
    let scsp_base = unsafe { get_shadow_call_base(ct) };
    ltracef!(
        "scsp {:p}, scsp base {:#x}\n",
        // SAFETY: `ct` is the current thread, and only it writes its `shadow_call_sp`.
        unsafe { (*thread_arch_const(ct)).shadow_call_sp },
        scsp_base
    );

    assert!(is_valid_user_pc(iframe_ref.elr as usize));

    // SAFETY: `iframe` holds a user `pc` checked above. `stack_top` is the top of the current
    // thread's kernel stack and `scsp_base` the base of its shadow call stack; the entry abandons
    // what is on them, which nothing needs again. Interrupts are disabled, as the entry requires.
    unsafe { arm64_uspace_entry(iframe, stack_top, scsp_base) }
}

/// Called during clock selection (if it is called at all) before secondary CPUs
/// have started.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_allow_pct_in_el0() {
    ALLOW_PCT_IN_EL0.store(true, Ordering::Relaxed);
    setup_cntkctl_el1();
}

/// Sets up the boot CPU right after the kernel takes over from physboot: clears the phys
/// exception vectors, switches TCR_EL1 to the kernel-only configuration, registers the boot CPU
/// as CPU 0 and sets up its per-CPU pointer.
///
/// # Safety
///
/// The kernel calls this once, on the boot CPU, before anything uses per-CPU state or runs from
/// the TTBR0 trampoline mapping. `_arch_handoff` is not read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ArchPostHandoffBootstrap(_arch_handoff: *const ArchPhysHandoff) {
    // Clear any phys exception handlers.
    VBAR_EL1.write(VectorBaseAddressRegister::from(0));

    // Disable trampoline page-table in ttbr0
    // SAFETY: The value keeps the kernel's TTBR1 configuration and disables TTBR0 walks. The
    // caller guarantees that nothing runs from the TTBR0 trampoline mapping any more.
    unsafe { TCR_EL1.write(TranslationControlRegisterEl1::from(kMmuTcrFlagsKernel)) };

    // Invalidate the entire TLB
    invalidate_local_tlbs();
    device_memory_barrier();
    isb();

    // save the mpid for cpu 0
    let mpidr = MPIDR_EL1_RAW.read();
    // SAFETY: CPU 0 is the calling CPU.
    unsafe { arch_register_mpid(0, mpidr & ARM64_MPIDR_MASK) };

    // set the per cpu pointer for cpu 0
    // SAFETY: CPU 0's mpid was just registered, and the caller guarantees that nothing has used
    // per-CPU state yet.
    unsafe { arm64_init_percpu_early() };
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs`, and `argc` is at least 1.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_cpu(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!(
            "{:s} sev                              : issue a SEV (Send Event) instruction",
            argv[0].as_str()
        );
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }

    if argv[1].as_str() == "sev" {
        libarch::arm64::sev();
        kprintln!("done");
    } else {
        kprintln!("unknown command");
        return usage();
    }

    ZX_OK
}

#[cfg(console_enabled)]
static_command!(
    CMD_CPU,
    c"cpu".as_ptr(),
    c"cpu diagnostic commands".as_ptr(),
    cmd_cpu,
    CMD_AVAIL_NORMAL
);
