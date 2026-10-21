// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-processor CPU power state tracking and transition request generation.

use core::mem::{align_of, size_of};
use core::sync::atomic::Ordering;

use fbl::RefPtr;
use zx_status::Status;

use super::energy_model::{POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PowerDomain, PowerDomainSet};
pub use super::power_level_controller::PowerLevelUpdateRequest;
use super::{ControlInterface, ProcessingRate, Utilization};

/// PowerState encapsulates the current power level, power domain, and energy model for an
/// individual processor.
///
/// Instances of PowerState are not safe for concurrent use and must be protected by an external
/// lock associated with the owning processor.
#[repr(C)]
#[derive(Default)]
pub struct PowerState {
    /// The power domain set configured for this processor.
    power_domain_set: PowerDomainSet,

    /// The power domain the processor belongs to.
    domain: Option<RefPtr<PowerDomain>>,

    /// The power level when the processor is actively running.
    active_power_level: Option<u8>,

    /// Represents the power level that needs be to transitioned to.
    desired_active_power_level: Option<u8>,

    /// The current normalized utilization of the processor.
    normalized_utilization: Utilization,
}

zr::static_assert!(size_of::<PowerState>() == 56);
zr::static_assert!(align_of::<PowerState>() == 8);

impl PowerState {
    /// Creates a new default `PowerState`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            power_domain_set: PowerDomainSet::new(),
            domain: None,
            active_power_level: None,
            desired_active_power_level: None,
            normalized_utilization: Utilization::from_raw(0),
        }
    }

    /// Returns true if the power domain is set.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        self.domain().is_some()
    }

    /// Domain the PowerState is being modeled after. Returns `None` if there is
    /// no power domain configured for processor owning this power state.
    #[inline]
    pub const fn domain(&self) -> &Option<RefPtr<PowerDomain>> {
        &self.domain
    }

    /// Returns the id of the domain this power state belongs to.
    #[inline]
    pub fn domain_id(&self) -> Option<u32> {
        if let Some(domain) = self.domain() {
            return Some(domain.id());
        }
        None
    }

    /// Returns whether the domain's controller is serving requests. Returns false if there is no
    /// domain.
    #[inline]
    pub fn is_serving(&self) -> bool {
        self.domain().as_ref().is_some_and(|domain| domain.controller().is_serving())
    }

    /// Returns whether the kernel scheduler should send power level update requests through the
    /// control interface to handle utilization changes.
    #[inline]
    pub fn scheduler_control_enabled(&self) -> bool {
        self.domain().as_ref().is_some_and(|domain| domain.scheduler_control_enabled())
    }

    /// Returns the active power level when device is not idle.
    #[inline]
    pub const fn active_power_level(&self) -> Option<u8> {
        self.active_power_level
    }

    /// Returns the active power level the device is in the process of transitioning to.
    #[inline]
    pub const fn desired_active_power_level(&self) -> Option<u8> {
        self.desired_active_power_level
    }

    /// Returns the maximum (i.e. highest power, lowest latency) idle power level.
    #[inline]
    pub fn max_idle_power_level(&self) -> Option<u8> {
        if let Some(domain) = self.domain() {
            return domain.model().max_idle_power_level();
        }
        None
    }

    /// Returns the power coefficient of the current active power level. Returns zero if there is no
    /// active power level or no domain is set.
    #[inline]
    pub fn active_power_coefficient_nw(&self) -> u64 {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            return domain.model().levels()[active_power_level as usize].power_coefficient_nw();
        }
        0
    }

    /// Returns the power coefficient of the maximum idle power level. Returns zero if there is no
    /// idle power level or no domain is set.
    #[inline]
    pub fn max_idle_power_coefficient_nw(&self) -> u64 {
        if let Some(domain) = self.domain() {
            return domain.model().max_idle_power_coefficient_nw().unwrap_or(0);
        }
        0
    }

    /// Returns the control interface of the maximum idle power level. Returns `None` if there is no
    /// idle power level or no domain is set.
    #[inline]
    pub fn max_idle_power_level_interface(&self) -> Option<ControlInterface> {
        if let Some(domain) = self.domain() {
            return domain.model().max_idle_power_level_interface();
        }
        None
    }

    /// Returns true if the given power level is valid active power level
    /// (`IsValidActivePowerLevel`).
    #[inline]
    pub fn is_valid_active_power_level(&self, power_level: u8) -> bool {
        if let Some(domain) = self.domain() {
            let model = domain.model();
            let level = power_level as usize;
            return level >= model.idle_levels_len() && level < model.levels_len();
        }
        false
    }

    /// Returns the processing rate of the fastest power level in this domain.
    #[inline]
    pub fn max_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain() {
            return domain.model().max_processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the current active power level. Returns zero if there is no
    /// active power level or no domain is set.
    #[inline]
    pub fn active_processing_rate(&self) -> ProcessingRate {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            return domain.model().levels()[active_power_level as usize].processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the preceding active power level (i.e. the
    /// lower bound of the current active power level). Returns zero if there is no
    /// preceding active power level, no active power level is set, or no domain
    /// is set.
    #[inline]
    pub fn preceding_active_processing_rate(&self) -> ProcessingRate {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            let model = domain.model();
            let index = active_power_level as usize;
            if index > model.idle_levels_len() {
                return model.levels()[index - 1].processing_rate();
            }
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the targeted active power level (the in-flight desired level if set,
    /// otherwise the current active level).
    #[inline]
    pub fn target_active_power_level(&self) -> Option<u8> {
        self.desired_active_power_level.or(self.active_power_level)
    }

    /// Returns the processing rate of the targeted active power level (the in-flight
    /// desired level if set, otherwise the current active level).
    #[inline]
    pub fn target_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain()
            && let Some(level) = self.target_active_power_level()
        {
            return domain.model().levels()[level as usize].processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the active power level immediately preceding
    /// the targeted active power level (i.e. the lower bound of the targeted active
    /// power level).
    #[inline]
    pub fn preceding_target_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain()
            && let Some(level) = self.target_active_power_level()
        {
            let model = domain.model();
            let index = level as usize;
            if index > model.idle_levels_len() {
                return model.levels()[index - 1].processing_rate();
            }
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns true if the targeted active power level has an active power level
    /// preceding it (i.e. is not the lowest active power level).
    #[inline]
    pub fn has_preceding_target_power_level(&self) -> bool {
        if let Some(domain) = self.domain()
            && let Some(level) = self.target_active_power_level()
        {
            return (level as usize) > domain.model().idle_levels_len();
        }
        false
    }

    /// Returns the current utilization of the processor.
    #[inline]
    pub const fn normalized_utilization(&self) -> Utilization {
        self.normalized_utilization
    }

    /// Returns the total normalized utilization of the domain this processor belongs to.
    #[inline]
    pub fn total_normalized_utilization(&self) -> Utilization {
        if let Some(domain) = self.domain() {
            return domain.total_normalized_utilization();
        }
        Utilization::from_raw(0)
    }

    /// Updates the power domain set and associates this power state with the domain in the set
    /// matching the given CPU number, swapping `power_domain_set` with the previous power domain
    /// set in place (`UpdatePowerDomainSet`).
    pub fn update_power_domain_set(&mut self, power_domain_set: &mut PowerDomainSet, cpu_num: u32) {
        let domain = power_domain_set.find_by_cpu_num(cpu_num).cloned();

        if let Some(current_domain) = &self.domain {
            current_domain
                .total_normalized_utilization_atomic()
                .fetch_sub(self.normalized_utilization.raw_value(), Ordering::Relaxed);
        }

        self.active_power_level = None;
        self.desired_active_power_level = None;
        self.domain = domain;

        if let Some(domain) = &self.domain {
            let control_argument_result = domain.controller().get_current_power_level(domain.id());
            if let Ok(control_argument) = control_argument_result {
                let level = domain
                    .model()
                    .find_power_level(domain.controller().control_interface(), control_argument);
                self.active_power_level = level;
                self.desired_active_power_level = level;
            }

            domain
                .total_normalized_utilization_atomic()
                .fetch_add(self.normalized_utilization.raw_value(), Ordering::Relaxed);
        }

        core::mem::swap(power_domain_set, &mut self.power_domain_set);
    }

    /// Returns the power domain set configured for this processor.
    #[inline]
    pub const fn power_domain_set(&self) -> &PowerDomainSet {
        &self.power_domain_set
    }

    /// Posts a request to transition the power domain associated with this power state to a given
    /// active power level from the current energy model (`RequestTransition`).
    ///
    /// Ignored when no domain is set. Asserts that the requested power level is an active power
    /// level.
    ///
    /// `cpu_num` is the CPU associated with this power state.
    /// `active_power_level` is the desired power level.
    pub fn request_transition(
        &mut self,
        cpu_num: u32,
        active_power_level: u8,
    ) -> Option<PowerLevelUpdateRequest> {
        let Some(domain) = &self.domain else {
            return None;
        };

        self.active_power_level?;

        let model = domain.model();
        assert!(
            (active_power_level as usize) >= model.idle_levels_len()
                && (active_power_level as usize) < model.levels_len(),
            "ASSERT FAILED"
        );

        if self.desired_active_power_level == Some(active_power_level) {
            return None;
        }

        self.desired_active_power_level = Some(active_power_level);

        let level = &model.levels()[active_power_level as usize];

        let mut transition = PowerLevelUpdateRequest {
            domain_id: domain.id(),
            target_id: domain.id(),
            control: level.control(),
            control_argument: level.control_argument(),
            options: 0,
        };

        if level.targets_cpus() {
            transition.target_id = cpu_num;
            transition.options |= POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT as u32;
        }

        Some(transition)
    }

    /// Updates the active power level of this power state (`UpdateActivePowerLevel`). Typically
    /// called in response to the power level controller completing a power level transition
    /// request.
    pub fn update_active_power_level(&mut self, level: u8) -> Result<(), Status> {
        let Some(domain) = &self.domain else {
            return Err(Status::BAD_STATE);
        };

        let model = domain.model();
        if (level as usize) < model.idle_levels_len() || (level as usize) >= model.levels_len() {
            return Err(Status::OUT_OF_RANGE);
        }

        let was_at_desired = self.desired_active_power_level == self.active_power_level;
        self.active_power_level = Some(level);
        if was_at_desired || self.desired_active_power_level == Some(level) {
            self.desired_active_power_level = Some(level);
        }
        Ok(())
    }

    /// Update the utilization of this processor and the total utilization of its domain
    /// (`UpdateUtilization`).
    #[inline]
    pub fn update_utilization(&mut self, utilization_delta: Utilization) -> Utilization {
        if let Some(domain) = self.domain() {
            domain
                .total_normalized_utilization_atomic()
                .fetch_add(utilization_delta.raw_value(), Ordering::Relaxed);
        }
        self.normalized_utilization += utilization_delta;
        self.normalized_utilization
    }
}
