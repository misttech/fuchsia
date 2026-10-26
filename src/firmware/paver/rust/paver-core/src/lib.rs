// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod boot_manager;
pub mod data_sink;
pub mod partition;
pub mod partition_io;
pub mod paver;
pub mod sysconfig;

use crate::paver::Paver;
use anyhow::{Context as _, Error};
use fidl_next::{Encode, Respond, RespondErr, Responder, Transport, TwoWayMethod, Wire};
use fidl_next_fuchsia_paver as fpaver;
use fidl_next_fuchsia_process_lifecycle as fprocess_lifecycle;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_runtime::HandleType;
use futures::StreamExt as _;
use std::future::Future;
use std::rc::Rc;

/// Extension trait on [`fidl_next::Responder`] for sending `Result` and response values and
/// logging transport errors.
pub trait ResponderExt<M> {
    /// Sends `Ok` via [`Responder::respond`] or `Err` via [`Responder::respond_err`] for FIDL
    /// methods declared with `error zx.Status` syntax.
    fn send_result<R>(self, result: Result<R, zx::Status>) -> impl Future<Output = ()>
    where
        M: TwoWayMethod + Respond<R> + RespondErr<zx::Status>,
        M::Response: Wire<Constraint = ()>,
        <M as Respond<R>>::Output: Encode<M::Response, <zx::Channel as Transport>::SendBuffer>,
        <M as RespondErr<zx::Status>>::Output:
            Encode<M::Response, <zx::Channel as Transport>::SendBuffer>;

    /// Sends `response` via [`Responder::respond`] (including `Result<(), zx::Status>` for
    /// methods returning `(struct { status zx.Status; })`).
    fn send_response<R>(self, response: R) -> impl Future<Output = ()>
    where
        M: TwoWayMethod + Respond<R>,
        M::Response: Wire<Constraint = ()>,
        <M as Respond<R>>::Output: Encode<M::Response, <zx::Channel as Transport>::SendBuffer>;
}

impl<M> ResponderExt<M> for Responder<M, zx::Channel> {
    async fn send_result<R>(self, result: Result<R, zx::Status>)
    where
        M: TwoWayMethod + Respond<R> + RespondErr<zx::Status>,
        M::Response: Wire<Constraint = ()>,
        <M as Respond<R>>::Output: Encode<M::Response, <zx::Channel as Transport>::SendBuffer>,
        <M as RespondErr<zx::Status>>::Output:
            Encode<M::Response, <zx::Channel as Transport>::SendBuffer>,
    {
        let res = match result {
            Ok(ok) => self.respond(ok).await,
            Err(err) => self.respond_err(err).await,
        };
        if let Err(err) = res {
            log::error!(err:?; "Failed to send FIDL response");
        }
    }

    async fn send_response<R>(self, response: R)
    where
        M: TwoWayMethod + Respond<R>,
        M::Response: Wire<Constraint = ()>,
        <M as Respond<R>>::Output: Encode<M::Response, <zx::Channel as Transport>::SendBuffer>,
    {
        if let Err(err) = self.respond(response).await {
            log::error!(err:?; "Failed to send FIDL response");
        }
    }
}

enum IncomingService {
    Paver(fidl_next::ServerEnd<fpaver::Paver>),
}

/// Runs the paver service loop for `paver_impl`.
pub async fn run<P: Paver>(paver_impl: Rc<P>) -> Result<(), Error> {
    log::info!("paver starting");

    let mut fs = ServiceFs::new_local();
    fs.dir("svc")
        .add_service_at(<fpaver::Paver as fidl_next::Discoverable>::PROTOCOL_NAME, |channel| {
            Some(IncomingService::Paver(fidl_next::ServerEnd::from_untyped(channel)))
        });
    fs.take_and_serve_directory_handle().context("serving outgoing directory")?;

    let lifecycle_handle = fuchsia_runtime::take_startup_handle(HandleType::Lifecycle.into())
        .ok_or_else(|| anyhow::anyhow!("PA_LIFECYCLE startup handle is required"))?;
    let lifecycle_server = fidl_next::ServerEnd::<fprocess_lifecycle::Lifecycle>::from_untyped(
        zx::Channel::from(lifecycle_handle),
    );

    let scope = fasync::Scope::new_with_name("paver");
    let scope_handle = scope.to_handle();
    let paver_clone = paver_impl.clone();
    scope.spawn_local(async move {
        while let Some(IncomingService::Paver(server_end)) = fs.next().await {
            let paver_clone = paver_clone.clone();
            let inner_scope = scope_handle.clone();
            scope_handle.spawn_local(async move {
                if let Err(err) = paver::serve_paver(paver_clone, server_end, inner_scope).await {
                    log::error!(err:?; "Error handling Paver connection");
                }
            });
        }
    });

    paver::serve_lifecycle(paver_impl, lifecycle_server, scope).await
}
