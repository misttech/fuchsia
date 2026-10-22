// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake charge governor that votes from the telemetry battery-manager serves it.
//!
//! Runs [`governor::run_governor`] against the `Battery`, `Charger`, and `Controller` connectors
//! that battery-manager hands it through `CreateChildArgs.additional_inputs`, voting a
//! `charge_current_limit_ua` derived from `level_percent` and a `charge_term_current_ua` derived
//! from `charge_phase`, so that the integration test can verify in Inspect that the telemetry
//! arrived.

mod fake_telemetry_governor_votes;

use anyhow::Result;
use fake_telemetry_governor_votes::{
    charge_current_limit_ua_for_level, charge_term_current_ua_for_phase,
};
use fidl_fuchsia_hardware_power_charger as fcharger;
use governor::{Evaluation, Governor, Telemetry, run_governor};

struct FakeTelemetryGovernor;

impl Governor for FakeTelemetryGovernor {
    fn evaluate(&mut self, telemetry: &Telemetry) -> Evaluation {
        Evaluation {
            vote: fcharger::ControlOptions {
                charge_current_limit_ua: telemetry
                    .battery
                    .level_percent
                    .map(charge_current_limit_ua_for_level),
                charge_term_current_ua: telemetry
                    .charger
                    .as_ref()
                    .and_then(|charger| charger.charge_phase)
                    .map(charge_term_current_ua_for_phase),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

#[fuchsia::main(logging_tags = ["fake_telemetry_governor"])]
async fn main() -> Result<()> {
    run_governor(FakeTelemetryGovernor).await?;
    Ok(())
}
