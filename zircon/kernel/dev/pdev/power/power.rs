// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/power/power.cc
//
// Platform Device (pdev) Power Driver Interface.
//
// Provides platform-level abstraction and dispatch for system power operations,
// including system reboot, shutdown, CPU core power state management, and
// dynamic Operating Performance Point (OPP) control for CPU power domains.

#[cfg(console_enabled)]
pub mod console;
#[cfg(not(console_enabled))]
use debug as _;
mod power_ffi;

use core::sync::atomic::Ordering;
#[cfg(ktest)]
use unittest as _;
use zr::AtomicConstPtr;
use zx_status::Status;

/// Flags passed to `power_reboot` specifying the reason/target for reboot.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerRebootFlags {
    Normal = 0,
    Bootloader = 1,
    Recovery = 2,
    Panic = 3,
}

/// Represents the architectural power state of a CPU.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerCpuState {
    On = 0,
    Off = 1,
    OnPending = 2,
    Started = 3,
    Stopped = 4,
    StartPending = 5,
    StopPending = 6,
    Suspended = 7,
    SuspendPending = 8,
    ResumePending = 9,
}

/// Platform device power operations table.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct PdevPowerOps {
    /// Reboots the system according to the provided reboot flags.
    pub reboot: Option<extern "C" fn(flags: PowerRebootFlags) -> Result<(), Status>>,
    /// Powers off / shuts down the system.
    pub shutdown: Option<extern "C" fn() -> Result<(), Status>>,
    /// Powers off the current CPU.
    pub cpu_off: Option<extern "C" fn() -> Result<(), Status>>,
    /// Initiates power-on sequence for the specified hardware CPU.
    pub cpu_on:
        Option<extern "C" fn(hw_cpu_id: u64, entry: u64, context: u64) -> Result<(), Status>>,
    /// Retrieves the current power state of the specified CPU.
    pub get_cpu_state:
        Option<extern "C" fn(hw_cpu_id: u64, out_state: *mut PowerCpuState) -> Result<(), Status>>,
    /// Sets the Operating Performance Point (OPP) for the specified power domain.
    pub opp_set: Option<extern "C" fn(domain_id: u32, opp: u64) -> Result<(), Status>>,
    /// Retrieves the active Operating Performance Point (OPP) for the specified power domain.
    pub opp_get: Option<extern "C" fn(domain_id: u32, out_opp: *mut u64) -> Result<(), Status>>,
    /// Returns the number of supported OPP control domains.
    pub opp_get_domain_count: Option<extern "C" fn(out_count: *mut usize) -> Result<(), Status>>,
    /// Restores platform power state that firmware may have altered while the system was
    /// suspended. Called on the boot CPU, with interrupts disabled, immediately after returning
    /// from system suspend.
    pub system_resume: Option<extern "C" fn() -> Result<(), Status>>,
}

zr::static_assert!(core::mem::size_of::<PowerRebootFlags>() == 4);
zr::static_assert!(core::mem::align_of::<PowerRebootFlags>() == 4);
zr::static_assert!(core::mem::size_of::<PowerCpuState>() == 4);
zr::static_assert!(core::mem::align_of::<PowerCpuState>() == 4);
zr::static_assert!(core::mem::size_of::<PdevPowerOps>() == 72);
zr::static_assert!(core::mem::align_of::<PdevPowerOps>() == 8);

/// Power level options flag: entity power level is independent of other CPUs in the domain.
pub const K_POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT: u32 = 1;

/// Control interface identifier for ARM WFI (Wait-For-Interrupt) transitions.
pub const CONTROL_INTERFACE_ARM_WFI: u32 = 0;

/// Control interface identifier for CPU power driver transitions.
pub const CONTROL_INTERFACE_CPU_DRIVER: u32 = 1;

/// FFI representation of a processor power level for energy model registration.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ProcessorPowerLevelFfi {
    /// Power level options (e.g. `kPowerLevelOptionsDomainIndependent`).
    pub options: u32,
    /// Processing rate associated with this power level.
    pub processing_rate: u64,
    /// Power consumption coefficient in nanowatts.
    pub power_coefficient_nw: u64,
    /// Power control interface (0 for `kArmWfi`, 1 for `kCpuDriver`).
    pub control_interface: u32,
    /// Control argument passed to the power control interface.
    pub control_argument: u64,
    /// Optional diagnostic name string pointer for debugging/inspect.
    pub diagnostic_name: *const core::ffi::c_char,
}

zr::static_assert!(core::mem::size_of::<ProcessorPowerLevelFfi>() == 48);
zr::static_assert!(core::mem::align_of::<ProcessorPowerLevelFfi>() == 8);

/// FFI representation of a power domain configuration for energy model registration.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerDomainConfigFfi {
    /// Domain identifier.
    pub domain_id: u32,
    /// Bitmask of CPU core IDs belonging to this power domain.
    pub cpu_mask: u64,
    /// Pointer to the array of supported power levels.
    pub levels: *const ProcessorPowerLevelFfi,
    /// Number of power levels in the `levels` array.
    pub level_count: usize,
}

zr::static_assert!(core::mem::size_of::<PowerDomainConfigFfi>() == 32);
zr::static_assert!(core::mem::align_of::<PowerDomainConfigFfi>() == 8);

use boot_options::BootOptions;
use power_ffi::{
    cpp_power_management_boot_boost_enabled, cpp_power_management_register_domains,
    cpp_power_management_set_rate_limits,
};

/// Registers the provided power domain configurations and energy models with the kernel scheduler.
pub fn power_management_register_domains(domains: &[PowerDomainConfigFfi]) -> Result<(), Status> {
    let ptr = if domains.is_empty() { core::ptr::null() } else { domains.as_ptr() };
    // SAFETY: `domains` is a valid slice of `PowerDomainConfigFfi`.
    unsafe { cpp_power_management_register_domains(ptr, domains.len()) }
}

/// Sets processing rate limits (min and max) for the CPU cores identified by `cpu_mask`.
///
/// # Arguments
///
/// * `cpu_mask` - Bitmask of logical CPU cores to update.
/// * `min_rate` - Minimum processing rate on the `[0, 1000]` user processing rate scale.
/// * `max_rate` - Maximum processing rate on the `[0, 1000]` user processing rate scale.
pub fn power_management_set_rate_limits(
    cpu_mask: u64,
    min_rate: u64,
    max_rate: u64,
) -> Result<(), Status> {
    // SAFETY: Foreign FFI call to C++ kernel scheduler helper.
    unsafe { cpp_power_management_set_rate_limits(cpu_mask, min_rate, max_rate) }
}

/// Returns whether early boot performance boosting is enabled via kernel boot options.
pub fn power_management_boot_boost_enabled() -> bool {
    // SAFETY: Foreign FFI call to C++ boot options query.
    unsafe { cpp_power_management_boot_boost_enabled() }
}

/// Returns whether runtime processor power management (RPPM) is enabled via kernel boot options.
pub fn power_management_rppm_enabled() -> bool {
    BootOptions::get().power_rppm
}

static DEFAULT_OPS: PdevPowerOps = PdevPowerOps {
    reboot: None,
    shutdown: None,
    cpu_off: None,
    cpu_on: None,
    get_cpu_state: None,
    opp_set: None,
    opp_get: None,
    opp_get_domain_count: None,
    system_resume: None,
};

static POWER_OPS: AtomicConstPtr<PdevPowerOps> =
    AtomicConstPtr::new(core::ptr::addr_of!(DEFAULT_OPS));

fn get_ops() -> &'static PdevPowerOps {
    let ops_ptr = POWER_OPS.load(Ordering::Acquire);
    if ops_ptr.is_null() {
        &DEFAULT_OPS
    } else {
        // SAFETY: `POWER_OPS` always points to either `DEFAULT_OPS` or a registered `PdevPowerOps`
        // table with `'static` lifetime guaranteed by the caller of `pdev_register_power`.
        unsafe { &*ops_ptr }
    }
}

/// Registers the platform power operations table with `'static` lifetime.
pub fn pdev_register_power(ops: &'static PdevPowerOps) {
    POWER_OPS.store(ops as *const PdevPowerOps, Ordering::Release);
}

/// Swaps the current power operations table with a new one for testing, returning the previous
/// table.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevPowerOps` table for the duration of the test.
pub unsafe fn pdev_swap_power_for_test(ops: *const PdevPowerOps) -> *const PdevPowerOps {
    let target_ptr = if ops.is_null() { core::ptr::addr_of!(DEFAULT_OPS) } else { ops };
    POWER_OPS.swap(target_ptr, Ordering::AcqRel)
}

/// Reboots the system using the registered power operations.
pub fn power_reboot(flags: PowerRebootFlags) {
    let ops = get_ops();
    if let Some(reboot_fn) = ops.reboot {
        let _ = reboot_fn(flags);
    }
}

/// Shuts down the system using the registered power operations.
pub fn power_shutdown() {
    let ops = get_ops();
    if let Some(shutdown_fn) = ops.shutdown {
        let _ = shutdown_fn();
    }
}

/// Powers off the calling CPU.
pub fn power_cpu_off() -> Result<(), Status> {
    let ops = get_ops();
    if let Some(cpu_off_fn) = ops.cpu_off { cpu_off_fn() } else { Ok(()) }
}

/// Initiates the power-on sequence for the CPU with the given hardware CPU ID.
///
/// Note that a hardware CPU ID is architecture-specific (e.g. MPID on ARM, hart ID on RISC-V)
/// and not equivalent to a `cpu_num_t`. This function does not block/wait on the CPU actually
/// coming online; callers are expected to use [`power_get_cpu_state`] to poll the state of the
/// CPU until it comes online.
///
/// On some architectures, the `context` argument is passed to the newly started CPU.
pub fn power_cpu_on(hw_cpu_id: u64, entry: u64, context: u64) -> Result<(), Status> {
    let ops = get_ops();
    if let Some(cpu_on_fn) = ops.cpu_on { cpu_on_fn(hw_cpu_id, entry, context) } else { Ok(()) }
}

/// Retrieves the power state of the CPU with the specified hardware ID.
pub fn power_get_cpu_state(hw_cpu_id: u64) -> Result<PowerCpuState, Status> {
    let ops = get_ops();
    if let Some(get_cpu_state_fn) = ops.get_cpu_state {
        let mut state = PowerCpuState::Off;
        get_cpu_state_fn(hw_cpu_id, &mut state)?;
        Ok(state)
    } else {
        Err(Status::NOT_SUPPORTED)
    }
}

/// Sets the opp for the given domain, if supported.
pub fn power_opp_set(domain_id: u32, opp: u64) -> Result<(), Status> {
    let ops = get_ops();
    if let Some(opp_set_fn) = ops.opp_set {
        opp_set_fn(domain_id, opp)
    } else {
        Err(Status::NOT_SUPPORTED)
    }
}

/// Gets the opp for the given domain, if supported.
pub fn power_opp_get(domain_id: u32) -> Result<u64, Status> {
    let ops = get_ops();
    if let Some(opp_get_fn) = ops.opp_get {
        let mut opp = 0;
        opp_get_fn(domain_id, &mut opp)?;
        Ok(opp)
    } else {
        Err(Status::NOT_SUPPORTED)
    }
}

/// Gets the number of supported opp control domains, if supported. If this
/// returns zero or an error it generally means that in-kernel OPP control is not
/// supported.
pub fn power_opp_get_domain_count() -> Result<usize, Status> {
    let ops = get_ops();
    if let Some(opp_get_domain_count_fn) = ops.opp_get_domain_count {
        let mut count = 0;
        opp_get_domain_count_fn(&mut count)?;
        Ok(count)
    } else {
        Err(Status::NOT_SUPPORTED)
    }
}

/// Restores platform power state after returning from system suspend, if supported.
///
/// Firmware may alter platform power state (e.g. CPU OPP registers) while the system is
/// suspended. This gives the platform driver a chance to re-apply the kernel's intended state
/// before the rest of the system resumes. Platforms without such state treat this as a no-op.
pub fn power_system_resume() -> Result<(), Status> {
    let ops = get_ops();
    if let Some(system_resume_fn) = ops.system_resume { system_resume_fn() } else { Ok(()) }
}

/// In-kernel unit tests for the platform power subsystem.
#[cfg(ktest)]
#[unittest::suite(name = "pdev_power")]
mod tests {
    use crate::pdev_power::power_ffi::rust_power_get_cpu_state;
    use crate::pdev_power::{
        PdevPowerOps, PowerCpuState, PowerRebootFlags, pdev_swap_power_for_test, power_cpu_off,
        power_cpu_on, power_get_cpu_state, power_management_register_domains,
        power_management_set_rate_limits, power_opp_get, power_opp_get_domain_count, power_opp_set,
        power_reboot, power_shutdown, power_system_resume,
    };
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use unittest::{assert_eq, assert_err, assert_ok};
    use zx_status::Status;

    static TEST_REBOOT_CALLED: AtomicU32 = AtomicU32::new(0);
    static TEST_SHUTDOWN_CALLED: AtomicU32 = AtomicU32::new(0);
    static TEST_CPU_OFF_CALLED: AtomicU32 = AtomicU32::new(0);
    static TEST_CPU_ON_HW_ID: AtomicU64 = AtomicU64::new(0);
    static TEST_OPP_SET_DOMAIN: AtomicU32 = AtomicU32::new(0);
    static TEST_OPP_SET_VALUE: AtomicU64 = AtomicU64::new(0);
    static TEST_SYSTEM_RESUME_CALLED: AtomicU32 = AtomicU32::new(0);

    extern "C" fn test_reboot(flags: PowerRebootFlags) -> Result<(), Status> {
        TEST_REBOOT_CALLED.store(flags as u32 + 1, Ordering::Relaxed);
        Ok(())
    }

    extern "C" fn test_shutdown() -> Result<(), Status> {
        TEST_SHUTDOWN_CALLED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    extern "C" fn test_cpu_off() -> Result<(), Status> {
        TEST_CPU_OFF_CALLED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    extern "C" fn test_cpu_on(hw_cpu_id: u64, _entry: u64, _context: u64) -> Result<(), Status> {
        TEST_CPU_ON_HW_ID.store(hw_cpu_id, Ordering::Relaxed);
        Ok(())
    }

    extern "C" fn test_get_cpu_state(
        hw_cpu_id: u64,
        out_state: *mut PowerCpuState,
    ) -> Result<(), Status> {
        if !out_state.is_null() {
            // SAFETY: Caller provides a valid pointer for test.
            unsafe {
                *out_state = if hw_cpu_id == 42 { PowerCpuState::On } else { PowerCpuState::Off };
            }
        }
        Ok(())
    }

    extern "C" fn test_opp_set(domain_id: u32, opp: u64) -> Result<(), Status> {
        TEST_OPP_SET_DOMAIN.store(domain_id, Ordering::Relaxed);
        TEST_OPP_SET_VALUE.store(opp, Ordering::Relaxed);
        Ok(())
    }

    extern "C" fn test_opp_get(domain_id: u32, out_opp: *mut u64) -> Result<(), Status> {
        if domain_id == 1 && !out_opp.is_null() {
            // SAFETY: Caller provides a valid pointer for test.
            unsafe {
                *out_opp = 7;
            }
            Ok(())
        } else {
            Err(Status::INVALID_ARGS)
        }
    }

    extern "C" fn test_opp_get_domain_count(out_count: *mut usize) -> Result<(), Status> {
        if !out_count.is_null() {
            // SAFETY: Caller provides a valid pointer for test.
            unsafe {
                *out_count = 2;
            }
            Ok(())
        } else {
            Err(Status::INVALID_ARGS)
        }
    }

    extern "C" fn test_system_resume() -> Result<(), Status> {
        TEST_SYSTEM_RESUME_CALLED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    static TEST_OPS: PdevPowerOps = PdevPowerOps {
        reboot: Some(test_reboot),
        shutdown: Some(test_shutdown),
        cpu_off: Some(test_cpu_off),
        cpu_on: Some(test_cpu_on),
        get_cpu_state: Some(test_get_cpu_state),
        opp_set: Some(test_opp_set),
        opp_get: Some(test_opp_get),
        opp_get_domain_count: Some(test_opp_get_domain_count),
        system_resume: Some(test_system_resume),
    };

    struct TestOpsGuard {
        previous: *const PdevPowerOps,
    }

    impl TestOpsGuard {
        fn new(ops: *const PdevPowerOps) -> Self {
            // SAFETY: Swapping ops for the duration of the test.
            let previous = unsafe { pdev_swap_power_for_test(ops) };
            Self { previous }
        }
    }

    impl Drop for TestOpsGuard {
        fn drop(&mut self) {
            // SAFETY: Restoring the previous ops table when test ends.
            unsafe {
                pdev_swap_power_for_test(self.previous);
            }
        }
    }

    /// Tests that default operations fallback cleanly without registered power ops.
    #[test]
    fn test_default_ops_fallback() {
        let _guard = TestOpsGuard::new(core::ptr::null());

        assert_ok!(power_cpu_off());
        assert_ok!(power_cpu_on(1, 0, 0));
        assert_err!(power_get_cpu_state(1), Status::NOT_SUPPORTED);
        assert_err!(power_opp_set(0, 1), Status::NOT_SUPPORTED);
        assert_err!(power_opp_get(0), Status::NOT_SUPPORTED);
        assert_err!(power_opp_get_domain_count(), Status::NOT_SUPPORTED);
        assert_ok!(power_system_resume());
    }

    /// Tests that registered power ops are dispatched correctly.
    #[test]
    fn test_registered_ops_dispatch() {
        let _guard = TestOpsGuard::new(&TEST_OPS);

        TEST_REBOOT_CALLED.store(0, Ordering::Relaxed);
        power_reboot(PowerRebootFlags::Recovery);
        assert_eq!(
            TEST_REBOOT_CALLED.load(Ordering::Relaxed),
            PowerRebootFlags::Recovery as u32 + 1
        );

        TEST_SHUTDOWN_CALLED.store(0, Ordering::Relaxed);
        power_shutdown();
        assert_eq!(TEST_SHUTDOWN_CALLED.load(Ordering::Relaxed), 1);

        TEST_CPU_OFF_CALLED.store(0, Ordering::Relaxed);
        assert_ok!(power_cpu_off());
        assert_eq!(TEST_CPU_OFF_CALLED.load(Ordering::Relaxed), 1);

        TEST_CPU_ON_HW_ID.store(0, Ordering::Relaxed);
        assert_ok!(power_cpu_on(1234, 0, 0));
        assert_eq!(TEST_CPU_ON_HW_ID.load(Ordering::Relaxed), 1234);

        let state = power_get_cpu_state(42);
        assert_ok!(state.map(|_| ()));
        assert_eq!(state.unwrap(), PowerCpuState::On);

        TEST_OPP_SET_DOMAIN.store(0, Ordering::Relaxed);
        TEST_OPP_SET_VALUE.store(0, Ordering::Relaxed);
        assert_ok!(power_opp_set(3, 5));
        assert_eq!(TEST_OPP_SET_DOMAIN.load(Ordering::Relaxed), 3);
        assert_eq!(TEST_OPP_SET_VALUE.load(Ordering::Relaxed), 5);

        let opp = power_opp_get(1);
        assert_ok!(opp.map(|_| ()));
        assert_eq!(opp.unwrap(), 7);

        let domain_count = power_opp_get_domain_count();
        assert_ok!(domain_count.map(|_| ()));
        assert_eq!(domain_count.unwrap(), 2);

        TEST_SYSTEM_RESUME_CALLED.store(0, Ordering::Relaxed);
        assert_ok!(power_system_resume());
        assert_eq!(TEST_SYSTEM_RESUME_CALLED.load(Ordering::Relaxed), 1);
    }

    /// Tests null pointer handling for FFI power operations.
    #[test]
    fn test_null_pointer_arguments() {
        let _guard = TestOpsGuard::new(&TEST_OPS);

        // SAFETY: Testing null pointer validation behavior.
        assert_err!(
            unsafe { rust_power_get_cpu_state(1, core::ptr::null_mut()) },
            Status::INVALID_ARGS
        );
    }

    /// Tests registering an empty domain slice with power_management_register_domains.
    #[test]
    fn test_power_management_register_domains_empty() {
        let status = power_management_register_domains(&[]);
        assert_ok!(status);
    }

    /// Tests setting rate limits with an empty CPU mask.
    #[test]
    fn test_power_management_set_rate_limits_empty_mask() {
        let status = power_management_set_rate_limits(0, 0, 1000);
        assert_ok!(status);
    }

    /// Tests querying the boot boost enabled configuration.
    #[test]
    fn test_power_management_boot_boost_enabled() {
        let _ = super::power_management_boot_boost_enabled();
    }

    /// Tests querying the RPPM enabled configuration.
    #[test]
    fn test_power_management_rppm_enabled() {
        let _ = super::power_management_rppm_enabled();
    }
}
