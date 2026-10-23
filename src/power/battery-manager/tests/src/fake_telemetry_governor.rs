// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake charge governor that votes from the telemetry battery-manager serves it.
//!
//! Runs [`governor::run_governor`] against the `Battery`, `Charger`, and `Controller` connectors
//! that battery-manager hands it through `CreateChildArgs.additional_inputs`, with a
//! `sample_interval` so that `Watch` also completes with periodic samples. On every evaluation it
//! votes a `charge_current_limit_ua` derived from `level_percent`, a `charge_term_current_ua`
//! derived from `charge_phase`, and an `input_current_limit_ua` derived from the number of
//! statuses received, so that the integration test can verify in Inspect that the telemetry
//! arrived.

mod fake_telemetry_governor_votes;

use anyhow::Result;
use fake_telemetry_governor_votes::{
    MAX_INTERVAL_MILLIS, SAMPLED_RESPONSES, charge_current_limit_ua_for_level,
    charge_term_current_ua_for_phase, input_current_limit_ua_for_responses,
};
use fidl_fuchsia_hardware_power_charger as fcharger;
use governor::{Evaluation, Governor, Telemetry, run_governor};

#[derive(Default)]
struct FakeTelemetryGovernor {
    responses: u32,
}

impl Governor for FakeTelemetryGovernor {
    fn evaluate(&mut self, telemetry: &Telemetry) -> Evaluation {
        self.responses = self.responses.min(SAMPLED_RESPONSES - 1) + 1;
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
                input_current_limit_ua: Some(input_current_limit_ua_for_responses(self.responses)),
                ..Default::default()
            },
            sample_interval: Some(zx::BootDuration::from_millis(MAX_INTERVAL_MILLIS as i64)),
            ..Default::default()
        }
    }
}

#[fuchsia::main(logging_tags = ["fake_telemetry_governor"])]
async fn main() -> Result<()> {
    run_governor(FakeTelemetryGovernor::default()).await?;
    Ok(())
}
