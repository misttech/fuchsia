// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
use super::idle_states::X86IdleStates;

unsafe extern "C" {
    #[cfg(console_enabled)]
    #[link_name = "use_monitor"]
    pub(super) static mut USE_MONITOR: bool;

    #[cfg(console_enabled)]
    #[link_name = "x86_num_cpus"]
    pub(super) static mut X86_NUM_CPUS: u8;

    /// Returns a pointer to the bootstrap processor's `X86IdleStates` table.
    ///
    /// # Safety
    ///
    /// Returns `bp_percpu.idle_states` initialized during early boot.
    #[cfg(console_enabled)]
    pub(super) fn cpp_x86_mp_bp_idle_states() -> *mut X86IdleStates;

    /// Returns a pointer to AP `idx`'s `X86IdleStates` table (`ap_percpus[idx].idle_states`), or
    /// null if `use_monitor` is false.
    ///
    /// # Safety
    ///
    /// Caller must ensure `idx < (x86_num_cpus - 1) as usize`.
    #[cfg(console_enabled)]
    pub(super) fn cpp_x86_mp_ap_idle_states(idx: usize) -> *mut X86IdleStates;

    /// Forcibly stops all other CPUs except the current one and the BSP (which is cpu 0).
    pub(super) fn x86_force_halt_all_but_local_and_bsp();
}
