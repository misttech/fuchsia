// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::test::*;
use anyhow::*;
use ffx_config::EnvironmentContext;
use ffx_executor::FfxExecutor;
use std::time::Duration;

pub(crate) async fn test_manual_add_target_list(context: EnvironmentContext) -> Result<()> {
    let isolate = new_isolate(&context, "target-manual-add-target-list").await?;
    isolate.start_daemon().await?;

    let _ = isolate.exec_ffx(&["target", "add", "--nowait", "[::1]:8022"]).await?;

    let out = isolate
        .exec_ffx(&["--target", "[::1]:8022", "target", "list", "--format", "a", "--no-probe"])
        .await?;

    ensure!(out.stdout.contains("[::1]:8022"), "stdout is unexpected: {:?}", out);
    ensure!(out.stderr.lines().count() == 0, "stderr is unexpected: {:?}", out);
    // TODO: establish a good way to assert against the whole target address.

    Ok(())
}

pub(crate) async fn test_manual_add_target_list_late_add(
    context: EnvironmentContext,
) -> Result<()> {
    let isolate = new_isolate(&context, "target-manual-add-target-list-late-add").await?;
    isolate.start_daemon().await?;

    let task = isolate.exec_ffx(&[
        "--target",
        "[::1]:8022",
        "target",
        "list",
        "--format",
        "a",
        "--no-probe",
    ]);

    // The target-list should pick up targets added after it has started, as well as before.
    fuchsia_async::Timer::new(Duration::from_millis(500)).await;

    let _ = isolate.exec_ffx(&["target", "add", "--nowait", "[::1]:8022"]).await?;

    let out = task.await?;

    ensure!(out.stdout.contains("[::1]:8022"), "stdout is unexpected: {:?}", out);
    ensure!(out.stderr.lines().count() == 0, "stderr is unexpected: {:?}", out);
    // TODO: establish a good way to assert against the whole target address.

    Ok(())
}

pub mod include_target {
    use super::*;

    pub(crate) async fn test_target_show(context: EnvironmentContext) -> Result<()> {
        let isolate = new_isolate(&context, "target-show").await?;
        isolate.start_daemon().await?;

        let target_nodeaddr = get_target_addr();

        let out = isolate.exec_ffx(&["--target", &target_nodeaddr, "target", "show"]).await?;

        ensure!(out.status.success(), "status is unexpected: {:?}", out);
        ensure!(!out.stdout.is_empty(), "stdout is unexpectedly empty: {:?}", out);
        ensure!(out.stderr.lines().count() == 0, "stderr is unexpected: {:?}", out);

        Ok(())
    }
}
