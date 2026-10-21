// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel power management subsystem.

#![cfg_attr(feature = "userspace", no_std)]

pub mod energy_model;
mod energy_model_ffi;
#[cfg(not(feature = "userspace"))]
pub mod pdev_power_level_controller;
#[cfg(not(feature = "userspace"))]
mod pdev_power_level_controller_ffi;
pub mod power_level_controller;
pub mod power_state;
mod power_state_ffi;

/// The normalized processing rate of a CPU, relative to the fastest CPU in the system.
pub type ProcessingRate = ffl::FastFixed<i64, 31>;

/// The normalized utilization of a CPU by a task or set of tasks.
pub type Utilization = ffl::FastFixed<i64, 31>;

/// A time point on an indeterminate timeline.
pub type Time = ffl::FastFixed<i64, 0>;

pub use energy_model::{
    CppPowerLevelController, EnergyModel, MAX_POWER_DOMAINS,
    POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PowerDomain, PowerDomainSet, PowerLevel,
};
#[cfg(not(feature = "userspace"))]
pub use pdev_power_level_controller::PDevPowerLevelController;
pub use power_level_controller::{
    ControlInterface, PowerLevelController, PowerLevelControllerBase, PowerLevelUpdateRequest,
};
pub use power_state::PowerState;
