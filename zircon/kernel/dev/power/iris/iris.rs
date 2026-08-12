// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/power/iris/power.cc

use crate::pdev_power::{PdevPowerOps, PowerCpuState, PowerRebootFlags, rust_pdev_register_power};
use core::sync::atomic::{AtomicPtr, Ordering};
use debug::dprintf;
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
#[cfg(ktest)]
use unittest as _;
use zx_status::Status;

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Iris.
const VENDOR_SPECIFIC_WARM_RESET_TYPE: u32 = 0x8000_0000;

const POWER_DOMAIN_COUNT: usize = 4;

const DOMAIN0_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x00);
const DOMAIN1_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x08);
const DOMAIN2_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x10);
const DOMAIN3_REG_OFFSET: Offset<u32, RwSafe> = Offset::new(0x18);
const OPP_BANK_SIZE: usize = 0x20;

static OPP_REG_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Copy, Clone, Debug)]
struct DomainInfo {
    opp_count: u32,
    mmio_offset: u32,
    boot_opp: u64,
    reg_offset: Offset<u32, RwSafe>,
}

const DOMAIN_INFOS: [DomainInfo; 4] = [
    // Domain 0 (Little): 22 OPPs (0..21), mmio_offset = 2, boot_opp = 8
    DomainInfo { opp_count: 22, mmio_offset: 2, boot_opp: 8, reg_offset: DOMAIN0_REG_OFFSET },
    // Domain 1 (Medium 1): 24 OPPs (0..23), mmio_offset = 0, boot_opp = 11
    DomainInfo { opp_count: 24, mmio_offset: 0, boot_opp: 11, reg_offset: DOMAIN1_REG_OFFSET },
    // Domain 2 (Medium 2): 24 OPPs (0..23), mmio_offset = 0, boot_opp = 11
    DomainInfo { opp_count: 24, mmio_offset: 0, boot_opp: 11, reg_offset: DOMAIN2_REG_OFFSET },
    // Domain 3 (Big): 23 OPPs (0..22), mmio_offset = 1, boot_opp = 10
    DomainInfo { opp_count: 23, mmio_offset: 1, boot_opp: 10, reg_offset: DOMAIN3_REG_OFFSET },
];

unsafe extern "C" {
    fn cpp_iris_get_opp_vaddr() -> usize;
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

/// Helper function to construct a `regio::MmioBank` for the OPP register block.
fn get_opp_bank() -> Option<MmioBank<u32, RwSafe>> {
    let base = OPP_REG_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: `OPP_REG_BASE` is mapped into kernel address space during early boot
    // and remains mapped for the kernel's lifetime.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    Some(MmioBank::new(ptr, OPP_BANK_SIZE))
}

/// Sets the active Operating Performance Point (OPP) for the specified power domain.
extern "C" fn iris_opp_set(domain_id: u32, opp: u64) -> i32 {
    let Some(bank) = get_opp_bank() else {
        return Status::BAD_STATE.into_raw();
    };
    let Ok(domain_index) = usize::try_from(domain_id) else {
        return Status::INVALID_ARGS.into_raw();
    };
    let Some(info) = DOMAIN_INFOS.get(domain_index) else {
        return Status::INVALID_ARGS.into_raw();
    };
    if opp >= info.opp_count as u64 {
        return Status::INVALID_ARGS.into_raw();
    }

    let mmio_opp = (opp as u32) + info.mmio_offset;
    // SAFETY: `info.reg_offset` is within `OPP_BANK_SIZE` (0x20) and aligned to 4 bytes.
    let reg = unsafe { bank.at(info.reg_offset) };
    reg.write(mmio_opp);
    Status::OK.into_raw()
}

/// Retrieves the active Operating Performance Point (OPP) for the specified power domain.
extern "C" fn iris_opp_get(domain_id: u32, out_opp: *mut u64) -> i32 {
    if out_opp.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    let Some(bank) = get_opp_bank() else {
        return Status::BAD_STATE.into_raw();
    };
    let Ok(domain_index) = usize::try_from(domain_id) else {
        return Status::INVALID_ARGS.into_raw();
    };
    let Some(info) = DOMAIN_INFOS.get(domain_index) else {
        return Status::INVALID_ARGS.into_raw();
    };

    // SAFETY: `info.reg_offset` is within `OPP_BANK_SIZE` (0x20) and aligned to 4 bytes.
    let reg = unsafe { bank.at(info.reg_offset) };
    let raw_val = reg.read();
    let opp = raw_val.saturating_sub(info.mmio_offset) as u64;
    // SAFETY: `out_opp` was checked non-null above.
    unsafe {
        *out_opp = opp;
    }
    Status::OK.into_raw()
}

/// Retrieves the number of supported OPP control domains.
extern "C" fn iris_opp_get_domain_count(out_count: *mut usize) -> i32 {
    if out_count.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `out_count` was checked non-null above.
    unsafe {
        *out_count = POWER_DOMAIN_COUNT;
    }
    Status::OK.into_raw()
}

static IRIS_POWER_OPS: PdevPowerOps = PdevPowerOps {
    reboot: Some(iris_reboot),
    shutdown: Some(iris_shutdown),
    cpu_off: Some(iris_cpu_off),
    cpu_on: Some(iris_cpu_on),
    get_cpu_state: Some(iris_get_cpu_state),
    opp_set: Some(iris_opp_set),
    opp_get: Some(iris_opp_get),
    opp_get_domain_count: Some(iris_opp_get_domain_count),
};

/// Early initialization hook for Iris power management and OPP register bank.
#[unsafe(no_mangle)]
pub extern "C" fn iris_power_init_early() {
    dprintf!(INFO, "POWER: registering iris power hooks\n");
    // SAFETY: Retrieves the mapped virtual address for the OPP peripheral block.
    let vaddr = unsafe { cpp_iris_get_opp_vaddr() };
    OPP_REG_BASE.store(core::ptr::with_exposed_provenance_mut::<u32>(vaddr), Ordering::Release);

    for (domain_id, info) in DOMAIN_INFOS.iter().enumerate() {
        let _ = iris_opp_set(domain_id as u32, info.boot_opp);
    }

    // SAFETY: IRIS_POWER_OPS has static lifetime and remains valid for the lifetime of the kernel.
    unsafe {
        rust_pdev_register_power(&IRIS_POWER_OPS);
    }
}

/// In-kernel unit tests for the Iris power driver.
#[cfg(ktest)]
#[unittest::suite(name = "iris_power")]
mod tests {
    use super::{OPP_REG_BASE, Ordering};
    use unittest::assert_eq;

    /// Tests that passing a null output pointer to get_cpu_state returns INVALID_ARGS.
    #[test]
    fn test_iris_get_cpu_state_null_arg() {
        assert_eq!(
            super::iris_get_cpu_state(0, core::ptr::null_mut()),
            Status::INVALID_ARGS.into_raw()
        );
    }

    /// Tests opp_get_domain_count.
    #[test]
    fn test_iris_opp_get_domain_count() {
        assert_eq!(
            super::iris_opp_get_domain_count(core::ptr::null_mut()),
            Status::INVALID_ARGS.into_raw()
        );

        let mut count = 0usize;
        assert_eq!(super::iris_opp_get_domain_count(&mut count), Status::OK.into_raw());
        assert_eq!(count, 4);
    }

    /// Tests opp_get and opp_set with mock backing memory.
    #[test]
    fn test_iris_opp_get_set() {
        let mut mock_reg_bank = [0u32; 8];
        let old_base = OPP_REG_BASE.swap(mock_reg_bank.as_mut_ptr(), Ordering::SeqCst);

        // Test Domain 0 (mmio_offset = 2)
        assert_eq!(super::iris_opp_set(0, 5), Status::OK.into_raw());
        assert_eq!(mock_reg_bank[0], 7); // 5 + 2

        let mut opp = 0u64;
        assert_eq!(super::iris_opp_get(0, &mut opp), Status::OK.into_raw());
        assert_eq!(opp, 5);

        // Test Domain 3 (mmio_offset = 1, reg offset 0x18 -> index 6)
        assert_eq!(super::iris_opp_set(3, 10), Status::OK.into_raw());
        assert_eq!(mock_reg_bank[6], 11); // 10 + 1

        assert_eq!(super::iris_opp_get(3, &mut opp), Status::OK.into_raw());
        assert_eq!(opp, 10);

        // Test Out of bounds domain
        assert_eq!(super::iris_opp_set(4, 0), Status::INVALID_ARGS.into_raw());
        assert_eq!(super::iris_opp_get(4, &mut opp), Status::INVALID_ARGS.into_raw());

        // Test Out of bounds opp
        assert_eq!(super::iris_opp_set(0, 22), Status::INVALID_ARGS.into_raw());

        // Restore original base pointer
        OPP_REG_BASE.store(old_base, Ordering::SeqCst);
    }
}
