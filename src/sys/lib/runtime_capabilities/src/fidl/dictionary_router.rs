// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fidl::router;
use crate::{ConversionError, Dictionary, Router, WeakInstanceToken};
use fidl::AsHandleRef;
use fidl_fuchsia_component_sandbox as fsandbox;
use fidl_fuchsia_io as fio;
use futures::TryStreamExt;
use std::sync::Arc;
use vfs::directory::entry::DirectoryEntry;
use vfs::execution_scope::ExecutionScope;

impl crate::RemotableCapability for Router<Dictionary> {
    fn try_into_directory_entry(
        self,
        scope: ExecutionScope,
        token: WeakInstanceToken,
    ) -> Result<Arc<dyn DirectoryEntry>, ConversionError> {
        Ok(self.into_directory_entry(fio::DirentType::Directory, scope, token))
    }
}

impl crate::fidl::IntoFsandboxCapability for Router<Dictionary> {
    fn into_fsandbox_capability(self, token: WeakInstanceToken) -> fsandbox::Capability {
        let (client_end, sender_stream) =
            fidl::endpoints::create_request_stream::<fsandbox::DictionaryRouterMarker>();
        self.serve_and_register(sender_stream, client_end.as_handle_ref().koid().unwrap(), token);
        fsandbox::Capability::DictionaryRouter(client_end)
    }
}

impl Router<Dictionary> {
    async fn serve_router(
        self,
        mut stream: fsandbox::DictionaryRouterRequestStream,
        token: WeakInstanceToken,
    ) -> Result<(), fidl::Error> {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fsandbox::DictionaryRouterRequest::Route { payload, responder } => {
                    let resp = match router::route_from_fidl(&self, payload, token.clone()).await {
                        Ok(Some(c)) => {
                            Ok(fsandbox::DictionaryRouterRouteResponse::Dictionary(c.into()))
                        }
                        Ok(None) => Ok(fsandbox::DictionaryRouterRouteResponse::Unavailable(
                            fsandbox::Unit {},
                        )),
                        Err(e) => Err(e),
                    };
                    responder.send(resp)?;
                }
                fsandbox::DictionaryRouterRequest::_UnknownMethod { ordinal, .. } => {
                    log::warn!(
                        ordinal:%; "Received unknown DictionaryRouter request"
                    );
                }
            }
        }
        Ok(())
    }

    /// Serves the `fuchsia.sandbox.Router` protocol and moves ourself into the registry.
    pub fn serve_and_register(
        self,
        stream: fsandbox::DictionaryRouterRequestStream,
        koid: zx::Koid,
        token: WeakInstanceToken,
    ) {
        let router = self.clone();

        // Move this capability into the registry.
        crate::fidl::registry::insert(self.into(), koid, async move {
            router.serve_router(stream, token).await.expect("failed to serve Router");
        });
    }
}
