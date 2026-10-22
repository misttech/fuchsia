// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Votes cast by the `fake_governor` test component, shared with the integration test so that it
//! can verify them in battery-manager's Inspect.

use fidl_fuchsia_hardware_power_charger as fcharger;

/// `charge_current_limit_ua` voted by the fake governor.
pub const FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA: u32 = 500_000;

/// `operating_mode` voted by the fake governor.
pub const FAKE_GOVERNOR_OPERATING_MODE: fcharger::OperatingMode =
    fcharger::OperatingMode::Passthrough;
