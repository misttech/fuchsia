// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Dispatch of the physboot driver handoff to the platform's drivers.

use crate::arch_rs::riscv64::timer::{DcfgRiscvGenericTimerDriver, riscv_generic_timer_init_early};
use crate::plic::{DcfgRiscvPlicDriver, plic_init_early, plic_init_late, plic_init_post_vm};

/// Early driver initialization from the handoff: the PLIC and the generic
/// timer, each only if physboot handed off a driver configuration for it.
///
/// # Safety
///
/// Each configuration that is `Some` must be a live driver configuration from
/// the physboot handoff.  Must be called once, on the boot CPU with interrupts
/// disabled, before secondary CPUs are started.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_driver_handoff_early(
    plic_driver: Option<&DcfgRiscvPlicDriver>,
    timer_driver: Option<&DcfgRiscvGenericTimerDriver>,
) {
    if let Some(plic) = plic_driver {
        plic_init_early(plic);
    }
    if let Some(timer) = timer_driver {
        // SAFETY: the caller upholds the one-time, boot-CPU contract above.
        unsafe { riscv_generic_timer_init_early(timer) };
    }
}

/// Post-VM driver initialization from the handoff: the PLIC, if configured.
///
/// # Safety
///
/// A `Some` configuration must be a live driver configuration from the
/// physboot handoff.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_driver_handoff_post_vm(
    plic_driver: Option<&DcfgRiscvPlicDriver>,
) {
    if let Some(plic) = plic_driver {
        plic_init_post_vm(plic);
    }
}

/// Late driver initialization from the handoff: the PLIC, if configured.
///
/// # Safety
///
/// A `Some` configuration must be a live driver configuration from the
/// physboot handoff.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_driver_handoff_late(
    plic_driver: Option<&DcfgRiscvPlicDriver>,
) {
    if let Some(plic) = plic_driver {
        plic_init_late(plic);
    }
}
