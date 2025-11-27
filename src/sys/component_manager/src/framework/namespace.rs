// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::model::component::WeakComponentInstance;
use crate::sandbox_util::take_handle_as_stream;
use cm_types::Path;
use fidl::endpoints;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::future::BoxFuture;
use futures::prelude::*;
use log::warn;
use namespace::NamespaceError;
use sandbox::{Capability, WeakInstanceToken};
use serve_processargs::{BuildNamespaceError, NamespaceBuilder};
use vfs::execution_scope::ExecutionScope;
use {
    fidl_fuchsia_component as fcomponent, fidl_fuchsia_component_sandbox as fsandbox,
    fuchsia_async as fasync,
};

pub fn serve(
    server_end: zx::Channel,
    _target: WeakComponentInstance,
    source: WeakComponentInstance,
) -> BoxFuture<'static, Result<(), anyhow::Error>> {
    async move {
        let namespace_scope = source.upgrade()?.execution_scope.clone();
        let stream = take_handle_as_stream::<fcomponent::NamespaceMarker>(server_end);
        serve_inner(namespace_scope, stream, source.into()).await.map_err(Into::into)
    }
    .boxed()
}

async fn serve_inner(
    namespace_scope: ExecutionScope,
    mut stream: fcomponent::NamespaceRequestStream,
    target: WeakInstanceToken,
) -> Result<(), fidl::Error> {
    let (store, store_stream) =
        endpoints::create_proxy_and_stream::<fsandbox::CapabilityStoreMarker>();
    let target_clone = target.clone();
    let _store_task = fasync::Task::spawn(async move {
        let receiver_scope = fasync::Scope::new();
        let _ = sandbox::serve_capability_store(store_stream, &receiver_scope, target_clone).await;
    });
    while let Some(request) = stream.try_next().await? {
        let method_name = request.method_name();
        let result = handle_request(&namespace_scope, &store, request, target.clone()).await;
        match result {
            // If the error was PEER_CLOSED then we don't need to log it as a client can
            // disconnect while we are processing its request.
            Err(error) if !error.is_closed() => {
                warn!(method_name:%, error:%; "Couldn't send Namespace response");
            }
            _ => {}
        }
    }
    Ok(())
}

async fn handle_request(
    namespace_scope: &ExecutionScope,
    store: &fsandbox::CapabilityStoreProxy,
    request: fcomponent::NamespaceRequest,
    target: WeakInstanceToken,
) -> Result<(), fidl::Error> {
    match request {
        fcomponent::NamespaceRequest::Create { entries, responder } => {
            let res = create(namespace_scope, store, entries, target).await;
            responder.send(res)?;
        }
        fcomponent::NamespaceRequest::_UnknownMethod { ordinal, .. } => {
            warn!(ordinal:%; "fuchsia.component/Namespace received unknown method");
        }
    }
    Ok(())
}

async fn create(
    namespace_scope: &ExecutionScope,
    store: &fsandbox::CapabilityStoreProxy,
    entries: Vec<fcomponent::NamespaceInputEntry>,
    target: WeakInstanceToken,
) -> Result<Vec<fcomponent::NamespaceEntry>, fcomponent::NamespaceError> {
    let mut namespace_builder =
        NamespaceBuilder::new(namespace_scope.clone(), ignore_not_found(), target);
    for entry in entries {
        const ERR: fcomponent::NamespaceError = fcomponent::NamespaceError::DictionaryRead;

        // This API accepts legacy [Dictionary] channel. Round-trip through the import/export
        // CapabilityStore API to convert the channel to a local Dict object that we can
        // enumerate.
        let path = entry.path;
        let dict_id = 1;
        store
            .dictionary_legacy_import(dict_id, entry.dictionary.into())
            .await
            .map_err(|_| ERR)?
            .map_err(|_| ERR)?;
        let dict = store.export(dict_id).await.map_err(|_| ERR)?.map_err(|_| ERR)?;
        let dict = Capability::try_from(dict).map_err(|_| ERR)?;
        let Capability::Dictionary(dict) = dict else {
            return Err(ERR);
        };
        for (key, capability) in dict.enumerate() {
            let capability = capability.map_err(|_| fcomponent::NamespaceError::Conversion)?;
            let path = Path::new(format!("{}/{}", path, key))
                .map_err(|_| fcomponent::NamespaceError::BadEntry)?;
            namespace_builder.add_object(capability, &path).map_err(error_to_fidl)?;
        }
    }
    let namespace = namespace_builder.serve().map_err(error_to_fidl)?;
    let out = namespace.flatten().into_iter().map(Into::into).collect();
    Ok(out)
}

fn error_to_fidl(e: BuildNamespaceError) -> fcomponent::NamespaceError {
    match e {
        BuildNamespaceError::NamespaceError(e) => match e {
            NamespaceError::Shadow(_) => fcomponent::NamespaceError::Shadow,
            NamespaceError::Duplicate(_) => fcomponent::NamespaceError::Duplicate,
            NamespaceError::EntryError(_) => fcomponent::NamespaceError::BadEntry,
        },
        BuildNamespaceError::Conversion { .. } | BuildNamespaceError::Serve { .. } => {
            fcomponent::NamespaceError::Conversion
        }
    }
}

fn ignore_not_found() -> UnboundedSender<String> {
    let (sender, _receiver) = unbounded();
    sender
}

#[cfg(all(test, not(feature = "src_model_tests")))]
mod tests {
    use super::*;
    use crate::model::component::ComponentInstance;
    use crate::model::context::ModelContext;
    use ::routing::bedrock::structured_dict::ComponentInput;
    use ::routing::component_instance::ComponentInstanceInterface;
    use assert_matches::assert_matches;
    use fidl::endpoints::{ProtocolMarker, Proxy, ServerEnd};
    use fuchsia_component::client;
    use futures::TryStreamExt;
    use sandbox::fidl::IntoFsandboxCapability;
    use sandbox::{Connector, Dict};
    use std::sync::{Arc, Weak};
    use {
        fidl_fidl_examples_routing_echo as fecho, fidl_fuchsia_component_sandbox as fsandbox,
        fuchsia_async as fasync,
    };

    async fn handle_echo_request_stream(response: &str, mut stream: fecho::EchoRequestStream) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fecho::EchoRequest::EchoString { value: _, responder } => {
                    responder.send(Some(response)).unwrap();
                }
            }
        }
    }

    async fn new_root() -> Arc<ComponentInstance> {
        ComponentInstance::new_root(
            ComponentInput::default(),
            Arc::new(ModelContext::new_for_test()),
            Weak::new(),
            "test:///root".parse().unwrap(),
        )
        .await
    }

    async fn namespace(
        instance: &Arc<ComponentInstance>,
    ) -> (fcomponent::NamespaceProxy, fasync::Task<()>) {
        let (proxy, server) = endpoints::create_proxy::<fcomponent::NamespaceMarker>();
        let weak_instance = instance.as_weak();
        let task = fasync::Task::spawn(async move {
            serve(server.into_channel(), weak_instance.clone(), weak_instance).await.unwrap();
        });
        (proxy, task)
    }

    #[fuchsia::test]
    async fn namespace_create() {
        let mut tasks = fasync::TaskGroup::new();
        let root = new_root().await;
        let (namespace_proxy, _task) = namespace(&root).await;

        let (store, stream) =
            endpoints::create_proxy_and_stream::<fsandbox::CapabilityStoreMarker>();
        let root_token = root.as_weak().into();
        tasks.spawn(async move {
            let receiver_scope = fasync::Scope::new();
            sandbox::serve_capability_store(stream, &receiver_scope, root_token).await.unwrap()
        });

        let mut namespace_pairs = vec![];
        let mut next_id = 1;
        for (path, response) in [("/svc", "first"), ("/zzz/svc", "second")] {
            // Initialize the host and sender/receiver pair.
            let (receiver, sender) = Connector::new();

            // Serve an Echo request handler on the Receiver.
            tasks.spawn(async move {
                loop {
                    let msg = receiver.receive().await.unwrap();
                    let stream: fecho::EchoRequestStream =
                        ServerEnd::<fecho::EchoMarker>::from(msg.channel).into_stream();
                    handle_echo_request_stream(response, stream).await;
                }
            });

            // Create a dictionary and add the Sender to it.
            let dict = Dict::new();
            dict.insert(
                fecho::EchoMarker::DEBUG_NAME.parse().unwrap(),
                Capability::Connector(sender),
            )
            .expect("dict entry already exists");

            let dict_id = next_id;
            next_id += 1;
            store
                .import(
                    dict_id,
                    Capability::from(dict).into_fsandbox_capability(root.as_weak().into()),
                )
                .await
                .unwrap()
                .unwrap();
            let (client_end, server_end) = fidl::Channel::create();
            store.dictionary_legacy_export(dict_id, server_end).await.unwrap().unwrap();

            namespace_pairs.push(fcomponent::NamespaceInputEntry {
                path: path.into(),
                dictionary: client_end.into(),
            })
        }

        // Convert the dictionaries to a namespace.
        let mut namespace_entries = namespace_proxy.create(namespace_pairs).await.unwrap().unwrap();

        // Confirm that the Sender in the dictionary was converted to a service node, and we
        // can access the Echo protocol (served by the Receiver) through this node.
        let entry = namespace_entries.remove(0);
        assert_matches!(entry.path, Some(p) if p == "/svc");
        let dir = entry.directory.unwrap().into_proxy();
        let echo = client::connect_to_protocol_at_dir_root::<fecho::EchoMarker>(&dir).unwrap();
        let response = echo.echo_string(None).await.unwrap();
        assert_matches!(response, Some(m) if m == "first");

        let entry = namespace_entries.remove(0);
        assert!(namespace_entries.is_empty());
        assert_matches!(entry.path, Some(p) if p == "/zzz/svc");
        let dir = entry.directory.unwrap().into_proxy();
        let echo = client::connect_to_protocol_at_dir_root::<fecho::EchoMarker>(&dir).unwrap();
        let response = echo.echo_string(None).await.unwrap();
        assert_matches!(response, Some(m) if m == "second");
    }

    #[fuchsia::test]
    async fn namespace_create_err_shadow() {
        let mut tasks = fasync::TaskGroup::new();
        let root = new_root().await;
        let (namespace_proxy, _task) = namespace(&root).await;

        let (store, stream) =
            endpoints::create_proxy_and_stream::<fsandbox::CapabilityStoreMarker>();
        let root_token = root.as_weak().into();
        tasks.spawn(async move {
            let receiver_scope = fasync::Scope::new();
            sandbox::serve_capability_store(stream, &receiver_scope, root_token).await.unwrap()
        });

        // Two entries with a shadowing path.
        let mut namespace_pairs = vec![];
        let mut next_id = 1;
        for path in ["/svc", "/svc/shadow"] {
            // Initialize the host and sender/receiver pair.
            let (receiver, sender) = Connector::new();

            // Serve an Echo request handler on the Receiver.
            tasks.spawn(async move {
                while let Some(msg) = receiver.receive().await {
                    let stream: fecho::EchoRequestStream =
                        ServerEnd::<fecho::EchoMarker>::from(msg.channel).into_stream();
                    handle_echo_request_stream("hello", stream).await;
                }
            });

            // Create a dictionary and add the Sender to it.
            let dict = Dict::new();
            dict.insert(
                fecho::EchoMarker::DEBUG_NAME.parse().unwrap(),
                Capability::Connector(sender),
            )
            .expect("dict entry already exists");

            let dict_id = next_id;
            next_id += 1;
            store
                .import(
                    dict_id,
                    Capability::from(dict).into_fsandbox_capability(root.as_weak().into()),
                )
                .await
                .unwrap()
                .unwrap();
            let (client_end, server_end) = fidl::Channel::create();
            store.dictionary_legacy_export(dict_id, server_end).await.unwrap().unwrap();

            namespace_pairs.push(fcomponent::NamespaceInputEntry {
                path: path.into(),
                dictionary: client_end.into(),
            })
        }

        // Try to convert the dictionaries to a namespace. Expect an error because one path
        // shadows another.
        let res = namespace_proxy.create(namespace_pairs).await.unwrap();
        assert_matches!(res, Err(fcomponent::NamespaceError::Shadow));
    }

    #[fuchsia::test]
    async fn namespace_create_err_dict_read() {
        let root = new_root().await;
        let (namespace_proxy, _task) = namespace(&root).await;

        // Create a dictionary and close the server end.
        let (dict_proxy, stream) =
            endpoints::create_proxy_and_stream::<fsandbox::DictionaryMarker>();
        drop(stream);
        let namespace_pairs = vec![fcomponent::NamespaceInputEntry {
            path: "/svc".into(),
            dictionary: dict_proxy.into_channel().unwrap().into_zx_channel().into(),
        }];

        // Try to convert the dictionaries to a namespace. Expect an error because the dictionary
        // was unreadable.
        let res = namespace_proxy.create(namespace_pairs).await.unwrap();
        assert_matches!(res, Err(fcomponent::NamespaceError::DictionaryRead));
    }
}
