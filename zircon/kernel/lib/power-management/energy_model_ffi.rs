// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI declarations for `EnergyModel`, `PowerDomain`, and `CppPowerLevelController`.

use core::sync::atomic::AtomicI64;

use zx_types::zx_status_t;

use super::ControlInterface;
use super::energy_model::{CppPowerLevelController, EnergyModel, PowerDomain, PowerLevel};

unsafe extern "C" {
    /// Recycles a `PowerDomain` instance when its last `RefPtr` reference is dropped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid, non-null pointer to a `PowerDomain` whose
    /// reference count has just reached zero, and must only be called from `RefPtr` drop logic.
    pub fn cpp_power_domain_recycle(ptr: *mut PowerDomain);

    /// Returns the ID of `domain` (`PowerDomain::id`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_id(domain: &PowerDomain) -> u32;

    /// Returns the CPU mask of `domain` (`PowerDomain::cpus`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_cpus(domain: &PowerDomain) -> u64;

    /// Returns a reference to the `EnergyModel` of `domain` (`PowerDomain::model`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` for the
    /// lifetime of the returned reference.
    pub fn cpp_power_domain_model(domain: &PowerDomain) -> &EnergyModel;

    /// Returns a reference to the `CppPowerLevelController` of `domain`
    /// (`PowerDomain::controller`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` with a
    /// non-null `controller_` for the lifetime of the returned reference.
    pub fn cpp_power_domain_controller(domain: &PowerDomain) -> &CppPowerLevelController;

    /// Returns whether scheduler control is enabled for `domain`
    /// (`PowerDomain::scheduler_control_enabled`).
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain`.
    pub fn cpp_power_domain_scheduler_control_enabled(domain: &PowerDomain) -> bool;

    /// Returns a reference to `domain->total_normalized_utilization_`.
    ///
    /// # Safety
    ///
    /// `domain` must alias a live, initialized C++ `power_management::PowerDomain` for the
    /// lifetime of the returned reference.
    pub fn cpp_power_domain_total_normalized_utilization_ptr(domain: &PowerDomain) -> &AtomicI64;

    /// Returns whether `controller` is serving requests (`PowerLevelController::is_serving`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`.
    pub fn cpp_power_level_controller_is_serving(controller: &CppPowerLevelController) -> bool;

    /// Returns the control interface of `controller`
    /// (`PowerLevelController::control_interface`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`
    /// whose `control_interface_` is a valid [`ControlInterface`] discriminant.
    pub fn cpp_power_level_controller_control_interface(
        controller: &CppPowerLevelController,
    ) -> ControlInterface;

    /// Queries the current power level argument from `controller` for `domain_id`
    /// (`PowerLevelController::GetCurrentPowerLevel`).
    ///
    /// # Safety
    ///
    /// `controller` must alias a live, initialized C++ `power_management::PowerLevelController`.
    pub fn cpp_power_level_controller_get_current_power_level(
        controller: &CppPowerLevelController,
        domain_id: u32,
        out_level: &mut u64,
    ) -> zx_status_t;

    /// Returns a pointer to the `PowerLevel` array of `model` (`EnergyModel::levels().data()`).
    ///
    /// # Safety
    ///
    /// `model` must alias a live, initialized C++ `power_management::EnergyModel`.
    pub fn cpp_energy_model_levels_data(model: &EnergyModel) -> *const PowerLevel;

    /// Returns the total number of power levels in `model` (`EnergyModel::levels().size()`).
    ///
    /// # Safety
    ///
    /// `model` must alias a live, initialized C++ `power_management::EnergyModel`.
    pub fn cpp_energy_model_levels_size(model: &EnergyModel) -> usize;

    /// Returns the number of idle power levels in `model` (`EnergyModel::idle_levels().size()`).
    ///
    /// # Safety
    ///
    /// `model` must alias a live, initialized C++ `power_management::EnergyModel`.
    pub fn cpp_energy_model_idle_levels_size(model: &EnergyModel) -> usize;

    /// Looks up the power level index matching `(interface_id, control_argument)` in `model`
    /// (`EnergyModel::FindPowerLevel`).
    ///
    /// # Safety
    ///
    /// `model` must alias a live, initialized C++ `power_management::EnergyModel`.
    pub fn cpp_energy_model_find_power_level(
        model: &EnergyModel,
        interface_id: ControlInterface,
        control_argument: u64,
        out_level: &mut u8,
    ) -> bool;
}
