// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C FFI bindings for `PDevPowerLevelController`.

use fbl::RefPtr;
use zx_status::Status;

use super::pdev_power_level_controller::PDevPowerLevelController;
use super::power_level_controller::{PowerLevelController, PowerLevelUpdateRequest};

/// Resets the singleton `PDevPowerLevelController` instance for tests.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_reset_for_test() {
    PDevPowerLevelController::reset_for_test();
}

/// Acquires a strong reference to the singleton `PDevPowerLevelController` for `domain_id`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_get(
    domain_id: u32,
    out_controller: &mut *const PDevPowerLevelController,
) -> Result<(), Status> {
    let controller = PDevPowerLevelController::get(domain_id)?;
    *out_controller = RefPtr::into_raw(controller);
    Ok(())
}

/// Releases a strong reference to `PDevPowerLevelController` previously returned by
/// [`rust_pdev_power_level_controller_get`].
///
/// # Safety
///
/// If non-null, `controller` must have been previously returned by
/// [`rust_pdev_power_level_controller_get`], and each non-null call consumes one strong reference
/// count previously returned by [`rust_pdev_power_level_controller_get`] (since `get` returns the
/// same singleton address with an incremented reference count on each call). The caller must not
/// use that reference after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_power_level_controller_release(
    controller: *const PDevPowerLevelController,
) {
    if !controller.is_null() {
        // SAFETY: `controller` was created by `RefPtr::into_raw` in
        // `rust_pdev_power_level_controller_get` and ownership of one strong reference is
        // transferred back here.
        unsafe {
            drop(RefPtr::from_raw(controller));
        }
    }
}

/// Posts a pending power level update request to `controller`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_post(
    controller: &PDevPowerLevelController,
    pending: &PowerLevelUpdateRequest,
    out_cpu_mask: &mut u32,
) -> Result<(), Status> {
    *out_cpu_mask = controller.post(pending)?;
    Ok(())
}

/// Queries the current power level for `domain_id` from `controller`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_get_current_power_level(
    controller: &PDevPowerLevelController,
    domain_id: u32,
    out_level: &mut u64,
) -> Result<(), Status> {
    *out_level = controller.get_current_power_level(domain_id)?;
    Ok(())
}

/// Returns the number of power domains supported by `controller`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_domain_count(
    controller: &PDevPowerLevelController,
) -> usize {
    controller.domain_count()
}

/// Returns whether `PDevPowerLevelController` is supported on the current platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pdev_power_level_controller_is_supported() -> bool {
    PDevPowerLevelController::is_supported()
}

#[cfg(ktest)]
unsafe extern "C" {
    /// Saves and clears the per-CPU `PowerDomainSet`s across all CPUs for unit testing.
    ///
    /// # Safety
    ///
    /// The caller must ensure no concurrent test is accessing `g_saved_power_domains` and must pair
    /// this call with [`cpp_restore_power_domains_for_test`].
    pub(super) fn cpp_save_and_clear_power_domains_for_test();

    /// Restores the per-CPU `PowerDomainSet`s previously saved by
    /// [`cpp_save_and_clear_power_domains_for_test`].
    ///
    /// # Safety
    ///
    /// Must only be called after a matching call to [`cpp_save_and_clear_power_domains_for_test`]
    /// with no concurrent access to `g_saved_power_domains`.
    pub(super) fn cpp_restore_power_domains_for_test();
}
