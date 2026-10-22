// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_test_manager as ftest_manager;
use ftest_manager::{RealmOptions, RunSuiteOptions, SuiteResult, TestCaseResult};
use fuchsia_component::client;
use futures::prelude::*;
use pretty_assertions::assert_eq;
use test_case::test_case;
use test_manager_test_lib::{
    AttributedLog, GroupRunEventByTestCase, RunEvent, SuiteRunner,
    collect_string_from_socket_helper, collect_suite_events_with_watch, default_run_suite_options,
};

const ECHO_TEST_COL: &str = "echo_test_coll";
const HERMETIC_TEST_COL: &str = "hermetic_test_coll";

macro_rules! connect_suite_runner {
    () => {
        client::connect_to_protocol::<ftest_manager::SuiteRunnerMarker>()
            .context("cannot connect to suite runner proxy")
    };
}

macro_rules! connect_test_case_enumerator_server {
    () => {
        client::connect_to_protocol::<ftest_manager::TestCaseEnumeratorMarker>()
            .context("cannot connect to test case enumerator proxy")
    };
}

fn connect_realm() -> Result<ClientEnd<fcomponent::RealmMarker>, Error> {
    let (client_end, server_end) = fidl::endpoints::create_endpoints::<fcomponent::RealmMarker>();
    client::connect_channel_to_protocol::<fcomponent::RealmMarker>(server_end.into_channel())
        .context("could not connect to Realm service")?;
    Ok(client_end)
}

fn default_offers() -> Vec<fdecl::Offer> {
    vec![
        fdecl::Offer::Storage(fdecl::OfferStorage {
            source_name: Some("data".to_string()),
            source: Some(fdecl::Ref::Parent(fdecl::ParentRef {})),
            target: Some(fdecl::Ref::Self_(fdecl::SelfRef {})),
            target_name: Some("data".to_string()),
            ..Default::default()
        }),
        fdecl::Offer::EventStream(fdecl::OfferEventStream {
            target_name: Some("capability_requested".to_string()),
            ..Default::default()
        }),
    ]
}

async fn run_test_in_echo_test_realm(
    test_url: &str,
    run_options: RunSuiteOptions,
) -> Result<(Vec<RunEvent>, Vec<AttributedLog>), Error> {
    let realm = connect_realm().unwrap();
    let mut offers = default_offers();
    offers.push(fdecl::Offer::Protocol(fdecl::OfferProtocol {
        source_name: Some("fidl.examples.routing.echo.Echo".into()),
        target_name: Some("fidl.examples.routing.echo.Echo".into()),
        source: None,
        target: None,
        dependency_type: None,
        ..Default::default()
    }));
    run_single_test(realm, &offers, ECHO_TEST_COL, test_url, run_options).await
}

async fn run_test_in_hermetic_test_realm(
    test_url: &str,
    run_options: RunSuiteOptions,
) -> Result<(Vec<RunEvent>, Vec<AttributedLog>), Error> {
    let realm = connect_realm().unwrap();
    let offers = default_offers();
    run_single_test(realm, &offers, HERMETIC_TEST_COL, test_url, run_options).await
}

async fn run_single_test(
    realm: ClientEnd<fcomponent::RealmMarker>,
    offers: &[fdecl::Offer],
    test_collection: &str,
    test_url: &str,
    mut run_options: RunSuiteOptions,
) -> Result<(Vec<RunEvent>, Vec<AttributedLog>), Error> {
    let runner = SuiteRunner::new(connect_suite_runner!()?);
    run_options.realm_options = Some(RealmOptions {
        realm: Some(realm),
        offers: Some(offers.to_vec()),
        test_collection: Some(test_collection.into()),
        ..Default::default()
    });
    let suite_instance =
        runner.start_suite_run(test_url, run_options).context("Cannot create suite instance")?;
    collect_suite_events_with_watch(suite_instance, true, true).await
}

#[fuchsia::test]
async fn launch_and_test_echo_test() {
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/echo_test_client.cm";
    let (events, logs) =
        run_test_in_echo_test_realm(test_url, default_run_suite_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("EchoTest"),
        RunEvent::case_started("EchoTest"),
        RunEvent::case_stopped("EchoTest", TestCaseResult::Passed),
        RunEvent::case_finished("EchoTest"),
        RunEvent::suite_stopped(SuiteResult::Finished),
    ];

    assert_eq!(logs, Vec::new());
    assert_eq!(&expected_events, &events);
}

#[fuchsia::test]
async fn enumerate_echo_test() {
    let proxy = connect_test_case_enumerator_server!().unwrap();
    let realm = connect_realm().unwrap();
    let mut offers = default_offers();
    offers.push(fdecl::Offer::Protocol(fdecl::OfferProtocol {
        source_name: Some("fidl.examples.routing.echo.Echo".into()),
        target_name: Some("fidl.examples.routing.echo.Echo".into()),
        source: None,
        target: None,
        dependency_type: None,
        ..Default::default()
    }));

    let (iterator, server_end) = fidl::endpoints::create_proxy();

    proxy
        .enumerate(
            "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/echo_test_realm.cm",
            ftest_manager::EnumerateTestCasesOptions {
                realm_options: Some(RealmOptions {
                    realm: Some(realm),
                    offers: Some(offers),
                    test_collection: Some(ECHO_TEST_COL.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_end,
        )
        .await
        .unwrap()
        .expect("This should not fail");

    let mut cases = vec![];
    loop {
        let mut c = iterator.get_next().await.unwrap();
        if c.is_empty() {
            break;
        }
        cases.append(&mut c);
    }
    assert_eq!(
        cases.into_iter().map(|c| c.name.unwrap()).collect::<Vec<_>>(),
        vec!["EchoTest".to_string()]
    );
}

#[fuchsia::test]
async fn launch_and_test_echo_test_in_hermetic_realm() {
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/echo_test_client.cm";
    let (events, _logs) =
        run_test_in_hermetic_test_realm(test_url, default_run_suite_options()).await.unwrap();

    // this will fail because the hermetic realm does not have access to echo service.
    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("EchoTest"),
        RunEvent::case_started("EchoTest"),
        RunEvent::case_stopped("EchoTest", TestCaseResult::Failed),
        RunEvent::case_finished("EchoTest"),
        RunEvent::suite_stopped(SuiteResult::Failed),
    ];

    //assert_eq!(logs, Vec::<String>::new());
    assert_eq!(&expected_events, &events);
}

#[fuchsia::test]
async fn launch_and_test_hermetic_echo_test_in_hermetic_realm() {
    // This test does not depend on system echo service so should pass in hermetic realm.
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/echo_test_realm.cm";
    let (events, logs) =
        run_test_in_hermetic_test_realm(test_url, default_run_suite_options()).await.unwrap();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("EchoTest"),
        RunEvent::case_started("EchoTest"),
        RunEvent::case_stopped("EchoTest", TestCaseResult::Passed),
        RunEvent::case_finished("EchoTest"),
        RunEvent::suite_stopped(SuiteResult::Finished),
    ];

    assert_eq!(logs, Vec::new());
    assert_eq!(&expected_events, &events);
}

#[fuchsia::test]
async fn collect_isolated_logs_using_default_log_iterator() {
    let test_url = "fuchsia-pkg://fuchsia.com/test-manager-diagnostics-tests#meta/test-root.cm";
    let (_events, logs) =
        run_test_in_hermetic_test_realm(test_url, default_run_suite_options()).await.unwrap();

    assert_eq!(
        logs.iter().map(|attributed| attributed.log.as_ref()).collect::<Vec<&str>>(),
        vec!["Started diagnostics publisher", "Finishing through Stop"],
        "{logs:#?}",
    );
}

#[fuchsia::test]
async fn collect_isolated_logs_using_batch() {
    let test_url = "fuchsia-pkg://fuchsia.com/test-manager-diagnostics-tests#meta/test-root.cm";
    let mut options = default_run_suite_options();
    options.logs_iterator_type = Some(ftest_manager::LogsIteratorType::Batch);
    let (_events, logs) = run_test_in_hermetic_test_realm(test_url, options).await.unwrap();

    assert_eq!(
        logs.iter().map(|attributed| attributed.log.as_ref()).collect::<Vec<&str>>(),
        vec!["Started diagnostics publisher", "Finishing through Stop"],
        "{logs:#?}",
    );
}

#[fuchsia::test]
async fn collect_isolated_logs_using_archive_iterator() {
    let test_url = "fuchsia-pkg://fuchsia.com/test-manager-diagnostics-tests#meta/test-root.cm";
    let options = RunSuiteOptions {
        logs_iterator_type: Some(ftest_manager::LogsIteratorType::Socket),
        ..default_run_suite_options()
    };
    let (_events, logs) = run_test_in_hermetic_test_realm(test_url, options).await.unwrap();

    assert_eq!(
        logs.iter().map(|attributed| attributed.log.as_ref()).collect::<Vec<&str>>(),
        vec!["Started diagnostics publisher", "Finishing through Stop"],
        "{logs:#?}",
    );
}

#[fuchsia::test]
async fn update_log_severity_for_all_components() {
    let test_url = "fuchsia-pkg://fuchsia.com/test-manager-diagnostics-tests#meta/test-root.cm";
    let options = RunSuiteOptions {
        logs_iterator_type: Some(ftest_manager::LogsIteratorType::Socket),
        log_interest: Some(vec![
            selectors::parse_log_interest_selector_or_severity("DEBUG").unwrap(),
        ]),
        ..default_run_suite_options()
    };
    let (_events, logs) = run_test_in_hermetic_test_realm(test_url, options).await.unwrap();
    assert_eq!(
        logs.iter().map(|attributed| attributed.log.as_ref()).collect::<Vec<&str>>(),
        vec![
            "I'm a debug log from a test",
            "Started diagnostics publisher",
            "I'm a debug log from the publisher!",
            "Finishing through Stop",
        ],
        "{logs:#?}",
    );
}

#[test_case(true; "compressed debug_data")]
#[fuchsia::test]
async fn debug_data_test(compressed: bool) {
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/debug_data_write_test.cm";

    let runner = SuiteRunner::new(connect_suite_runner!().unwrap());
    let realm = connect_realm().unwrap();
    let mut options = default_run_suite_options();
    options.realm_options = Some(RealmOptions {
        realm: Some(realm),
        offers: Some(default_offers()),
        test_collection: Some(HERMETIC_TEST_COL.into()),
        ..Default::default()
    });
    let suite_instance =
        runner.start_suite_run(test_url, options).expect("Cannot create suite instance");
    let (suite_events, _) =
        collect_suite_events_with_watch(suite_instance, false, compressed).await.unwrap();
    let (debug_data_events, suite_events): (Vec<_>, Vec<_>) =
        suite_events.into_iter().partition(|e| matches!(e, RunEvent::DebugData { .. }));
    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("publish_debug_data"),
        RunEvent::case_started("publish_debug_data"),
        RunEvent::case_stopped("publish_debug_data", TestCaseResult::Passed),
        RunEvent::case_finished("publish_debug_data"),
        RunEvent::suite_stopped(SuiteResult::Finished),
    ];

    assert_eq!(
        suite_events.into_iter().group_by_test_case_unordered(),
        expected_events.into_iter().group_by_test_case_unordered(),
    );

    let num_debug_data_events = stream::iter(debug_data_events)
        .then(|run_event| async move {
            let RunEvent::DebugData { filename, socket } = run_event else {
                return false;
            };
            filename.starts_with("data_sink/data_sink.")
                && collect_string_from_socket_helper(socket, compressed).await.unwrap()
                    == "Debug data from test\n"
        })
        .filter(|matches_vmo| futures::future::ready(*matches_vmo))
        .count()
        .await;
    assert_eq!(num_debug_data_events, 1);
}

#[fuchsia::test]
async fn debug_data_accumulate_test() {
    let compressed_debug_data = true;
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/debug_data_write_test.cm";

    // If I run the same test again, also accumulating debug_data, I should see two files
    for iteration in 1usize..3 {
        let runner = SuiteRunner::new(connect_suite_runner!().unwrap());
        let realm = connect_realm().unwrap();
        let options = RunSuiteOptions {
            accumulate_debug_data: Some(true),
            realm_options: Some(RealmOptions {
                realm: Some(realm),
                offers: Some(default_offers()),
                test_collection: Some(HERMETIC_TEST_COL.into()),
                ..Default::default()
            }),
            ..default_run_suite_options()
        };
        let suite_instance =
            runner.start_suite_run(test_url, options).expect("Cannot create suite instance");
        let (suite_events, _) =
            collect_suite_events_with_watch(suite_instance, false, compressed_debug_data)
                .await
                .unwrap();

        let num_debug_data_events = stream::iter(suite_events)
            .then(|run_event| async move {
                let RunEvent::DebugData { filename, socket } = run_event else {
                    return false;
                };
                filename.starts_with("data_sink/data_sink.")
                    && collect_string_from_socket_helper(socket, compressed_debug_data)
                        .await
                        .unwrap()
                        == "Debug data from test\n"
            })
            .filter(|matches_vmo| futures::future::ready(*matches_vmo))
            .count()
            .await;
        assert_eq!(num_debug_data_events, iteration);
    }
}

#[test_case(true; "compressed debug_data")]
#[fuchsia::test]
async fn debug_data_isolated_test(compressed: bool) {
    let test_url =
        "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/debug_data_write_test.cm";
    // By default, when I run the same test twice, debug data is not accumulated.
    for _ in 0..2 {
        let runner = SuiteRunner::new(connect_suite_runner!().unwrap());
        let realm = connect_realm().unwrap();
        let mut options = default_run_suite_options();
        options.realm_options = Some(RealmOptions {
            realm: Some(realm),
            offers: Some(default_offers()),
            test_collection: Some(HERMETIC_TEST_COL.into()),
            ..Default::default()
        });
        let suite_instance =
            runner.start_suite_run(test_url, options).expect("Cannot create suite instance");
        let (suite_events, _) =
            collect_suite_events_with_watch(suite_instance, false, compressed).await.unwrap();

        let num_debug_data_events = stream::iter(suite_events)
            .then(|run_event| async move {
                let RunEvent::DebugData { filename, socket } = run_event else {
                    return false;
                };
                filename.starts_with("data_sink/data_sink.")
                    && collect_string_from_socket_helper(socket, compressed).await.unwrap()
                        == "Debug data from test\n"
            })
            .filter(|matches_vmo| futures::future::ready(*matches_vmo))
            .count()
            .await;
        assert_eq!(num_debug_data_events, 1);
    }
}

#[fuchsia::test]
async fn custom_artifact_realm_test() {
    let test_url = "fuchsia-pkg://fuchsia.com/test_manager_specified_realm_test#meta/custom_artifact_realm_test.cm";

    let (events, _) =
        run_test_in_hermetic_test_realm(test_url, default_run_suite_options()).await.unwrap();
    let events = events.into_iter().group_by_test_case_unordered();

    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found("use_artifact"),
        RunEvent::case_started("use_artifact"),
        RunEvent::case_stopped("use_artifact", TestCaseResult::Passed),
        RunEvent::case_finished("use_artifact"),
        RunEvent::suite_stopped(SuiteResult::Finished),
        RunEvent::suite_custom("test_driver", "artifact.txt", "Hello, world!"),
    ]
    .into_iter()
    .group_by_test_case_unordered();

    assert_eq!(&expected_events, &events);
}
