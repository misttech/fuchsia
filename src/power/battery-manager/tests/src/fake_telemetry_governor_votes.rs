// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Votes cast by the `fake_telemetry_governor` test component, shared with the integration test
//! so that it can verify them in battery-manager's Inspect.

use fidl_fuchsia_hardware_power_charger as fcharger;

/// `charge_current_limit_ua` the fake telemetry governor votes per percent of battery level.
const CHARGE_CURRENT_LIMIT_UA_PER_LEVEL_PERCENT: u32 = 10_000;

/// Returns the `charge_current_limit_ua` the fake telemetry governor votes after receiving a
/// battery status with `level_percent`.
pub fn charge_current_limit_ua_for_level(level_percent: f32) -> u32 {
    level_percent.round() as u32 * CHARGE_CURRENT_LIMIT_UA_PER_LEVEL_PERCENT
}

/// `charge_term_current_ua` the fake telemetry governor votes per `ChargePhase` primitive value.
const CHARGE_TERM_CURRENT_UA_PER_CHARGE_PHASE: u32 = 1_000;

/// Returns the `charge_term_current_ua` the fake telemetry governor votes after receiving a
/// charger status with `charge_phase`.
pub fn charge_term_current_ua_for_phase(charge_phase: fcharger::ChargePhase) -> u32 {
    charge_phase.into_primitive() * CHARGE_TERM_CURRENT_UA_PER_CHARGE_PHASE
}
