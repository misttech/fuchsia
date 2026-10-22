// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints;
use fidl_fidl_examples_routing_echo::{EchoMarker, EchoRequest};
use fuchsia_component::client;
use fuchsia_component::runtime::{Connector, ConnectorReceiver, Dictionary};
use futures::{StreamExt, TryStreamExt};

// This waits for a single Echo call, and then checks that the client exits afterwards.
async fn send_single_response_and_exit(mut receiver: ConnectorReceiver, response: String) {
    let channel = receiver.next().await.unwrap();
    let server_end = endpoints::ServerEnd::<EchoMarker>::new(channel);
    let event = server_end.into_stream().try_next().await.unwrap().unwrap();
    let EchoRequest::EchoString { value, responder } = event;
    match value {
        Some(_) => responder.send(Some(&response)),
        None => responder.send(None),
    }
    .unwrap();
    let next = receiver.next().await;
    assert!(next.is_none(), "{:#?}", next);
}

async fn create_dictionary_with_receiver(connector: Connector) -> Dictionary {
    let dictionary = Dictionary::new().await;
    dictionary.insert("fidl.examples.routing.echo.Echo", connector).await;
    dictionary
}

async fn create_child_with_specialized_name(
    realm: &fidl_fuchsia_component::RealmProxy,
    name: String,
) {
    // Create our connector.
    let (connector, receiver) = Connector::new().await;

    // Create our dictionary
    let dictionary = create_dictionary_with_receiver(connector).await;
    log::info!("Populated the dictionary");

    // Create our child and supply it with the protocol in the form of a
    // `Connector` in its input dictionary.
    let collection_ref =
        fidl_fuchsia_component_decl::CollectionRef { name: "test_collection".to_string() };

    let args = fidl_fuchsia_component::CreateChildArgs {
        additional_inputs: Some(dictionary.handle),
        ..Default::default()
    };
    let child_decl = fidl_fuchsia_component_decl::Child {
        name: Some(name.clone()),
        url: Some("#meta/dynamic-create-child-child.cm".into()),
        startup: Some(fidl_fuchsia_component_decl::StartupMode::Eager),
        ..Default::default()
    };
    realm.create_child(&collection_ref, &child_decl, args).await.unwrap().unwrap();

    // Now that we created the child, receive our single request from it.
    send_single_response_and_exit(receiver, format!("Hello {}", name)).await;
}

// This test creates a child that will call a single `fuchsia.examples.routing.echo.Echo" function.
// We route the protocol to that child by using CreateChildArgs::additional_inputs.
// We then verify that we receive the one call to echo.
#[fuchsia::test]
async fn create_child() {
    let realm = client::connect_to_protocol::<fidl_fuchsia_component::RealmMarker>().unwrap();

    create_child_with_specialized_name(&realm, "child-a".into()).await;
    create_child_with_specialized_name(&realm, "child-b".into()).await;
}
