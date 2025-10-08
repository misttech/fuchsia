// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::capability;
use crate::framework::capability_store::CapabilityStore;
use crate::model::routing::Route;
use crate::model::testing::out_dir::OutDir;
use crate::model::testing::routing_test_helpers::RoutingTestBuilder;
use ::routing::capability_source::{CapabilitySource, InternalCapability, VoidSource};
use ::routing::{RouteRequest, RouteSource};
use ::routing_test_helpers::dictionary::CommonDictionaryTest;
use ::routing_test_helpers::{CheckUse, ExpectedResult};
use assert_matches::assert_matches;
use cm_rust::*;
use cm_rust_testing::*;
use cm_types::RelativePath;
use fidl::endpoints::{self, ServerEnd};
use fidl_fidl_examples_routing_echo::EchoMarker;
use futures::TryStreamExt;
use moniker::Moniker;
use routing_test_helpers::RoutingTestModel;
use zx_status::Status;
use {fidl_fuchsia_component_sandbox as fsandbox, fuchsia_async as fasync};

#[fuchsia::test]
async fn use_protocol_from_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_use_protocol_from_dictionary().await
}

#[fuchsia::test]
async fn use_protocol_from_dictionary_not_a_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_use_protocol_from_dictionary_not_a_dictionary()
        .await
}

#[fuchsia::test]
async fn use_protocol_from_dictionary_not_used() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_use_protocol_from_dictionary_not_used()
        .await
}

#[fuchsia::test]
async fn use_protocol_from_dictionary_not_found() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_use_protocol_from_dictionary_not_found()
        .await
}

#[fuchsia::test]
async fn use_directory_from_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_use_directory_from_dictionary().await
}

#[fuchsia::test]
async fn expose_directory_from_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_expose_directory_from_dictionary().await
}

#[fuchsia::test]
async fn use_protocol_from_nested_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_use_protocol_from_nested_dictionary()
        .await
}

#[fuchsia::test]
async fn offer_protocol_from_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_offer_protocol_from_dictionary().await
}

#[fuchsia::test]
async fn offer_protocol_from_dictionary_not_found() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_offer_protocol_from_dictionary_not_found()
        .await
}

#[fuchsia::test]
async fn offer_protocol_from_dictionary_to_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_offer_protocol_from_dictionary_to_dictionary()
        .await
}

#[fuchsia::test]
async fn offer_protocol_from_nested_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_offer_protocol_from_nested_dictionary()
        .await
}

#[fuchsia::test]
async fn expose_protocol_from_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_expose_protocol_from_dictionary().await
}

#[fuchsia::test]
async fn expose_protocol_from_dictionary_not_found() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_expose_protocol_from_dictionary_not_found()
        .await
}

#[fuchsia::test]
async fn expose_protocol_from_nested_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_expose_protocol_from_nested_dictionary()
        .await
}

#[fuchsia::test]
async fn dictionary_in_exposed_dir() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_dictionary_in_exposed_dir().await
}

#[fuchsia::test]
async fn offer_dictionary_to_dictionary() {
    CommonDictionaryTest::<RoutingTestBuilder>::new().test_offer_dictionary_to_dictionary().await
}

#[fuchsia::test]
async fn use_from_dictionary_availability_invalid() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_use_from_dictionary_availability_invalid()
        .await
}

#[fuchsia::test]
async fn offer_from_dictionary_availability_invalid() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_offer_from_dictionary_availability_invalid()
        .await
}

#[fuchsia::test]
async fn expose_from_dictionary_availability_attenuated() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_expose_from_dictionary_availability_attenuated()
        .await
}

#[fuchsia::test]
async fn expose_from_dictionary_availability_invalid() {
    CommonDictionaryTest::<RoutingTestBuilder>::new()
        .test_expose_from_dictionary_availability_invalid()
        .await
}

#[fuchsia::test]
async fn use_from_void_dictionary() {
    let use_decl = UseBuilder::protocol()
        .name("A")
        .from_dictionary("dict")
        .availability(Availability::Optional)
        .build();
    let components = vec![
        (
            "root",
            ComponentDeclBuilder::new()
                .offer(
                    OfferBuilder::dictionary()
                        .name("dict")
                        .source(OfferSource::Void)
                        .target_static_child("leaf")
                        .availability(Availability::Optional),
                )
                .child_default("leaf")
                .build(),
        ),
        ("leaf", ComponentDeclBuilder::new().use_(use_decl.clone()).build()),
    ];

    let test = RoutingTestBuilder::new("root", components).build().await;
    let leaf = test.model.root().find_and_maybe_resolve(&"leaf".parse().unwrap()).await.unwrap();

    let cm_rust::UseDecl::Protocol(use_decl) = use_decl else {
        unreachable!();
    };
    let res = RouteRequest::UseProtocol(use_decl).route(&leaf).await;
    assert_matches!(
        res,
        Ok(RouteSource {
            source: CapabilitySource::Void(VoidSource {
                moniker,
                capability: InternalCapability::Dictionary(name)
            }),
            relative_path
        }) if moniker == Moniker::root() && name == "dict" &&
              relative_path == RelativePath::dot()
    );
}

#[fuchsia::test]
async fn use_from_void_nested_dictionary() {
    let use_decl = UseBuilder::protocol()
        .name("A")
        .from_dictionary("outer/inner")
        .availability(Availability::Optional)
        .build();
    let components = vec![
        (
            "root",
            ComponentDeclBuilder::new()
                .dictionary_default("outer")
                .offer(
                    OfferBuilder::dictionary()
                        .name("outer")
                        .source(OfferSource::Self_)
                        .target_static_child("leaf")
                        .availability(Availability::Optional),
                )
                .offer(
                    OfferBuilder::dictionary()
                        .name("inner")
                        .source_static_child("child")
                        .target(OfferTarget::Capability("outer".parse().unwrap()))
                        .availability(Availability::Optional),
                )
                .child_default("leaf")
                .child_default("child")
                .build(),
        ),
        (
            "child",
            ComponentDeclBuilder::new()
                .expose(
                    ExposeBuilder::dictionary()
                        .name("inner")
                        .source(ExposeSource::Void)
                        .availability(Availability::Optional),
                )
                .build(),
        ),
        ("leaf", ComponentDeclBuilder::new().use_(use_decl.clone()).build()),
    ];

    let test = RoutingTestBuilder::new("root", components).build().await;
    let leaf = test.model.root().find_and_maybe_resolve(&"leaf".parse().unwrap()).await.unwrap();

    let cm_rust::UseDecl::Protocol(use_decl) = use_decl else {
        unreachable!();
    };
    let res = RouteRequest::UseProtocol(use_decl).route(&leaf).await;
    assert_matches!(
        res,
        Ok(RouteSource {
            source: CapabilitySource::Void(VoidSource {
                moniker,
                capability: InternalCapability::Dictionary(name)
            }),
            relative_path
        }) if moniker == "child".parse().unwrap() && name == "inner" &&
              relative_path == RelativePath::dot()
    );
}

#[fuchsia::test]
async fn test_dictionary_from_program() {
    // Tests a dictionary that is backed by the program.

    const ROUTER_PATH: &str = "/svc/fuchsia.component.sandbox.DictionaryRouter";
    let components = vec![
        (
            "root",
            ComponentDeclBuilder::new()
                .capability(CapabilityBuilder::dictionary().name("dict").path(ROUTER_PATH))
                .offer(
                    OfferBuilder::dictionary()
                        .name("dict")
                        .source(OfferSource::Self_)
                        .target_static_child("leaf"),
                )
                .child_default("leaf")
                .build(),
        ),
        (
            "leaf",
            ComponentDeclBuilder::new()
                .use_(UseBuilder::protocol().name("A").from_dictionary("dict"))
                .build(),
        ),
    ];
    let test = RoutingTestBuilder::new("root", components).build().await;

    let host = CapabilityStore::new();
    let (store, server) = endpoints::create_proxy::<fsandbox::CapabilityStoreMarker>();
    capability::open_framework(&host, test.model.root(), server.into()).await.unwrap();

    // Create a dictionary with a Sender at "A" for the Echo protocol.
    let dict_id = 1;
    store.dictionary_create(dict_id).await.unwrap().unwrap();
    let (receiver_client, mut receiver_stream) =
        endpoints::create_request_stream::<fsandbox::ReceiverMarker>();
    let connector_id = 10;
    store.connector_create(connector_id, receiver_client).await.unwrap().unwrap();
    store
        .dictionary_insert(
            dict_id,
            &fsandbox::DictionaryItem { key: "A".into(), value: connector_id },
        )
        .await
        .unwrap()
        .unwrap();

    // Serve the Echo protocol from the Receiver.
    let _receiver_task = fasync::Task::spawn(async move {
        let mut task_group = fasync::TaskGroup::new();
        while let Ok(Some(request)) = receiver_stream.try_next().await {
            match request {
                fsandbox::ReceiverRequest::Receive { channel, control_handle: _ } => {
                    let channel: ServerEnd<EchoMarker> = channel.into();
                    task_group.spawn(OutDir::echo_protocol_fn(channel.into_stream()));
                }
                fsandbox::ReceiverRequest::_UnknownMethod { .. } => {
                    unimplemented!()
                }
            }
        }
    });

    // Serve the Router protocol from the root's out dir. Its implementation calls Dictionary/Clone
    // and returns the handle.
    let mut root_out_dir = OutDir::new();
    let dict_store2 = store.clone();
    root_out_dir.add_entry(
        ROUTER_PATH.parse().unwrap(),
        vfs::service::endpoint(move |scope, channel| {
            let server_end: ServerEnd<fsandbox::DictionaryRouterMarker> =
                channel.into_zx_channel().into();
            let mut stream = server_end.into_stream();
            let store = dict_store2.clone();
            scope.spawn(async move {
                while let Ok(Some(request)) = stream.try_next().await {
                    match request {
                        fsandbox::DictionaryRouterRequest::Route { payload: _, responder } => {
                            let dup_dict_id = dict_id + 1;
                            store.duplicate(dict_id, dup_dict_id).await.unwrap().unwrap();
                            let capability = store.export(dup_dict_id).await.unwrap().unwrap();
                            let fsandbox::Capability::Dictionary(dict) = capability else {
                                panic!("capability was not a dictionary? {capability:?}");
                            };
                            let _ = responder.send(Ok(
                                fsandbox::DictionaryRouterRouteResponse::Dictionary(dict),
                            ));
                        }
                        fsandbox::DictionaryRouterRequest::_UnknownMethod { .. } => {
                            unimplemented!()
                        }
                    }
                }
            });
        }),
    );
    test.mock_runner.add_host_fn("test:///root", root_out_dir.host_fn());

    // Using "A" from the dictionary should succeed.
    for _ in 0..3 {
        test.check_use(
            "leaf".try_into().unwrap(),
            CheckUse::Protocol {
                path: "/svc/A".parse().unwrap(),
                expected_res: ExpectedResult::Ok,
            },
        )
        .await;
    }

    // Now, remove "A" from the dictionary. Using "A" this time should fail.
    let dest_id = 100;
    store
        .dictionary_remove(dict_id, "A", Some(&fsandbox::WrappedNewCapabilityId { id: dest_id }))
        .await
        .unwrap()
        .unwrap();
    test.check_use(
        "leaf".try_into().unwrap(),
        CheckUse::Protocol {
            path: "/svc/A".parse().unwrap(),
            expected_res: ExpectedResult::Err(Status::NOT_FOUND),
        },
    )
    .await;
}
