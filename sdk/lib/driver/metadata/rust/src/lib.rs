// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod serde;

use fdf_component::ServiceOffer;
use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Responder, ServerEnd};
use fidl_next_fuchsia_driver_metadata as fmetadata;
use fidl_next_fuchsia_hardware_platform_device as fdevice_next;
use fuchsia_async as fasync;
use fuchsia_component::directory::Directory;
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use std::sync::Arc;

pub use serde::*;

pub struct MetadataServer {
    data: Option<Arc<Vec<u8>>>,
    name: String,
}

impl MetadataServer {
    pub fn new(name: impl Into<String>) -> Self {
        Self { data: None, name: name.into() }
    }

    pub fn with_metadata<T: fidl::Persistable>(self, metadata: &T) -> Result<Self, zx::Status> {
        let data = fidl::persist(metadata).map_err(|_| zx::Status::INTERNAL)?;
        Ok(Self { data: Some(Arc::new(data)), name: self.name })
    }

    pub async fn forward_from_pdev(
        self,
        pdev: &fidl_fuchsia_hardware_platform_device::DeviceProxy,
    ) -> Result<Self, zx::Status> {
        let data = pdev
            .get_metadata(&self.name)
            .await
            .map_err(|_| zx::Status::INTERNAL)?
            .map_err(zx::Status::err_from_raw)?;
        Ok(Self { data: Some(Arc::new(data)), name: self.name })
    }

    pub async fn forward_from_pdev_next(
        self,
        pdev: &fidl_next::Client<fdevice_next::Device>,
    ) -> Result<Self, zx::Status> {
        let data = pdev.get_metadata(&self.name).await.map_err(|_| zx::Status::INTERNAL)??.metadata;
        Ok(Self { data: Some(Arc::new(data)), name: self.name })
    }

    pub async fn forward_from_incoming(
        self,
        incoming: &fdf_component::Incoming,
    ) -> Result<Self, zx::Status> {
        self.forward_from_incoming_instance(incoming, fuchsia_component::DEFAULT_SERVICE_INSTANCE)
            .await
    }

    pub async fn forward_from_incoming_instance(
        self,
        incoming: &fdf_component::Incoming,
        instance_name: &str,
    ) -> Result<Self, zx::Status> {
        let path = format!("/svc/{}/{}/metadata", self.name, instance_name);
        let (client_end, server_end) = fidl_next::fuchsia::create_channel::<fmetadata::Metadata>();
        incoming
            .open(&path, fidl_fuchsia_io::Flags::PROTOCOL_SERVICE, server_end.into_untyped())
            .map_err(|_| zx::Status::NOT_FOUND)?;
        let client = client_end.spawn();
        let data = match client.get_persisted_metadata().await {
            Ok(Ok(resp)) => resp.persisted_metadata,
            Ok(Err(status)) => return Err(status),
            Err(
                fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed)
                | fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(_)),
            ) => {
                return Err(zx::Status::NOT_FOUND);
            }
            Err(_) => return Err(zx::Status::INTERNAL),
        };
        Ok(Self { data: Some(Arc::new(data)), name: self.name })
    }

    pub fn create_handler(&self, scope: fasync::ScopeHandle) -> Option<MetadataHandler> {
        let data = self.data.clone()?;
        Some(MetadataHandler { data, scope })
    }

    pub fn create_service_offer(&self) -> Option<ServiceOffer<fmetadata::Service>> {
        Some(ServiceOffer::<fmetadata::Service>::new_next())
    }

    pub fn serve<O>(
        &self,
        fs: &mut ServiceFs<O>,
        scope: fasync::ScopeHandle,
        instance_name: impl Into<String>,
    ) -> Option<fdf::Offer>
    where
        O: ServiceObjTrait,
    {
        let data = self.data.clone()?;
        let handler = MetadataHandler { data, scope };

        let offer = ServiceOffer::<fmetadata::Service>::new_with_name(self.name.clone())
            .add_default_named_next(fs, instance_name, handler)
            .build_zircon_offer_next();

        Some(offer)
    }

    /// Like [`MetadataServer::serve`], but serves connections on `dispatcher` instead of a
    /// fuchsia-async scope.
    ///
    /// The handler keeps a refcounted [`libasync::AsyncDispatcher`] rather than `dispatcher`
    /// itself, so `dispatcher` doesn't have to be `'static`. Connections that arrive after the
    /// dispatcher shuts down are closed.
    pub fn serve_on<O>(
        &self,
        fs: &mut ServiceFs<O>,
        dispatcher: impl libasync::GetAsyncDispatcher,
        instance_name: impl Into<String>,
    ) -> Option<fdf::Offer>
    where
        O: ServiceObjTrait,
    {
        let data = self.data.clone()?;
        let handler =
            MetadataDispatcherHandler { data, dispatcher: dispatcher.get_async_dispatcher() };

        let offer = ServiceOffer::<fmetadata::Service>::new_with_name(self.name.clone())
            .add_default_named_next(fs, instance_name, handler)
            .build_zircon_offer_next();

        Some(offer)
    }
}

pub struct MetadataHandler {
    data: Arc<Vec<u8>>,
    scope: fasync::ScopeHandle,
}

impl fmetadata::ServiceHandler for MetadataHandler {
    fn metadata(&self, server_end: ServerEnd<fmetadata::Metadata>) {
        let data = self.data.clone();
        self.scope.spawn_local(async move {
            let dispatcher = fidl_next::ServerDispatcher::new(server_end);
            let _ = dispatcher.run_local(MetadataServerImpl { data }).await;
        });
    }
}

struct MetadataDispatcherHandler {
    data: Arc<Vec<u8>>,
    dispatcher: libasync::AsyncDispatcher,
}

impl fmetadata::ServiceHandler for MetadataDispatcherHandler {
    fn metadata(&self, server_end: ServerEnd<fmetadata::Metadata>) {
        let data = self.data.clone();
        libasync_fidl::AsyncChannel::server_from_zx_channel_on_dispatcher(
            server_end,
            self.dispatcher.clone(),
        )
        .spawn(MetadataServerImpl { data });
    }
}

struct MetadataServerImpl {
    data: Arc<Vec<u8>>,
}

impl<T: fidl_next::Transport> fmetadata::MetadataServerHandler<T> for MetadataServerImpl {
    async fn get_persisted_metadata(
        &mut self,
        responder: Responder<fmetadata::metadata::GetPersistedMetadata, T>,
    ) {
        let _ = responder.respond(self.data.as_slice()).await;
    }
}

impl fmetadata::MetadataLocalServerHandler for MetadataServerImpl {
    async fn get_persisted_metadata(
        &mut self,
        responder: Responder<fmetadata::metadata::GetPersistedMetadata>,
    ) {
        let _ = responder.respond(self.data.as_slice()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_driver_metadata::MetadataMarker;
    use fidl_fuchsia_io as fio;
    use futures::StreamExt;
    use libasync::AsAsyncDispatcherRef;
    use libasync_scope_dispatcher::ScopeDispatcher;

    #[fuchsia::test]
    async fn forward_from_incoming_serves_metadata() {
        let scope = fasync::Scope::new();
        let mut parent_fs = ServiceFs::new();
        let expected = fidl_fuchsia_driver_metadata::Dictionary {
            entries: Some(vec![fidl_fuchsia_driver_metadata::DictionaryEntry {
                key: "key".to_string(),
                value: fidl_fuchsia_driver_metadata::DictionaryValue::Int64(123),
            }]),
            ..Default::default()
        };

        let parent_server = MetadataServer::new("test.Metadata")
            .with_metadata(&expected)
            .expect("with_metadata failed");
        parent_server
            .serve(&mut parent_fs, scope.to_handle(), fuchsia_component::DEFAULT_SERVICE_INSTANCE)
            .expect("serve failed");

        let (client, server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_io::DirectoryMarker>();
        parent_fs.serve_connection(server).expect("serve_connection failed");
        scope.spawn_local(parent_fs.collect());

        let incoming = fdf_component::Incoming::from(client);
        let forwarded = MetadataServer::new("test.Metadata")
            .forward_from_incoming(&incoming)
            .await
            .expect("forward_from_incoming failed");

        let mut child_fs = ServiceFs::new();
        forwarded.serve(&mut child_fs, scope.to_handle(), "child").expect("child serve failed");
        let (child_client, child_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_io::DirectoryMarker>();
        child_fs.serve_connection(child_server).expect("child serve_connection failed");
        scope.spawn_local(child_fs.collect());

        let child_incoming = fdf_component::Incoming::from(child_client);
        let (client_end, server_end) = fidl_next::fuchsia::create_channel::<fmetadata::Metadata>();
        child_incoming
            .open(
                "/svc/test.Metadata/child/metadata",
                fidl_fuchsia_io::Flags::PROTOCOL_SERVICE,
                server_end.into_untyped(),
            )
            .expect("open failed");
        let client = client_end.spawn();
        let persisted = client
            .get_persisted_metadata()
            .await
            .expect("FIDL call failed")
            .expect("GetPersistedMetadata failed")
            .persisted_metadata;
        let decoded: fidl_fuchsia_driver_metadata::Dictionary =
            fidl::unpersist(&persisted).expect("unpersist failed");
        assert_eq!(decoded, expected);
    }

    #[fuchsia::test]
    async fn forward_from_incoming_missing_returns_not_found() {
        let scope = fasync::Scope::new();
        let mut parent_fs =
            ServiceFs::<fuchsia_component::server::ServiceObjLocal<'_, ()>>::new_local();
        let (client, server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_io::DirectoryMarker>();
        parent_fs.serve_connection(server).expect("serve_connection failed");
        scope.spawn_local(parent_fs.collect());

        let incoming = fdf_component::Incoming::from(client);
        let result =
            MetadataServer::new("test.MissingMetadata").forward_from_incoming(&incoming).await;
        assert_eq!(result.err(), Some(zx::Status::NOT_FOUND));
    }

    #[fuchsia::test]
    async fn serve_on_borrowed_dispatcher() {
        let dispatcher = ScopeDispatcher::new();
        let server = MetadataServer { data: Some(Arc::new(vec![1, 2, 3])), name: "test".into() };
        let mut fs = ServiceFs::new();
        // A borrowed `AsyncDispatcherRef<'_>` is enough; `serve_on` takes its own refcount.
        assert!(
            server.serve_on(&mut fs, dispatcher.as_async_dispatcher_ref(), "default").is_some()
        );

        let (dir, dir_server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        fs.serve_connection(dir_server).unwrap();
        let _fs = fasync::Task::local(fs.collect::<()>());

        let metadata = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
            MetadataMarker,
        >(&dir, "svc/test/default/metadata")
        .unwrap();
        assert_eq!(metadata.get_persisted_metadata().await.unwrap(), Ok(vec![1, 2, 3]));

        dispatcher.shutdown().await;
    }
}
