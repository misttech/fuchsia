// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "power_management.h"

#include <lib/zx/time.h>
#include <zircon/errors.h>

namespace pci {

void PowerManagementCapability::WaitForTransitionRecovery(
    PowerManagementCapability::PowerState old_state,
    PowerManagementCapability::PowerState new_state) {
  zx::duration wait_time = kStateRecoveryTime[old_state][new_state];
  zx::nanosleep(zx::deadline_after(wait_time));
}

PowerManagementCapability::PowerState PowerManagementCapability::GetPowerState(
    const pci::Config& cfg) {
  return static_cast<PowerState>(pmcsr_.ReadFrom(&cfg).power_state());
}

void PowerManagementCapability::SetPowerState(const pci::Config& cfg,
                                              PowerManagementCapability::PowerState new_state) {
  auto pmc = pmc_.ReadFrom(&cfg);

  // If we're already in the requested power state then we're finished.
  auto pmcsr = pmcsr_.ReadFrom(&cfg);
  uint8_t old_state_u8 = static_cast<uint8_t>(pmcsr.power_state());
  if (new_state == old_state_u8) {
    return;
  }

  // Power down transitions are always allowed, but power up transitions must
  // always go through D0. In other words, to go from D3 to D2 we must change
  // state from D3 > D0 > D2, whereas D1 to D3 is permitted directly.
  // ACPI 6.1 spec, section 2.3 Device Power State Definitions
  if (new_state != PowerState::D0 && old_state_u8 > new_state) {
    pmcsr.set_power_state(PowerState::D0).WriteTo(&cfg);
    if (!pmc.immediate_readiness_on_return_to_d0()) {
      WaitForTransitionRecovery(static_cast<PowerState>(old_state_u8), PowerState::D0);
    }
    old_state_u8 = PowerState::D0;
  }

  pmcsr.set_power_state(new_state).WriteTo(&cfg);
  if (new_state != PowerState::D0 || !pmc.immediate_readiness_on_return_to_d0()) {
    WaitForTransitionRecovery(static_cast<PowerState>(old_state_u8), new_state);
  }
}

}  // namespace pci
