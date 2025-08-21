// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use isolated_swd::updater::Updater;

pub async fn apply_update() -> Result<(), Error> {
    let mut updater = Updater::new().context("Failed to create updater")?;
    // TODO(https://fxbug.dev/419106573): get the server address from adb sideload
    updater
        .install_update(Some(&"http://localhost:8083/ota_manifest.json".parse().unwrap()))
        .await
        .context("Failed to apply update")
}
