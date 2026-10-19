// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Base trait and shared state for CPU power domain power level controllers.

use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

use zx_status::Status;

/// Supported power level control interfaces.
#[repr(u64)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ControlInterface {
    /// In-kernel CPU driver control interface.
    CpuDriver = 0,
    /// ARM Power State Coordination Interface (PSCI).
    ArmPsci = 1,
    /// ARM Wait For Interrupt (WFI).
    ArmWfi = 2,
    /// RISC-V Supervisor Binary Interface (SBI).
    RiscvSbi = 3,
    /// RISC-V Wait For Interrupt (WFI).
    RiscvWfi = 4,
}

/// Returns the string representation of `control_interface`.
#[inline]
pub const fn to_string(control_interface: ControlInterface) -> &'static str {
    match control_interface {
        ControlInterface::CpuDriver => "CPU_DRIVER",
        ControlInterface::ArmPsci => "ARM_PSCI",
        ControlInterface::ArmWfi => "ARM_WFI",
        ControlInterface::RiscvSbi => "RISCV_SBI",
        ControlInterface::RiscvWfi => "RISCV_WFI",
    }
}

/// Represents a requested power level update to be executed at a later stage.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PowerLevelUpdateRequest {
    /// Domain ID where the request is originating from.
    pub domain_id: u32,

    /// Target ID of the device or domain that should be transitioned.
    pub target_id: u32,

    /// Interface this request should be routed to.
    pub control: ControlInterface,

    /// Argument used for transitioning through the control interface.
    pub control_argument: u64,

    /// Options that determine what `target_id` is. This is determined by the
    /// power level being transitioned to.
    pub options: u32,
}

zr::static_assert!(size_of::<ControlInterface>() == 8);
zr::static_assert!(align_of::<ControlInterface>() == 8);
zr::static_assert!(size_of::<PowerLevelUpdateRequest>() == 32);
zr::static_assert!(align_of::<PowerLevelUpdateRequest>() == 8);
zr::static_assert!(offset_of!(PowerLevelUpdateRequest, domain_id) == 0);
zr::static_assert!(offset_of!(PowerLevelUpdateRequest, target_id) == 4);
zr::static_assert!(offset_of!(PowerLevelUpdateRequest, control) == 8);
zr::static_assert!(offset_of!(PowerLevelUpdateRequest, control_argument) == 16);
zr::static_assert!(offset_of!(PowerLevelUpdateRequest, options) == 24);

/// Base state shared by all [`PowerLevelController`] implementations, corresponding to C++
/// `power_management::PowerLevelController`'s data members.
#[repr(C)]
#[derive(Debug)]
pub struct PowerLevelControllerBase {
    // Implementations will determine when to switch this flag. The flag must always
    // be initialized as true.
    pub(crate) serving: AtomicBool,
    control_interface: ControlInterface,
}

impl PowerLevelControllerBase {
    /// Creates a new `PowerLevelControllerBase` for `control_interface` with `serving` initialized
    /// to `true`.
    #[inline]
    pub const fn new(control_interface: ControlInterface) -> Self {
        Self { serving: AtomicBool::new(true), control_interface }
    }

    /// Determines whether the controller is serving requests or not.
    ///
    /// Allows scheduler instances to interrogate whether they should consider any active power
    /// levels or not.
    #[inline]
    pub fn is_serving(&self) -> bool {
        self.serving.load(Ordering::Relaxed)
    }

    /// Returns the control interface this power level controller implements.
    #[inline]
    pub const fn control_interface(&self) -> ControlInterface {
        self.control_interface
    }
}

/// Trait for power level controllers that can control the active power levels of CPU power
/// domains.
pub trait PowerLevelController {
    /// Returns a reference to the embedded [`PowerLevelControllerBase`] member.
    fn base(&self) -> &PowerLevelControllerBase;

    /// Posts a pending request making it available for the entity in charge of executing the
    /// transition. This method must be complemented by an acknowledgment of a transition.
    fn post(&self, pending: &PowerLevelUpdateRequest) -> Result<u32, Status>;

    /// Returns the current device-specific power level (i.e. the value passed as a control
    /// argument) for the given `domain_id`, if supported by the controller.
    ///
    /// May be implemented by in-kernel CPU drivers that are capable of querying the current
    /// hardware state.
    #[inline]
    fn get_current_power_level(&self, _domain_id: u32) -> Result<u64, Status> {
        Err(Status::NOT_SUPPORTED)
    }

    /// Unique id of the `PowerLevelController`, used for validation.
    fn id(&self) -> u64;

    /// Determines whether the controller is serving requests or not.
    ///
    /// Allows scheduler instances to interrogate whether they should consider any active power
    /// levels or not.
    #[inline]
    fn is_serving(&self) -> bool {
        self.base().is_serving()
    }

    /// Returns the control interface this power level controller implements.
    #[inline]
    fn control_interface(&self) -> ControlInterface {
        self.base().control_interface()
    }
}
