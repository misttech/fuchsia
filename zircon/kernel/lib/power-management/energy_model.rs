// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Safe Rust facades for `PowerLevel`, `EnergyModel`, `PowerLevelController`, `PowerDomain`, and
//! `PowerDomainSet`.

use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicI64, Ordering};

use fbl::RefPtr;
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

use super::energy_model_ffi::{
    cpp_energy_model_find_power_level, cpp_energy_model_idle_levels_size,
    cpp_energy_model_levels_data, cpp_energy_model_levels_size, cpp_power_domain_controller,
    cpp_power_domain_cpus, cpp_power_domain_id, cpp_power_domain_model, cpp_power_domain_recycle,
    cpp_power_domain_scheduler_control_enabled, cpp_power_domain_total_normalized_utilization_ptr,
    cpp_power_level_controller_control_interface,
    cpp_power_level_controller_get_current_power_level, cpp_power_level_controller_is_serving,
};
use super::{ControlInterface, ProcessingRate, Utilization};

/// Option flag indicating that a power level transition targets individual CPUs independently of
/// the power domain (`kPowerLevelOptionsDomainIndependent`).
pub const POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT: u64 = 1 << 0;

/// Maximum number of power domains in a `PowerDomainSet` (`PowerDomainSet::kMaxPowerDomains`).
pub const MAX_POWER_DOMAINS: usize = 4;

/// Kernel representation of `zx_processor_power_level_t` with useful accessors and option support.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PowerLevel {
    /// Options (`PowerLevel::options_`).
    options: u64,
    /// Control interface used to transition to this level (`PowerLevel::control_`).
    control: ControlInterface,
    /// Argument to be provided to the control interface (`PowerLevel::control_argument_`).
    control_argument: u64,
    /// Processing rate (`PowerLevel::processing_rate_`).
    processing_rate: ProcessingRate,
    /// Power coefficient in nanowatts (`PowerLevel::power_coefficient_nw_`).
    power_coefficient_nw: u64,
    /// Power cost. Memoized value of `power_coefficient_nw_ / processing_rate_`
    /// (`PowerLevel::power_cost_nw_per_rate_`).
    power_cost_nw_per_rate: u64,
    /// Diagnostic name (`PowerLevel::name_`).
    name: [u8; ZX_MAX_NAME_LEN],
    /// Length of the diagnostic name (`PowerLevel::name_len_`).
    name_len: usize,
    /// Level as described in the model shared with user (`PowerLevel::level_`).
    level: u8,
}

zr::static_assert!(size_of::<PowerLevel>() == 96);
zr::static_assert!(align_of::<PowerLevel>() == 8);
zr::static_assert!(offset_of!(PowerLevel, options) == 0);
zr::static_assert!(offset_of!(PowerLevel, control) == 8);
zr::static_assert!(offset_of!(PowerLevel, control_argument) == 16);
zr::static_assert!(offset_of!(PowerLevel, processing_rate) == 24);
zr::static_assert!(offset_of!(PowerLevel, power_coefficient_nw) == 32);
zr::static_assert!(offset_of!(PowerLevel, power_cost_nw_per_rate) == 40);
zr::static_assert!(offset_of!(PowerLevel, name) == 48);
zr::static_assert!(offset_of!(PowerLevel, name_len) == 80);
zr::static_assert!(offset_of!(PowerLevel, level) == 88);

impl PowerLevel {
    /// Processing rate when this power level is active. This is key to determining the available
    /// bandwidth of the entity.
    #[inline]
    pub const fn processing_rate(&self) -> ProcessingRate {
        self.processing_rate
    }

    /// Relative to the system power consumption, determines how much power is being consumed at
    /// this level. This allows determining if this power level should be a candidate when
    /// operating under a given energy budget.
    #[inline]
    pub const fn power_coefficient_nw(&self) -> u64 {
        self.power_coefficient_nw
    }

    /// Power cost of this power level, normalized by the rate of this power level.
    #[inline]
    pub const fn power_cost_nw_per_rate(&self) -> u64 {
        self.power_cost_nw_per_rate
    }

    /// ID of the interface handling transitions TO this power level.
    #[inline]
    pub const fn control(&self) -> ControlInterface {
        self.control
    }

    /// Argument to be interpreted by the control interface in order to transition to this level.
    ///
    /// The control interface is only aware of this argument, and power levels are identified by
    /// this argument.
    #[inline]
    pub const fn control_argument(&self) -> u64 {
        self.control_argument
    }

    /// This level may be transitioned on a per-CPU basis, without affecting other entities in the
    /// same power domain (`TargetsCpus`).
    #[inline]
    pub const fn targets_cpus(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) != 0
    }

    /// This level may be transitioned on a per-power-domain basis, that is, all other entities in
    /// the power domain will be transitioned together (`TargetsPowerDomain`).
    ///
    /// This means that underlying hardware elements are shared and it is not possible to
    /// transition a single member of the power domain.
    #[inline]
    pub const fn targets_power_domain(&self) -> bool {
        (self.options & POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT) == 0
    }

    /// Power level index as understood from the original model perspective.
    #[inline]
    pub const fn level(&self) -> u8 {
        self.level
    }
}

/// Opaque Rust facade for C++ `power_management::PowerLevelController`.
// Safety invariant: Any reference `&CppPowerLevelController` aliases a live, initialized C++
// `power_management::PowerLevelController` instance; this zero-sized type is never constructed by
// value in Rust.
#[repr(C)]
pub struct CppPowerLevelController {
    _facade: OpaqueFacade,
}

impl CppPowerLevelController {
    /// Determines whether the controller is serving requests or not.
    ///
    /// Allows scheduler instances to interrogate whether they should consider any active power
    /// levels or not.
    #[inline]
    pub fn is_serving(&self) -> bool {
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController`.
        unsafe { cpp_power_level_controller_is_serving(self) }
    }

    /// Returns the control interface this power level controller implements.
    #[inline]
    pub fn control_interface(&self) -> ControlInterface {
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController` with a
        // valid `ControlInterface` discriminant.
        unsafe { cpp_power_level_controller_control_interface(self) }
    }

    /// Returns the current device-specific power level (i.e. the value passed as a control
    /// argument) for the given `domain_id`, if supported by the controller. May be implemented by
    /// in-kernel CPU drivers that are capable of querying the current hardware state.
    #[inline]
    pub fn get_current_power_level(&self, domain_id: u32) -> Result<u64, Status> {
        let mut level = 0u64;
        // SAFETY: `self` is a valid reference to an initialized `PowerLevelController` and
        // `&mut level` is a valid writable `u64` on the stack.
        let status = unsafe {
            cpp_power_level_controller_get_current_power_level(self, domain_id, &mut level)
        };
        Status::ok(status)?;
        Ok(level)
    }
}

/// Opaque Rust facade for C++ `power_management::EnergyModel`.
///
/// EnergyModel describes the power consumption rates of the available active and idle states a
/// processor may enter, which interfaces to use to effect state transitions, and properties of
/// the state transitions, such as energy cost and latency.
// Safety invariant: Any reference `&EnergyModel` aliases a live, initialized C++
// `power_management::EnergyModel` whose `power_levels_` vector is immutable after construction and
// contains initialized `PowerLevel` elements with valid `ControlInterface` discriminants.
#[repr(C)]
pub struct EnergyModel {
    _facade: OpaqueFacade,
}

impl EnergyModel {
    /// Returns the total number of power levels in the model (`EnergyModel::levels().size()`).
    #[inline]
    pub fn levels_len(&self) -> usize {
        // SAFETY: `self` is a valid reference to an initialized `EnergyModel`.
        unsafe { cpp_energy_model_levels_size(self) }
    }

    /// Returns the number of idle power levels in the model (`EnergyModel::idle_levels().size()`).
    #[inline]
    pub fn idle_levels_len(&self) -> usize {
        // SAFETY: `self` is a valid reference to an initialized `EnergyModel`.
        unsafe { cpp_energy_model_idle_levels_size(self) }
    }

    /// All power levels described in the model, sorted by processing power and energy consumption.
    ///
    /// (1) The processing rate of power level `i` is less than or equal to the processing rate of
    /// power level `j`, where `i <= j`.
    ///
    /// (2) The energy cost of power level `i` is less than or equal to the processing rate of
    /// power level `j`, where `i <= j`.
    #[inline]
    pub fn levels(&self) -> &[PowerLevel] {
        // SAFETY: By `EnergyModel`'s type invariant, `self` aliases a live, initialized C++
        // `EnergyModel` whose `power_levels_` vector is immutable for the lifetime of `self`.
        // When `self.levels_len() > 0`, `cpp_energy_model_levels_data(self)` returns a non-null,
        // properly aligned pointer to `self.levels_len()` contiguous initialized `PowerLevel`
        // elements in a single allocation, satisfying `zr::slice_from_raw_parts`.
        unsafe { zr::slice_from_raw_parts(cpp_energy_model_levels_data(self), self.levels_len()) }
    }

    /// Following the same rules as [`Self::levels`] but returns only the set of power levels whose
    /// type is `PowerLevel::Type::kIdle`. This set may be empty.
    #[inline]
    pub fn idle_levels(&self) -> &[PowerLevel] {
        &self.levels()[..self.idle_levels_len()]
    }

    /// Following the same rules as [`Self::levels`] but returns only the set of power levels whose
    /// type is `PowerLevel::Type::kActive`. This set may be empty.
    #[inline]
    pub fn active_levels(&self) -> &[PowerLevel] {
        &self.levels()[self.idle_levels_len()..]
    }

    /// Returns the idle power level with the maximum power consumption.
    #[inline]
    pub fn max_idle_power_level(&self) -> Option<u8> {
        let idle_len = self.idle_levels_len();
        if idle_len > 0 { Some((idle_len - 1) as u8) } else { None }
    }

    /// Returns the power coefficient of the idle power level with the maximum power consumption.
    /// This idle power level typically corresponds to clock gating, such that the power
    /// consumption is almost entirely leakage power loss.
    #[inline]
    pub fn max_idle_power_coefficient_nw(&self) -> Option<u64> {
        let idle_levels = self.idle_levels();
        idle_levels.last().map(PowerLevel::power_coefficient_nw)
    }

    /// Returns the control interface of the idle power level with the maximum power consumption.
    #[inline]
    pub fn max_idle_power_level_interface(&self) -> Option<ControlInterface> {
        let idle_levels = self.idle_levels();
        idle_levels.last().map(PowerLevel::control)
    }

    /// Returns the processing rate of the fastest power level.
    #[inline]
    pub fn max_processing_rate(&self) -> ProcessingRate {
        self.levels().last().map_or(ProcessingRate::from_raw(0), PowerLevel::processing_rate)
    }

    /// Looks up the power level index matching `(interface_id, control_argument)`.
    #[inline]
    pub fn find_power_level(
        &self,
        interface_id: ControlInterface,
        control_argument: u64,
    ) -> Option<u8> {
        let mut level = 0u8;
        // SAFETY: `self` is a valid reference to an initialized `EnergyModel` and `&mut level` is
        // a valid writable `u8`.
        let found = unsafe {
            cpp_energy_model_find_power_level(self, interface_id, control_argument, &mut level)
        };
        if found { Some(level) } else { None }
    }
}

fbl::impl_opaque_ref_counted_facade!(
    /// PowerDomain establishes the relationship between a set of CPUs, the energy model that
    /// describes their characteristics, and the power level controller responsible for changing
    /// the active power levels for the set of CPUs.
    ///
    /// Instances of PowerDomain are safe for concurrent use.
    pub struct PowerDomain,
    cpp_power_domain_recycle,
);

impl PowerDomain {
    /// ID representing the relationship between a set of CPUs and a power model.
    #[inline]
    pub fn id(&self) -> u32 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_id(self) }
    }

    /// Set of CPUs associated with `model()`.
    #[inline]
    pub fn cpus(&self) -> u64 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_cpus(self) }
    }

    /// Model describing the behavior of the power domain.
    #[inline]
    pub fn model(&self) -> &EnergyModel {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`, and `model_` lives
        // inline inside `PowerDomain` for the lifetime of `self`.
        unsafe { cpp_power_domain_model(self) }
    }

    /// The total normalized utilization of the set of processors associated with this power domain.
    ///
    /// Uses relaxed semantics, since the value does not need to synchronize with other memory
    /// accesses and inaccuracy is acceptable.
    #[inline]
    pub fn total_normalized_utilization(&self) -> Utilization {
        Utilization::from_raw(self.total_normalized_utilization_atomic().load(Ordering::Relaxed))
    }

    /// Handler for transitions where the target level's control interface is not kernel handled.
    #[inline]
    pub fn controller(&self) -> &CppPowerLevelController {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain` whose `controller_`
        // is non-null (`ZX_DEBUG_ASSERT(controller_ != nullptr)`) and lives as long as `self`.
        unsafe { cpp_power_domain_controller(self) }
    }

    /// Returns whether the kernel scheduler should send power level update requests to the
    /// controller. This is used by tests to prevent the scheduler from sending power level change
    /// requests through the fake control interface that could confuse the test. It does not
    /// prevent the kernel from exercising the control interface, however, only whether the
    /// scheduler will interact with the control interface to handle utilization changes.
    ///
    /// Uses relaxed semantics, since this variable will generally be synchronized by the lock
    /// protecting each `PowerState` as it is associated with this `PowerDomain`. However, an
    /// atomic is used to prevent formal data races if the value is read outside of external
    /// synchronization, which can't be statically checked internally.
    #[inline]
    pub fn scheduler_control_enabled(&self) -> bool {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`.
        unsafe { cpp_power_domain_scheduler_control_enabled(self) }
    }

    /// Returns a reference to the atomic `total_normalized_utilization_` counter of `self`.
    ///
    /// Callers may use `Ordering::Relaxed` for loads and read-modify-write operations (`fetch_add`
    /// and `fetch_sub`) because the domain utilization sum is a standalone heuristic metric that
    /// does not synchronize other memory accesses.
    #[inline]
    pub(super) fn total_normalized_utilization_atomic(&self) -> &AtomicI64 {
        // SAFETY: `self` is a valid reference to an initialized `PowerDomain`, and
        // `total_normalized_utilization_` is an inline `mutable std::atomic<int64_t>` with
        // identical size and alignment to `AtomicI64`.
        unsafe { cpp_power_domain_total_normalized_utilization_ptr(self) }
    }
}

/// Maintains an array of ref pointers to the power domains in the system. This is used for both
/// the power domain registry storage and processor-local caches of the power domain set.
/// Processor-local caches avoid cross-processor and central lock contention when consulting a
/// snapshot of the system power domains during sensitive scheduling operations.
#[repr(C)]
#[derive(Clone, Default)]
pub struct PowerDomainSet {
    domains: [Option<RefPtr<PowerDomain>>; MAX_POWER_DOMAINS],
}

zr::static_assert!(size_of::<Option<RefPtr<PowerDomain>>>() == 8);
zr::static_assert!(align_of::<Option<RefPtr<PowerDomain>>>() == 8);
zr::static_assert!(size_of::<PowerDomainSet>() == 32);
zr::static_assert!(align_of::<PowerDomainSet>() == 8);
zr::static_assert!(offset_of!(PowerDomainSet, domains) == 0);

impl PowerDomainSet {
    /// Creates an empty `PowerDomainSet`.
    #[inline]
    pub const fn new() -> Self {
        Self { domains: [None, None, None, None] }
    }

    /// Creates a `PowerDomainSet` with the given `PowerDomain` as its only entry for testing.
    #[inline]
    pub fn create_for_test(domain: &RefPtr<PowerDomain>) -> Self {
        Self { domains: [Some(RefPtr::clone(domain)), None, None, None] }
    }

    /// Returns a borrowed reference to the power domain for the given CPU id, or `None` if there
    /// isn't one (`PowerDomainSet::FindByCpuNum`). Returns a reference to avoid unnecessary ref
    /// count changes in contexts where the set is guaranteed not to change and maintain the
    /// lifetime of its power domain elements.
    #[inline]
    pub fn find_by_cpu_num(&self, cpu_num: u32) -> Option<&RefPtr<PowerDomain>> {
        #[cfg(not(feature = "userspace"))]
        let cpu_mask = {
            debug_assert!(cpu_num < crate::kernel::types::cpu_mask_t::BITS);
            crate::kernel::types::cpu_num_to_mask(cpu_num) as u64
        };
        #[cfg(feature = "userspace")]
        let cpu_mask = {
            debug_assert!(cpu_num < u64::BITS);
            1u64.checked_shl(cpu_num).unwrap_or(0)
        };
        self.domains.iter().flatten().find(|element| (element.cpus() & cpu_mask) != 0)
    }
}
