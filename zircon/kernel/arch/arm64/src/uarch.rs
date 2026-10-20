// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Microarchitecture-specific workarounds and optimizations

use super::mp::read_percpu_field;
use arm64_mp_bindings as mp_bindings;

unsafe extern "C" {
    fn psci_get_version() -> u32;
}

/// Returns whether the calling CPU's microarchitecture needs the Spectre V2 mitigation.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_uarch_needs_spectre_v2_mitigation() -> bool {
    matches!(
        read_percpu_field!(microarch),
        mp_bindings::arm64_microarch_ARM_CORTEX_A57
            | mp_bindings::arm64_microarch_ARM_CORTEX_A72
            | mp_bindings::arm64_microarch_ARM_CORTEX_A73
            | mp_bindings::arm64_microarch_ARM_CORTEX_A75
            | mp_bindings::arm64_microarch_CAVIUM_CN99XX
    )
}

/// Spectre V2 mitigation for the calling CPU: a PSCI call, which firmware on affected cores uses
/// to invalidate the indirect branch predictors.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_uarch_do_spectre_v2_mitigation() {
    // Certain processors are vulnerable to branch target injection attacks (Spectre V2), where the
    // targets of indirect branches may be controlled by hostile code under speculation. The wrong
    // path speculation may leak secrets via cache side channels.
    //
    // Invalidate indirect branch predictors to guard the kernel by executing a PSCI call
    // TODO(https://fxbug.dev/42108888): Use SMCCC ARCH_WORKAROUND and v1.1 calling convention if
    // available.
    // SAFETY: The declaration matches `psci_get_version()` in `<dev/psci.h>`, which takes no
    // arguments and has no caller preconditions.
    unsafe { psci_get_version() };
}
