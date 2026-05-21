// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_erofs::{ErofsMarker, ErofsRequest, ErofsRequestStream};
use fidl_fuchsia_io as fio;
use futures::TryStreamExt;
use vfs::execution_scope::ExecutionScope;

#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    log::info!("Starting EROFS component");

    let outgoing =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
            .context("missing DirectoryRequest startup handle")?;

    let scope = ExecutionScope::new();

    let export = vfs::pseudo_directory! {
        "svc" => vfs::pseudo_directory! {
            ErofsMarker::PROTOCOL_NAME =>
                vfs::service::host(move |stream: ErofsRequestStream| {
                    async move {
                        if let Err(e) = handle_request_stream(stream).await {
                            log::error!("Error handling stream: {:?}", e);
                        }
                    }
                }),
        }
    };

    vfs::directory::serve_on(
        export,
        fio::PERM_READABLE,
        scope.clone(),
        fidl::endpoints::ServerEnd::new(outgoing.into()),
    );

    scope.wait().await;
    Ok(())
}

async fn handle_request_stream(mut stream: ErofsRequestStream) -> Result<(), anyhow::Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            ErofsRequest::Serve { payload, responder } => {
                log::debug!("Received Serve request");
                let _backing_vmo = payload.backing_vmo.context("Missing backing_vmo")?;
                let _root = payload.root.context("Missing root")?;

                log::info!("Serving new EROFS instance");
                // TODO(https://fxbug.dev/479841115): actually serve something!
                responder.send(Ok(()))?;
            }
            _ => {}
        }
    }
    Ok(())
}
