// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake charge governor launched by battery-manager into its `#governors` collection.
//!
//! Connects to the `fuchsia.hardware.power.charger.Controller` connector that battery-manager
//! hands it through `CreateChildArgs.additional_inputs`, casts a fixed set of votes, and then
//! stays alive so that the votes remain active.

mod fake_governor_votes;

use anyhow::{Context as _, Result};
use fake_governor_votes::{FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA, FAKE_GOVERNOR_OPERATING_MODE};
use fidl_fuchsia_hardware_power_charger as fcharger;
use fuchsia_component::client::connect_to_protocol;
use log::info;

#[fuchsia::main(logging_tags = ["fake_governor"])]
async fn main() -> Result<()> {
    let controller = connect_to_protocol::<fcharger::ControllerMarker>()
        .context("failed to connect to charger Controller")?;
    controller
        .set_control(&fcharger::ControlOptions {
            charge_current_limit_ua: Some(FAKE_GOVERNOR_CHARGE_CURRENT_LIMIT_UA),
            operating_mode: Some(FAKE_GOVERNOR_OPERATING_MODE),
            ..Default::default()
        })
        .await
        .context("SetControl transport error")?
        .map_err(|e| anyhow::anyhow!("SetControl rejected: {e:?}"))?;
    info!("Fake governor cast its votes");

    // Keep `controller` alive; closing the `Controller` connection withdraws this governor's votes.
    let _controller = controller;
    std::future::pending::<()>().await;
    Ok(())
}
