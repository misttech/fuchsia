// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C FFI entry points for `PowerState`.

use fbl::RefPtr;
use zx_status::Status;
use zx_types::zx_status_t;

use super::energy_model::{PowerDomain, PowerDomainSet};
use super::power_state::{PowerLevelUpdateRequest, PowerState};
use super::{ControlInterface, ProcessingRate, Utilization};

/// Initializes a default `PowerState` in-place at `state`.
///
/// # Safety
///
/// The caller must ensure that `state` is non-null, properly aligned to `align_of::<PowerState>()`,
/// points to writable storage of at least `size_of::<PowerState>()` bytes, and is exclusively
/// accessed (not aliased by any live reference or concurrent access) for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_state_init(state: *mut PowerState) {
    // SAFETY: Caller guarantees `state` is non-null, properly aligned, writable for
    // `size_of::<PowerState>()` bytes, and exclusively accessed during this write.
    unsafe { state.write(PowerState::new()) };
}

/// Drops a `PowerState` in-place at `state`.
///
/// # Safety
///
/// The caller must ensure that `state` is non-null, properly aligned, points to a live, initialized
/// `PowerState` whose fields satisfy their drop invariants, is exclusively accessed during this
/// call, and will not be accessed again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_state_destroy(state: *mut PowerState) {
    // SAFETY: Caller guarantees `state` is non-null, properly aligned, exclusively accessed, and
    // points to a valid initialized `PowerState` that will not be used again after being dropped.
    unsafe { core::ptr::drop_in_place(state) };
}

/// FFI entry point for [`PowerState::is_enabled`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_is_enabled(state: &PowerState) -> bool {
    state.is_enabled()
}

/// FFI entry point for [`PowerState::domain`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_domain(state: &PowerState) -> &Option<RefPtr<PowerDomain>> {
    state.domain()
}

/// FFI entry point for [`PowerState::domain_id`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_domain_id(state: &PowerState, out: &mut u32) -> bool {
    match state.domain_id() {
        Some(id) => {
            *out = id;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::is_serving`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_is_serving(state: &PowerState) -> bool {
    state.is_serving()
}

/// FFI entry point for [`PowerState::scheduler_control_enabled`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_scheduler_control_enabled(state: &PowerState) -> bool {
    state.scheduler_control_enabled()
}

/// FFI entry point for [`PowerState::active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_active_power_level(state: &PowerState, out: &mut u8) -> bool {
    match state.active_power_level() {
        Some(level) => {
            *out = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::desired_active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_desired_active_power_level(
    state: &PowerState,
    out: &mut u8,
) -> bool {
    match state.desired_active_power_level() {
        Some(level) => {
            *out = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::max_idle_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_max_idle_power_level(state: &PowerState, out: &mut u8) -> bool {
    match state.max_idle_power_level() {
        Some(level) => {
            *out = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::active_power_coefficient_nw`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_active_power_coefficient_nw(state: &PowerState) -> u64 {
    state.active_power_coefficient_nw()
}

/// FFI entry point for [`PowerState::max_idle_power_coefficient_nw`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_max_idle_power_coefficient_nw(state: &PowerState) -> u64 {
    state.max_idle_power_coefficient_nw()
}

/// FFI entry point for [`PowerState::max_idle_power_level_interface`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_max_idle_power_level_interface(
    state: &PowerState,
    out: &mut ControlInterface,
) -> bool {
    match state.max_idle_power_level_interface() {
        Some(iface) => {
            *out = iface;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::is_valid_active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_is_valid_active_power_level(
    state: &PowerState,
    power_level: u8,
) -> bool {
    state.is_valid_active_power_level(power_level)
}

/// FFI entry point for [`PowerState::max_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_max_processing_rate(state: &PowerState) -> ProcessingRate {
    state.max_processing_rate()
}

/// FFI entry point for [`PowerState::active_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_active_processing_rate(state: &PowerState) -> ProcessingRate {
    state.active_processing_rate()
}

/// FFI entry point for [`PowerState::preceding_active_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_preceding_active_processing_rate(
    state: &PowerState,
) -> ProcessingRate {
    state.preceding_active_processing_rate()
}

/// FFI entry point for [`PowerState::target_active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_target_active_power_level(
    state: &PowerState,
    out: &mut u8,
) -> bool {
    match state.target_active_power_level() {
        Some(level) => {
            *out = level;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::target_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_target_processing_rate(state: &PowerState) -> ProcessingRate {
    state.target_processing_rate()
}

/// FFI entry point for [`PowerState::preceding_target_processing_rate`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_preceding_target_processing_rate(
    state: &PowerState,
) -> ProcessingRate {
    state.preceding_target_processing_rate()
}

/// FFI entry point for [`PowerState::has_preceding_target_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_has_preceding_target_power_level(state: &PowerState) -> bool {
    state.has_preceding_target_power_level()
}

/// FFI entry point for [`PowerState::normalized_utilization`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_normalized_utilization(state: &PowerState) -> Utilization {
    state.normalized_utilization()
}

/// FFI entry point for [`PowerState::total_normalized_utilization`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_total_normalized_utilization(state: &PowerState) -> Utilization {
    state.total_normalized_utilization()
}

/// FFI entry point for [`PowerState::update_power_domain_set`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_update_power_domain_set(
    state: &mut PowerState,
    power_domain_set: &mut PowerDomainSet,
    cpu_num: u32,
) {
    state.update_power_domain_set(power_domain_set, cpu_num);
}

/// FFI entry point for [`PowerState::power_domain_set`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_power_domain_set(state: &PowerState) -> &PowerDomainSet {
    state.power_domain_set()
}

/// FFI entry point for [`PowerState::request_transition`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_request_transition(
    state: &mut PowerState,
    cpu_num: u32,
    active_power_level: u8,
    out_request: &mut PowerLevelUpdateRequest,
) -> bool {
    match state.request_transition(cpu_num, active_power_level) {
        Some(req) => {
            *out_request = req;
            true
        }
        None => false,
    }
}

/// FFI entry point for [`PowerState::update_active_power_level`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_update_active_power_level(
    state: &mut PowerState,
    active_power_level: u8,
) -> zx_status_t {
    Status::result_into_raw(state.update_active_power_level(active_power_level))
}

/// FFI entry point for [`PowerState::update_utilization`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_state_update_utilization(
    state: &mut PowerState,
    utilization_delta: Utilization,
) -> Utilization {
    state.update_utilization(utilization_delta)
}
