// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;
use zx_types::zx_duration_t;

use super::mp::{X86IdleState, X86IdleStates};

unsafe extern "C" {
    #[link_name = "use_monitor"]
    pub(super) static mut USE_MONITOR: bool;

    #[link_name = "x86_num_cpus"]
    pub(super) static mut X86_NUM_CPUS: u8;

    /// Returns a pointer to the bootstrap processor's `X86IdleStates` table.
    ///
    /// # Safety
    ///
    /// Returns `bp_percpu.idle_states` initialized during early boot.
    pub(super) fn cpp_x86_mp_bp_idle_states() -> *mut X86IdleStates;

    /// Returns a pointer to AP `idx`'s `X86IdleStates` table (`ap_percpus[idx].idle_states`).
    ///
    /// # Safety
    ///
    /// Caller must ensure `idx < (x86_num_cpus - 1) as usize`.
    pub(super) fn cpp_x86_mp_ap_idle_states(idx: usize) -> *mut X86IdleStates;

    /// Returns the static C string name of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `X86IdleState`.
    pub(super) fn cpp_x86_idle_state_name(state: *const X86IdleState) -> *const c_char;

    /// Returns the MWAIT hint for `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `X86IdleState`.
    pub(super) fn cpp_x86_idle_state_mwait_hint(state: *const X86IdleState) -> u32;

    /// Returns the number of times `state` has been entered.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `X86IdleState`.
    pub(super) fn cpp_x86_idle_state_times_entered(state: *const X86IdleState) -> u64;

    /// Returns the cumulative duration spent in `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `X86IdleState`.
    pub(super) fn cpp_x86_idle_state_cumulative_duration(
        state: *const X86IdleState,
    ) -> zx_duration_t;

    /// Resets the entry and duration counters of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `X86IdleState`.
    pub(super) fn cpp_x86_idle_state_reset_counters(state: *mut X86IdleState);

    /// Returns a pointer to the `X86_MAX_CSTATES`-element `states_` array in `states`.
    ///
    /// # Safety
    ///
    /// `states` must point to a valid, initialized `X86IdleStates`.
    pub(super) fn cpp_x86_idle_states_states(states: *mut X86IdleStates) -> *mut X86IdleState;

    /// Returns the number of populated states in `states`.
    ///
    /// # Safety
    ///
    /// `states` must point to a valid, initialized `X86IdleStates`.
    pub(super) fn cpp_x86_idle_states_num_states(states: *const X86IdleStates) -> usize;

    /// Updates the mask of valid C-states in `states`.
    ///
    /// # Safety
    ///
    /// `states` must point to a valid, initialized `X86IdleStates`.
    pub(super) fn cpp_x86_idle_states_set_state_mask(states: *mut X86IdleStates, mask: u32);
}
