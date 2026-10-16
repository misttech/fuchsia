// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor ktrace helpers mirroring `<hypervisor/ktrace.h>`.

use core::mem::{align_of, size_of};
use zr::static_assert;

use crate::ktrace_rs::{InternedString, declare_interned_string, duration_begin, duration_end};

/// Metadata categories for VCPU blocking operations.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VcpuMeta {
    /// Waiting for an interrupt.
    Interrupt = 0,
    /// Waiting for a port packet.
    Port = 1,
}

const VCPU_META_COUNT: usize = 2;

static_assert!(size_of::<VcpuMeta>() == size_of::<u8>());
static_assert!(align_of::<VcpuMeta>() == align_of::<u8>());

/// VCPU exit reasons.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VcpuExit {
    /// GIC underflow maintenance interrupt (arm64).
    #[cfg(target_arch = "aarch64")]
    UnderflowMaintenanceInterrupt,
    /// Physical interrupt (arm64).
    #[cfg(target_arch = "aarch64")]
    PhysicalInterrupt,
    /// WFI instruction trap (arm64).
    #[cfg(target_arch = "aarch64")]
    WfiInstruction,
    /// WFE instruction trap (arm64).
    #[cfg(target_arch = "aarch64")]
    WfeInstruction,
    /// SMC instruction trap (arm64).
    #[cfg(target_arch = "aarch64")]
    SmcInstruction,
    /// System register access trap (arm64).
    #[cfg(target_arch = "aarch64")]
    SystemInstruction,
    /// Stage-2 instruction abort (arm64).
    #[cfg(target_arch = "aarch64")]
    InstructionAbort,
    /// Stage-2 data abort (arm64).
    #[cfg(target_arch = "aarch64")]
    DataAbort,
    /// SError interrupt (arm64).
    #[cfg(target_arch = "aarch64")]
    SerrorInterrupt,

    /// Exception or non-maskable interrupt (x86_64).
    #[cfg(target_arch = "x86_64")]
    ExceptionOrNmi,
    /// External interrupt (x86_64).
    #[cfg(target_arch = "x86_64")]
    ExternalInterrupt,
    /// Interrupt-window exiting (x86_64).
    #[cfg(target_arch = "x86_64")]
    InterruptWindow,
    /// CPUID instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Cpuid,
    /// HLT instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Hlt,
    /// Control register access trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    ControlRegisterAccess,
    /// I/O instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    IoInstruction,
    /// RDMSR instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Rdmsr,
    /// WRMSR instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Wrmsr,
    /// VM-entry failure (x86_64).
    #[cfg(target_arch = "x86_64")]
    VmEntryFailure,
    /// EPT violation (x86_64).
    #[cfg(target_arch = "x86_64")]
    EptViolation,
    /// XSETBV instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Xsetbv,
    /// PAUSE instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Pause,
    /// VMCALL instruction trap (x86_64).
    #[cfg(target_arch = "x86_64")]
    Vmcall,

    /// Unsupported guest operation or exit reason.
    NotSupported,
    /// Internal hypervisor failure handling VCPU exit.
    Failure,
}

const VCPU_EXIT_COUNT: usize = VcpuExit::Failure as usize + 1;

static_assert!(size_of::<VcpuExit>() == size_of::<u8>());
static_assert!(align_of::<VcpuExit>() == align_of::<u8>());

/// VCPU block/unblock operation type.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VcpuBlockOp {
    /// VCPU begins blocking.
    Block = 0,
    /// VCPU finishes blocking.
    Unblock = 1,
}

static_assert!(size_of::<VcpuBlockOp>() == size_of::<u8>());
static_assert!(align_of::<VcpuBlockOp>() == align_of::<u8>());

declare_interned_string!(WAIT_INTERRUPT, "wait:interrupt");
declare_interned_string!(WAIT_PORT, "wait:port");
declare_interned_string!(VCPU_META_FALLBACK, "vcpu meta");

static VCPU_META: [&InternedString; VCPU_META_COUNT] = [WAIT_INTERRUPT, WAIT_PORT];

#[cfg(target_arch = "aarch64")]
mod exit_strings {
    use super::VCPU_EXIT_COUNT;
    use crate::ktrace_rs::{InternedString, declare_interned_string};

    declare_interned_string!(
        UNDERFLOW_MAINTENANCE_INTERRUPT,
        "exit:underflow_maintenance_interrupt"
    );
    declare_interned_string!(PHYSICAL_INTERRUPT, "exit:physical_interrupt");
    declare_interned_string!(WFI_INSTRUCTION, "exit:wfi_instruction");
    declare_interned_string!(WFE_INSTRUCTION, "exit:wfe_instruction");
    declare_interned_string!(SMC_INSTRUCTION, "exit:smc_instruction");
    declare_interned_string!(SYSTEM_INSTRUCTION, "exit:system_instruction");
    declare_interned_string!(INSTRUCTION_ABORT, "exit:instruction_abort");
    declare_interned_string!(DATA_ABORT, "exit:data_abort");
    declare_interned_string!(SERROR_INTERRUPT, "exit:serror_interrupt");
    declare_interned_string!(NOT_SUPPORTED, "exit:not_supported");
    declare_interned_string!(FAILURE, "exit:failure");

    pub(super) static VCPU_EXIT: [&InternedString; VCPU_EXIT_COUNT] = [
        UNDERFLOW_MAINTENANCE_INTERRUPT,
        PHYSICAL_INTERRUPT,
        WFI_INSTRUCTION,
        WFE_INSTRUCTION,
        SMC_INSTRUCTION,
        SYSTEM_INSTRUCTION,
        INSTRUCTION_ABORT,
        DATA_ABORT,
        SERROR_INTERRUPT,
        NOT_SUPPORTED,
        FAILURE,
    ];
}

#[cfg(target_arch = "x86_64")]
mod exit_strings {
    use super::VCPU_EXIT_COUNT;
    use crate::ktrace_rs::{InternedString, declare_interned_string};

    declare_interned_string!(EXCEPTION_OR_NMI, "exit:exception_or_nmi");
    declare_interned_string!(EXTERNAL_INTERRUPT, "exit:external_interrupt");
    declare_interned_string!(INTERRUPT_WINDOW, "exit:interrupt_window");
    declare_interned_string!(CPUID, "exit:cpuid");
    declare_interned_string!(HLT, "exit:hlt");
    declare_interned_string!(CONTROL_REGISTER_ACCESS, "exit:control_register_access");
    declare_interned_string!(IO_INSTRUCTION, "exit:io_instruction");
    declare_interned_string!(RDMSR, "exit:rdmsr");
    declare_interned_string!(WRMSR, "exit:wrmsr");
    declare_interned_string!(VM_ENTRY_FAILURE, "exit:vm_entry_failure");
    declare_interned_string!(EPT_VIOLATION, "exit:ept_violation");
    declare_interned_string!(XSETBV, "exit:xsetbv");
    declare_interned_string!(PAUSE, "exit:pause");
    declare_interned_string!(VMCALL, "exit:vmcall");
    declare_interned_string!(NOT_SUPPORTED, "exit:not_supported");
    declare_interned_string!(FAILURE, "exit:failure");

    pub(super) static VCPU_EXIT: [&InternedString; VCPU_EXIT_COUNT] = [
        EXCEPTION_OR_NMI,
        EXTERNAL_INTERRUPT,
        INTERRUPT_WINDOW,
        CPUID,
        HLT,
        CONTROL_REGISTER_ACCESS,
        IO_INSTRUCTION,
        RDMSR,
        WRMSR,
        VM_ENTRY_FAILURE,
        EPT_VIOLATION,
        XSETBV,
        PAUSE,
        VMCALL,
        NOT_SUPPORTED,
        FAILURE,
    ];
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod exit_strings {
    use super::VCPU_EXIT_COUNT;
    use crate::ktrace_rs::{InternedString, declare_interned_string};

    declare_interned_string!(NOT_SUPPORTED, "exit:not_supported");
    declare_interned_string!(FAILURE, "exit:failure");

    pub(super) static VCPU_EXIT: [&InternedString; VCPU_EXIT_COUNT] = [NOT_SUPPORTED, FAILURE];
}

/// Records a VCPU block or unblock ktrace duration event.
pub fn ktrace_vcpu(block_op: VcpuBlockOp, meta: VcpuMeta) {
    let name = VCPU_META.get(meta as usize).copied().unwrap_or(VCPU_META_FALLBACK);
    match block_op {
        VcpuBlockOp::Block => {
            duration_begin!("kernel:arch", name, "meta #" => meta as u32);
        }
        VcpuBlockOp::Unblock => {
            duration_end!("kernel:arch", name);
        }
    }
}

/// Records a VCPU exit ktrace duration end event.
pub fn ktrace_vcpu_exit(exit: VcpuExit, exit_address: u64) {
    if let Some(&exit_type) = exit_strings::VCPU_EXIT.get(exit as usize) {
        duration_end!(
            "kernel:arch",
            "vcpu",
            "exit_address" => exit_address,
            "exit_type" => exit_type,
        );
    } else {
        duration_end!(
            "kernel:arch",
            "vcpu",
            "exit_address" => exit_address,
            "exit_type" => exit as u32,
        );
    }
}
