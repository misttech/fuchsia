// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::scheduler;
use anyhow::{Error, format_err};
use fidl::endpoints;
use fidl_fuchsia_diagnostics_persist::{
    DataPersistenceMarker, DataPersistenceRequest, DataPersistenceRequestStream, PersistResult,
};
use futures::{StreamExt, TryStreamExt};
use log::*;
use persistence_config::ServiceName;
use std::sync::Arc;
use {fidl_fuchsia_component_sandbox as fsandbox, fuchsia_async as fasync};

pub struct PersistServerData {
    // Service name that this persist server is hosting.
    service_name: ServiceName,
    // Scheduler that will handle the persist requests
    scheduler: scheduler::Scheduler,
}

/// PersistServer handles all requests for a single persistence service.
pub(crate) struct PersistServer;

impl PersistServer {
    /// Spawn a task to handle requests from components through a dynamic dictionary.
    pub fn spawn(
        service_name: ServiceName,
        scheduler: scheduler::Scheduler,
        scope: &fasync::Scope,
        requests: fsandbox::ReceiverRequestStream,
    ) {
        let data = Arc::new(PersistServerData { service_name, scheduler });

        let scope_handle = scope.to_handle();
        scope.spawn(Self::accept_connections(data, requests, scope_handle));
    }

    async fn accept_connections(
        data: Arc<PersistServerData>,
        mut stream: fsandbox::ReceiverRequestStream,
        scope: fasync::ScopeHandle,
    ) {
        while let Some(request) = stream.try_next().await.unwrap() {
            match request {
                fsandbox::ReceiverRequest::Receive { channel, control_handle: _ } => {
                    let data = data.clone();
                    scope.spawn(async move {
                        let server_end =
                            endpoints::ServerEnd::<DataPersistenceMarker>::new(channel);
                        let stream: DataPersistenceRequestStream = server_end.into_stream();
                        if let Err(e) = Self::handle_requests(data, stream).await {
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

    async fn handle_requests(
        data: Arc<PersistServerData>,
        mut stream: DataPersistenceRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) = stream.next().await {
            let request =
                request.map_err(|e| format_err!("error handling persistence request: {e:?}"))?;

            debug!("Received {request:?}");

            match request {
                DataPersistenceRequest::Persist { tag, responder, .. } => {
                    let response = match data.scheduler.schedule(&data.service_name, [tag]).pop() {
                        Some(Ok(())) => PersistResult::Queued,
                        Some(Err(e)) => e.into(),
                        None => {
                            error!("Failed to retrieve a response from scheduler");
                            PersistResult::InternalError
                        }
                    };
                    responder.send(response).map_err(|err| {
                        format_err!("Failed to respond {:?} to client: {}", response, err)
                    })?;
                }
                DataPersistenceRequest::PersistTags { tags, responder, .. } => {
                    let response: Vec<PersistResult> = data
                        .scheduler
                        .schedule(&data.service_name, tags)
                        .into_iter()
                        .map(|res| match res {
                            Ok(()) => PersistResult::Queued,
                            Err(e) => e.into(),
                        })
                        .collect();
                    responder.send(&response).map_err(|err| {
                        format_err!("Failed to respond {:?} to client: {}", response, err)
                    })?;
                }
            }
        }
        Ok(())
    }
}
