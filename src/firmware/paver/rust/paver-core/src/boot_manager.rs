// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl_next_fuchsia_paver as fpaver;

/// Serves the `fuchsia.paver.BootManager` FIDL protocol over `server_end` using `manager`.
pub async fn serve_boot_manager<B: fpaver::BootManagerLocalServerHandler>(
    manager: B,
    server_end: fidl_next::ServerEnd<fpaver::BootManager>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(manager)
        .await
        .map(|_| ())
        .context("serving BootManager")
}
