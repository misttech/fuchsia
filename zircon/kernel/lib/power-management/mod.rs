// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel power management subsystem.

pub mod pdev_power_level_controller;
mod pdev_power_level_controller_ffi;
pub mod power_level_controller;

pub use pdev_power_level_controller::PDevPowerLevelController;
pub use power_level_controller::{
    ControlInterface, PowerLevelController, PowerLevelControllerBase, PowerLevelUpdateRequest,
};
