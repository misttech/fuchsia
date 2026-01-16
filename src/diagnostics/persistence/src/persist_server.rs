// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use fidl::endpoints;
use fidl_fuchsia_diagnostics_persist::{
    DataPersistenceMarker, DataPersistenceRequest, DataPersistenceRequestStream, PersistResult,
};
use futures::{StreamExt, TryStreamExt};
use log::warn;
use {fidl_fuchsia_component_sandbox as fsandbox, fuchsia_async as fasync};

/// PersistServer handles all requests for a single persistence service.
pub(crate) struct PersistServer;

impl PersistServer {
    /// Spawn a task to handle requests from components through a dynamic dictionary.
    pub fn spawn(scope: fasync::ScopeHandle, requests: fsandbox::ReceiverRequestStream) {
        scope.spawn(Self::accept_connections(requests, scope.clone()));
    }

    async fn accept_connections(
        mut stream: fsandbox::ReceiverRequestStream,
        scope: fasync::ScopeHandle,
    ) {
        while let Some(request) = stream.try_next().await.unwrap() {
            match request {
                fsandbox::ReceiverRequest::Receive { channel, control_handle: _ } => {
                    scope.spawn(async move {
                        let server_end =
                            endpoints::ServerEnd::<DataPersistenceMarker>::new(channel);
                        let stream: DataPersistenceRequestStream = server_end.into_stream();
                        if let Err(e) = Self::handle_requests(stream).await {
                            warn!("error handling persistence request: {e}");
                        }
                    });
                }
                fsandbox::ReceiverRequest::_UnknownMethod { ordinal, .. } => {
                    warn!(ordinal:%; "Unknown Receiver request");
                }
            }
        }
    }

    async fn handle_requests(mut stream: DataPersistenceRequestStream) -> Result<(), Error> {
        warn!("DataPersistence is deprecated; all requests are noop");
        while let Some(request) = stream.next().await {
            let request =
                request.map_err(|e| format_err!("error handling persistence request: {e:?}"))?;

            match request {
                DataPersistenceRequest::Persist { tag: _, responder } => {
                    responder.send(PersistResult::Queued)?
                }
                DataPersistenceRequest::PersistTags { tags, responder } => responder
                    .send(&tags.iter().map(|_| PersistResult::Queued).collect::<Vec<_>>())?,
            }
        }
        Ok(())
    }
}
