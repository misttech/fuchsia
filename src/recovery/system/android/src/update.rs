// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use isolated_swd::updater::Updater;

pub async fn apply_update(url: &str) -> Result<(), Error> {
    let mut updater = Updater::new().context("Failed to create updater")?;
    updater.install_update(Some(&url.parse()?)).await.context("Failed to apply update")
}
