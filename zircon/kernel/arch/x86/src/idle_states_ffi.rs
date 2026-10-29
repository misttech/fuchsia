// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;

use zx_types::zx_duration_t;

use super::feature::x86_idle_states_t;
use super::idle_states::{X86IdleState, X86IdleStates, x86_get_idle_states};

/// Returns a read-only pointer to the list of idle states supported by the
/// system.
#[unsafe(no_mangle)]
pub extern "C" fn rust_x86_get_idle_states() -> *const x86_idle_states_t {
    x86_get_idle_states()
}

/// Returns the NUL-terminated C string name of `idle_state`.
///
/// # Safety
///
/// `idle_state` must be a valid, aligned reference to an initialized `X86IdleState` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_state_name(idle_state: &X86IdleState) -> *const c_char {
    idle_state.raw_state().name
}

/// Returns the MWAIT hint for `idle_state`.
///
/// # Safety
///
/// `idle_state` must be a valid, aligned reference to an initialized `X86IdleState` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_state_mwait_hint(idle_state: &X86IdleState) -> u32 {
    idle_state.mwait_hint()
}

/// Marks `idle_state` as entered.
///
/// # Safety
///
/// `idle_state` must be a valid, aligned reference to an initialized `X86IdleState` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_state_count_entry(idle_state: &X86IdleState) {
    idle_state.count_entry();
}

/// Records `duration` spent in `idle_state`.
///
/// # Safety
///
/// `idle_state` must be a valid, aligned reference to an initialized `X86IdleState` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_state_record_duration(
    idle_state: &X86IdleState,
    duration: zx_duration_t,
) {
    idle_state.record_duration(duration);
}

/// Initializes `idle_states` in place from the supported C-state table `states`.
///
/// # Safety
///
/// - `idle_states` must be non-null, properly aligned, and valid for writes of
///   `size_of::<X86IdleStates>()` bytes.
/// - `states` must be a valid, aligned reference to an initialized `x86_idle_states_t` that lives
///   for `'static`, and every populated entry in `states.states` up to and including the
///   terminating base `C1` state must have a `name` pointer that is either null or points to a
///   valid NUL-terminated `'static` C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_init(
    idle_states: *mut X86IdleStates,
    states: &'static x86_idle_states_t,
) {
    // SAFETY: Caller guarantees `idle_states` is valid for writes and `states` upholds the
    // `'static` and NUL-terminated `name` preconditions.
    unsafe {
        X86IdleStates::init_ptr(idle_states, states);
    }
}

/// Returns a const pointer to the array of `X86IdleState` entries in `idle_states`.
///
/// # Safety
///
/// `idle_states` must be a valid, aligned reference to an initialized `X86IdleStates` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_states(
    idle_states: &X86IdleStates,
) -> *const X86IdleState {
    idle_states.states().as_ptr()
}

/// Returns the number of populated states in `idle_states`.
///
/// # Safety
///
/// `idle_states` must be a valid, aligned reference to an initialized `X86IdleStates` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_num_states(idle_states: &X86IdleStates) -> usize {
    idle_states.num_states()
}

/// Picks an idle state to enter from `idle_states`.
///
/// # Safety
///
/// `idle_states` must be a valid, aligned reference to an initialized `X86IdleStates` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_pick_idle_state(
    idle_states: &X86IdleStates,
) -> *const X86IdleState {
    idle_states.pick_idle_state()
}

/// Records the last idle `duration` in `idle_states`.
///
/// # Safety
///
/// `idle_states` must be a valid, aligned reference to an initialized `X86IdleStates` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_record_duration(
    idle_states: &X86IdleStates,
    duration: zx_duration_t,
) {
    idle_states.record_duration(duration);
}

/// Updates the bitmask of allowed C-states in `idle_states`.
///
/// # Safety
///
/// `idle_states` must be a valid, aligned reference to an initialized `X86IdleStates` for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_idle_states_set_state_mask(
    idle_states: &X86IdleStates,
    mask: u32,
) {
    idle_states.set_state_mask(mask);
}
