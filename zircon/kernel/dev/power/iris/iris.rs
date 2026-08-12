// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/power/iris/power.cc

use crate::pdev_power::{PdevPowerOps, PowerCpuState, PowerRebootFlags, rust_pdev_register_power};
use debug::dprintf;
#[cfg(ktest)]
use unittest as _;
use zx_status::Status;

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Iris.
const VENDOR_SPECIFIC_WARM_RESET_TYPE: u32 = 0x8000_0000;

unsafe extern "C" {
    fn psci_system_reset_cold() -> i32;
    fn psci_system_reset2_raw(reset_type: u32, cookie: u32) -> i32;
    fn psci_system_off() -> i32;
    fn psci_cpu_off() -> i32;
    fn psci_cpu_on(hw_cpu_id: u64, entry: u64, context: u64) -> i32;
    fn psci_get_cpu_state(hw_cpu_id: u64, out_state: *mut PowerCpuState) -> i32;
}

/// Reboots the system via PSCI cold reset or vendor-specific warm reset for panic.
extern "C" fn iris_reboot(flags: PowerRebootFlags) -> i32 {
    match flags {
        PowerRebootFlags::Normal | PowerRebootFlags::Bootloader | PowerRebootFlags::Recovery => {
            dprintf!(INFO, "Iris reboot: performing cold reset\n");
            // SAFETY: PSCI cold reset call to hardware firmware.
            unsafe { psci_system_reset_cold() }
        }
        PowerRebootFlags::Panic => {
            dprintf!(INFO, "Iris panic reboot: performing warm reset\n");
            // SAFETY: On Iris VENDOR_SPECIFIC_WARM_RESET_TYPE ignores the cookie parameter.
            unsafe { psci_system_reset2_raw(VENDOR_SPECIFIC_WARM_RESET_TYPE, 0) }
        }
    }
}

/// Shuts down the system via PSCI system off call.
extern "C" fn iris_shutdown() -> i32 {
    // SAFETY: PSCI system off call to hardware firmware.
    unsafe { psci_system_off() }
}

/// Powers off the calling CPU core via PSCI cpu off call.
extern "C" fn iris_cpu_off() -> i32 {
    // SAFETY: PSCI CPU off call.
    unsafe { psci_cpu_off() }
}

/// Powers on the CPU core with the specified hardware ID via PSCI.
extern "C" fn iris_cpu_on(hw_cpu_id: u64, entry: u64, context: u64) -> i32 {
    // SAFETY: PSCI CPU on call.
    unsafe { psci_cpu_on(hw_cpu_id, entry, context) }
}

/// Retrieves the current power state of the CPU core with the specified hardware ID.
extern "C" fn iris_get_cpu_state(hw_cpu_id: u64, out_state: *mut PowerCpuState) -> i32 {
    if out_state.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_state` was checked non-null above.
    unsafe { psci_get_cpu_state(hw_cpu_id, out_state) }
}

static IRIS_POWER_OPS: PdevPowerOps = PdevPowerOps {
    reboot: Some(iris_reboot),
    shutdown: Some(iris_shutdown),
    cpu_off: Some(iris_cpu_off),
    cpu_on: Some(iris_cpu_on),
    get_cpu_state: Some(iris_get_cpu_state),
    opp_set: None,
    opp_get: None,
    opp_get_domain_count: None,
};

/// Early initialization hook for Iris power management.
#[unsafe(no_mangle)]
pub extern "C" fn iris_power_init_early() {
    dprintf!(INFO, "POWER: registering iris power hooks\n");
    // SAFETY: IRIS_POWER_OPS has static lifetime and remains valid for the lifetime of the kernel.
    unsafe {
        rust_pdev_register_power(&IRIS_POWER_OPS);
    }
}

/// In-kernel unit tests for the Iris power driver.
#[cfg(ktest)]
#[unittest::suite(name = "iris_power")]
mod tests {
    use unittest::assert_eq;
    use zx_status::Status;

    /// Tests that passing a null output pointer to get_cpu_state returns INVALID_ARGS.
    #[test]
    fn test_iris_get_cpu_state_null_arg() {
        assert_eq!(
            super::iris_get_cpu_state(0, core::ptr::null_mut()),
            Status::INVALID_ARGS.into_raw()
        );
    }
}
