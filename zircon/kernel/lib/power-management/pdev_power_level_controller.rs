// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Platform device power level controller.

use fbl::{Recyclable, RefPtr, make_ref_counted, ref_counted};
use ksync::KCell;
use zx_status::Status;
use zx_types::ZX_KOID_INVALID;

use super::power_level_controller::{
    ControlInterface, PowerLevelController, PowerLevelControllerBase, PowerLevelUpdateRequest,
};
use crate::kernel::scheduler;
use crate::pdev_power::{power_opp_get, power_opp_get_domain_count, power_opp_set};

ksync::declare_singleton_mutex!(InstanceLock);

static INSTANCE: KCell<Option<RefPtr<PDevPowerLevelController>>, InstanceLock> = KCell::new(None);

/// Power level controller backed by the platform device (pdev) power driver.
#[ref_counted]
#[derive(Debug, Recyclable)]
#[repr(C)]
pub struct PDevPowerLevelController {
    base: PowerLevelControllerBase,
    domain_count: usize,
}

impl PDevPowerLevelController {
    /// Resets the singleton instance for tests.
    pub fn reset_for_test() {
        ksync::lock!(let mut guard = InstanceLock::Get().lock());
        // SAFETY: `guard.as_mut().token_mut()` proves `InstanceLock` is exclusively held.
        unsafe {
            *INSTANCE.get_mut(guard.as_mut().token_mut()) = None;
        }
    }

    /// Returns the singleton `PDevPowerLevelController` after validating `domain_id`.
    pub fn get(domain_id: u32) -> Result<RefPtr<Self>, Status> {
        ksync::lock!(let mut guard = InstanceLock::Get().lock());
        // SAFETY: `guard.as_mut().token_mut()` proves `InstanceLock` is exclusively held.
        let instance_slot = unsafe { INSTANCE.get_mut(guard.as_mut().token_mut()) };
        if let Some(instance) = instance_slot.as_ref() {
            if (domain_id as usize) >= instance.domain_count() {
                return Err(Status::OUT_OF_RANGE);
            }
            return Ok(RefPtr::clone(instance));
        }

        let domain_count = power_opp_get_domain_count()?;
        if (domain_id as usize) >= domain_count {
            return Err(Status::OUT_OF_RANGE);
        }

        let instance = make_ref_counted!(PDevPowerLevelController {
            base: PowerLevelControllerBase::new(ControlInterface::CpuDriver),
            domain_count: domain_count,
        })
        .map_err(|_| Status::NO_MEMORY)?;
        *instance_slot = Some(RefPtr::clone(&instance));
        Ok(instance)
    }

    /// Returns whether the pdev power level controller is supported on this platform.
    pub fn is_supported() -> bool {
        let result = power_opp_get_domain_count();
        result.is_ok_and(|count| count > 0)
    }

    /// Returns the number of domains supported by this controller.
    #[inline]
    pub const fn domain_count(&self) -> usize {
        self.domain_count
    }
}

impl PowerLevelController for PDevPowerLevelController {
    #[inline]
    fn base(&self) -> &PowerLevelControllerBase {
        &self.base
    }

    fn post(&self, pending: &PowerLevelUpdateRequest) -> Result<u32, Status> {
        // Validate the inputs as defense-in-depth in case the pdev driver does not
        // validate them.
        if pending.control != ControlInterface::CpuDriver {
            return Err(Status::NOT_SUPPORTED);
        }
        if (pending.domain_id as usize) >= self.domain_count() {
            return Err(Status::OUT_OF_RANGE);
        }

        power_opp_set(pending.domain_id, pending.control_argument)?;

        scheduler::update_power_level(
            pending.domain_id,
            self.id(),
            self.control_interface(),
            pending.control_argument,
        )
    }

    fn get_current_power_level(&self, domain_id: u32) -> Result<u64, Status> {
        // Validate the input as defense-in-depth in case the pdev driver does not
        // validate it.
        if (domain_id as usize) >= self.domain_count() {
            return Err(Status::OUT_OF_RANGE);
        }
        power_opp_get(domain_id)
    }

    /// Returns a koid that will never collide with a valid dispatcher koid (i.e.
    /// the `PortDispatcher` registered when setting up the domain).
    ///
    /// IMPORTANT: This prevents userspace from updating the active power level
    /// bookkeeping when this power level controller is being used, since this id
    /// is compared when handling `zx_system_set_processor_power_state`. It should be
    /// impossible for userspace to supply a port object with a koid that matches
    /// this id.
    #[inline]
    fn id(&self) -> u64 {
        ZX_KOID_INVALID
    }
}

/// Unit tests for `PDevPowerLevelController`.
#[cfg(ktest)]
#[unittest::suite(name = "pm_controller_rust")]
mod tests {
    use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

    use unittest::{assert_eq, assert_true, expect_eq, expect_err, expect_false, expect_true};

    use super::super::pdev_power_level_controller_ffi::{
        cpp_restore_power_domains_for_test, cpp_save_and_clear_power_domains_for_test,
    };
    use super::{
        ControlInterface, PDevPowerLevelController, PowerLevelController, PowerLevelUpdateRequest,
        Status,
    };
    use crate::pdev_power::{PdevPowerOps, pdev_swap_power_for_test};

    static MOCK_OPP_SET_STATUS: AtomicI32 = AtomicI32::new(0);
    static MOCK_OPP_GET_STATUS: AtomicI32 = AtomicI32::new(0);
    static MOCK_OPP_GET_VALUE: AtomicU64 = AtomicU64::new(0);
    static MOCK_OPP_GET_DOMAIN: AtomicU32 = AtomicU32::new(u32::MAX);
    static MOCK_OPP_GET_DOMAIN_COUNT_STATUS: AtomicI32 = AtomicI32::new(0);
    static MOCK_OPP_GET_DOMAIN_COUNT_VALUE: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn mock_opp_set(_domain_id: u32, _opp: u64) -> Result<(), Status> {
        Status::ok(MOCK_OPP_SET_STATUS.load(Ordering::Relaxed))
    }

    extern "C" fn mock_opp_get(domain_id: u32, out_opp: *mut u64) -> Result<(), Status> {
        let res = Status::ok(MOCK_OPP_GET_STATUS.load(Ordering::Relaxed));
        if res.is_ok() {
            // SAFETY: `out_opp` is a valid non-null pointer provided by `power_opp_get`.
            unsafe {
                *out_opp = MOCK_OPP_GET_VALUE.load(Ordering::Relaxed);
            }
            MOCK_OPP_GET_DOMAIN.store(domain_id, Ordering::Relaxed);
        }
        res
    }

    extern "C" fn mock_opp_get_domain_count(out_count: *mut usize) -> Result<(), Status> {
        let res = Status::ok(MOCK_OPP_GET_DOMAIN_COUNT_STATUS.load(Ordering::Relaxed));
        if res.is_ok() {
            // SAFETY: `out_count` is a valid non-null pointer provided by
            // `power_opp_get_domain_count`.
            unsafe {
                *out_count = MOCK_OPP_GET_DOMAIN_COUNT_VALUE.load(Ordering::Relaxed);
            }
        }
        res
    }

    static MOCK_POWER_OPS: PdevPowerOps = PdevPowerOps {
        reboot: None,
        shutdown: None,
        cpu_off: None,
        cpu_on: None,
        get_cpu_state: None,
        opp_set: Some(mock_opp_set),
        opp_get: Some(mock_opp_get),
        opp_get_domain_count: Some(mock_opp_get_domain_count),
        system_resume: None,
    };

    /// RAII guard that installs `MOCK_POWER_OPS` for the duration of a test.
    struct AutoMockPowerOps {
        original_ops: *const PdevPowerOps,
    }

    impl AutoMockPowerOps {
        fn new() -> Self {
            MOCK_OPP_SET_STATUS.store(0, Ordering::Relaxed);
            MOCK_OPP_GET_STATUS.store(0, Ordering::Relaxed);
            MOCK_OPP_GET_VALUE.store(0, Ordering::Relaxed);
            MOCK_OPP_GET_DOMAIN.store(u32::MAX, Ordering::Relaxed);
            MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
            MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(0, Ordering::Relaxed);
            // SAFETY: `MOCK_POWER_OPS` is a valid `'static` ops table.
            let original_ops = unsafe { pdev_swap_power_for_test(&MOCK_POWER_OPS) };
            Self { original_ops }
        }
    }

    impl Drop for AutoMockPowerOps {
        fn drop(&mut self) {
            // SAFETY: `self.original_ops` is the previously registered ops table.
            unsafe {
                pdev_swap_power_for_test(self.original_ops);
            }
            PDevPowerLevelController::reset_for_test();
        }
    }

    /// Verifies `PDevPowerLevelController::is_supported`.
    #[test]
    fn pdev_power_level_controller_is_supported() {
        let _mock = AutoMockPowerOps::new();

        // The controller is not supported if retrieving the domain count fails.
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(Status::NOT_SUPPORTED.into_raw(), Ordering::Relaxed);
        expect_false!(PDevPowerLevelController::is_supported());

        // The controller is not supported if the domain count is 0.
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(0, Ordering::Relaxed);
        expect_false!(PDevPowerLevelController::is_supported());

        // The controller is supported if the domain count is greater than 0.
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(2, Ordering::Relaxed);
        expect_true!(PDevPowerLevelController::is_supported());
    }

    /// RAII guard that saves and clears per-CPU `PowerDomainSet`s for the duration of a test.
    struct RestorePowerDomains;

    impl RestorePowerDomains {
        fn new() -> Self {
            // SAFETY: Single-threaded test setup saving and clearing per-CPU `PowerDomainSet`s.
            unsafe {
                cpp_save_and_clear_power_domains_for_test();
            }
            Self
        }
    }

    impl Drop for RestorePowerDomains {
        fn drop(&mut self) {
            // SAFETY: Restores the per-CPU `PowerDomainSet`s saved by `RestorePowerDomains::new`.
            unsafe {
                cpp_restore_power_domains_for_test();
            }
        }
    }

    /// Verifies `PDevPowerLevelController::get` and `post` validation.
    #[test]
    fn pdev_power_level_controller_post_validation() {
        // Save and clear the power domain set for all CPUs to prevent scheduler interference
        // and ensure consistent test behavior across architectures (e.g. even if the test
        // thread migrates to another CPU).
        let _restore_state = RestorePowerDomains::new();

        PDevPowerLevelController::reset_for_test();
        let _mock = AutoMockPowerOps::new();
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(2, Ordering::Relaxed);

        let controller_result = PDevPowerLevelController::get(0);
        assert_true!(controller_result.is_ok());
        let controller = controller_result.unwrap();

        // Validate that interface requests not matching CpuDriver are rejected.
        let request_wfi = PowerLevelUpdateRequest {
            domain_id: 0,
            target_id: 0,
            control: ControlInterface::ArmWfi,
            control_argument: 1,
            options: 0,
        };
        expect_err!(controller.post(&request_wfi), Status::NOT_SUPPORTED);

        // Validate that domain_id is checked against the total domain count
        // (domain_id 2 >= count 2).
        let request_oob = PowerLevelUpdateRequest {
            domain_id: 2,
            target_id: 2,
            control: ControlInterface::CpuDriver,
            control_argument: 1,
            options: 0,
        };
        expect_err!(controller.post(&request_oob), Status::OUT_OF_RANGE);

        // Validate that errors from power_opp_get_domain_count are correctly
        // propagated on creation.
        PDevPowerLevelController::reset_for_test();
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(Status::BAD_STATE.into_raw(), Ordering::Relaxed);
        expect_err!(PDevPowerLevelController::get(0), Status::BAD_STATE);

        // Validate that domain_id is checked against domain count during get().
        PDevPowerLevelController::reset_for_test();
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(2, Ordering::Relaxed);
        expect_err!(PDevPowerLevelController::get(2), Status::OUT_OF_RANGE);

        // Re-acquire the controller for the valid post test.
        PDevPowerLevelController::reset_for_test();
        let controller_result = PDevPowerLevelController::get(0);
        assert_true!(controller_result.is_ok());
        let controller = controller_result.unwrap();

        // Validate that a request passing all checks is allowed to proceed to the
        // registry update. Since domain 0 is not registered in this test registry,
        // update_power_level returns Status::NOT_FOUND, which verifies that all
        // earlier validation checks successfully passed.
        let request_valid = PowerLevelUpdateRequest {
            domain_id: 0,
            target_id: 0,
            control: ControlInterface::CpuDriver,
            control_argument: 1,
            options: 0,
        };
        MOCK_OPP_SET_STATUS.store(0, Ordering::Relaxed);
        expect_err!(controller.post(&request_valid), Status::NOT_FOUND);
    }

    /// Verifies `PDevPowerLevelController::get_current_power_level` validation.
    #[test]
    fn pdev_power_level_controller_get_power_level_validation() {
        PDevPowerLevelController::reset_for_test();
        let _mock = AutoMockPowerOps::new();
        MOCK_OPP_GET_DOMAIN_COUNT_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_DOMAIN_COUNT_VALUE.store(2, Ordering::Relaxed);

        let controller_result = PDevPowerLevelController::get(0);
        assert_true!(controller_result.is_ok());
        let controller = controller_result.unwrap();

        // Validate that domain_id is checked against the total domain count
        // (domain_id 2 >= count 2).
        expect_err!(controller.get_current_power_level(2), Status::OUT_OF_RANGE);

        // Validate that a valid domain query returns the value fetched from the pdev
        // backend.
        MOCK_OPP_GET_STATUS.store(0, Ordering::Relaxed);
        MOCK_OPP_GET_VALUE.store(42, Ordering::Relaxed);
        let current_power_level_result = controller.get_current_power_level(0);
        assert_true!(current_power_level_result.is_ok());
        assert_eq!(current_power_level_result.unwrap(), 42);
        expect_eq!(MOCK_OPP_GET_DOMAIN.load(Ordering::Relaxed), 0);
    }
}
