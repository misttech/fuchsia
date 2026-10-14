// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::wake_vector::WakeResult;
use percpu_bindings as bindings;
use zx_types::zx_instant_boot_t;

unsafe extern "C" {
    fn cpp_idle_power_thread_trigger_system_wake_event() -> WakeResult;
    fn cpp_idle_power_thread_acknowledge_system_wake_event();
    fn cpp_idle_power_thread_transition_all_active_to_suspend(
        resume_deadline: zx_instant_boot_t,
    ) -> zx_instant_boot_t;
}

/// An opaque type representing the C++ `IdlePowerThread` class.
#[repr(transparent)]
pub struct IdlePowerThread(pub bindings::IdlePowerThread);

impl IdlePowerThread {
    /// Triggers a system wake event, waking the boot CPU if the system is suspended and this is
    /// the first pending wake event.
    #[inline]
    pub(crate) fn trigger_system_wake_event() -> WakeResult {
        // SAFETY: Foreign function call into `IdlePowerThread::TriggerSystemWakeEvent`.
        unsafe { cpp_idle_power_thread_trigger_system_wake_event() }
    }

    /// Acknowledges a pending system wake event.
    #[inline]
    pub(crate) fn acknowledge_system_wake_event() {
        // SAFETY: Foreign function call into `IdlePowerThread::AcknowledgeSystemWakeEvent`.
        unsafe { cpp_idle_power_thread_acknowledge_system_wake_event() }
    }

    /// Transitions all active CPUs into the suspend state until `resume_deadline` or a wake event
    /// occurs, returning the boot timestamp when the suspend transition started.
    #[inline]
    pub(crate) fn transition_all_active_to_suspend(
        resume_deadline: zx_instant_boot_t,
    ) -> zx_instant_boot_t {
        // SAFETY: Foreign function call into `IdlePowerThread::TransitionAllActiveToSuspend`.
        unsafe { cpp_idle_power_thread_transition_all_active_to_suspend(resume_deadline) }
    }
}
