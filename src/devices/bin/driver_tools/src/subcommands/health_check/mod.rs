// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::Result;
use args::HealthCheckCommand;
use flex_fuchsia_driver_development as fdd;
use std::io::Write;

pub async fn health_check(
    _cmd: HealthCheckCommand,
    _writer: &mut dyn Write,
    _driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    // The subcommand body lands in the next CL; fail loudly rather than pretend to have run.
    anyhow::bail!("ffx driver health-check is not implemented yet")
}
