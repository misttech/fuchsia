// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for the platform device (pdev) power driver interface.

use zx_status::Status;

use super::{
    PdevPowerOps, PowerCpuState, PowerDomainConfigFfi, PowerRebootFlags, pdev_register_power,
    pdev_swap_power_for_test, power_cpu_off, power_cpu_on, power_get_cpu_state, power_reboot,
    power_shutdown,
};

unsafe extern "C" {
    /// Registers the provided power domain configurations and energy models with the kernel
    /// scheduler.
    ///
    /// # Safety
    ///
    /// If `domain_count > 0`, `domains` must be non-null, properly aligned, and valid for reads of
    /// `domain_count` consecutive initialized [`PowerDomainConfigFfi`] elements for the duration
    /// of this call.
    pub fn cpp_power_management_register_domains(
        domains: *const PowerDomainConfigFfi,
        domain_count: usize,
    ) -> Result<(), Status>;

    /// Sets the minimum and maximum processing rate limits for CPUs in `cpu_mask`.
    ///
    /// # Safety
    ///
    /// Must only be called after the kernel scheduler and power domain registry have been
    /// initialized.
    pub fn cpp_power_management_set_rate_limits(
        cpu_mask: u64,
        min_rate: u64,
        max_rate: u64,
    ) -> Result<(), Status>;

    /// Returns whether the boot-time CPU performance boost is enabled in kernel boot options.
    ///
    /// # Safety
    ///
    /// `BootOptions::Get()` must have been initialized prior to calling this function.
    pub fn cpp_power_management_boot_boost_enabled() -> bool;
}

/// Registers the platform power operations table from C-ABI.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevPowerOps` table that remains valid
/// for the duration of the kernel's execution (`'static` lifetime).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_register_power(ops: *const PdevPowerOps) {
    // SAFETY: Caller guarantees that if `ops` is non-null, it points to a valid `PdevPowerOps`
    // table with `'static` lifetime.
    let ops_ref = unsafe { ops.as_ref() };
    if let Some(ops_ref) = ops_ref {
        pdev_register_power(ops_ref);
    } else {
        // SAFETY: Passing null resets `POWER_OPS` to `DEFAULT_OPS`.
        unsafe {
            pdev_swap_power_for_test(core::ptr::null());
        }
    }
}

/// Swaps the current power operations table with a new one for testing, returning the previous
/// table.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevPowerOps` table for the duration of the test.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_swap_power_for_test(
    ops: *const PdevPowerOps,
) -> *const PdevPowerOps {
    // SAFETY: Caller upholds the safety invariants of `pdev_swap_power_for_test`.
    unsafe { pdev_swap_power_for_test(ops) }
}

/// Reboots the system using the registered power operations.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_reboot(flags: PowerRebootFlags) {
    power_reboot(flags);
}

/// Shuts down the system using the registered power operations.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_shutdown() {
    power_shutdown();
}

/// Powers off the calling CPU.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_cpu_off() -> Result<(), Status> {
    power_cpu_off()
}

/// Powers on the CPU with the specified hardware ID.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_cpu_on(
    hw_cpu_id: u64,
    entry: u64,
    context: u64,
) -> Result<(), Status> {
    power_cpu_on(hw_cpu_id, entry, context)
}

/// Retrieves the power state of the CPU with the specified hardware ID.
///
/// # Safety
///
/// `out_state` must be either null (which returns `Err(Status::INVALID_ARGS)`) or a valid,
/// well-aligned, writable pointer to a `PowerCpuState` for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_get_cpu_state(
    hw_cpu_id: u64,
    out_state: *mut PowerCpuState,
) -> Result<(), Status> {
    if out_state.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let state = power_get_cpu_state(hw_cpu_id)?;
    // SAFETY: `out_state` was verified non-null above and caller guarantees it points to valid,
    // aligned, writable memory for a `PowerCpuState`.
    unsafe {
        *out_state = state;
    }
    Ok(())
}
