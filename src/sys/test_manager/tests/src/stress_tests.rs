// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_test_manager as ftest_manager;
use ftest_manager::{SuiteResult, TestCaseResult};
use fuchsia_component::client;
use futures::prelude::*;
use pretty_assertions::assert_eq;
use test_manager_test_lib::{
    GroupRunEventByTestCase, RunEvent, SuiteRunner, collect_string_from_socket_helper,
    collect_suite_events_with_watch, default_run_suite_options,
};

async fn debug_data_stress_test(
    case_name: &str,
    vmo_count: usize,
    vmo_size: usize,
    compressed: bool,
) {
    const TEST_URL: &str =
        "fuchsia-pkg://fuchsia.com/test_manager_stress_test#meta/debug_data_spam_test.cm";

    let runner = SuiteRunner::new(
        client::connect_to_protocol::<ftest_manager::SuiteRunnerMarker>()
            .expect("cannot connect to suite runner proxy"),
    );
    let mut options = default_run_suite_options();
    options.test_case_filters = Some(vec![case_name.into()]);
    let suite_instance =
        runner.start_suite_run(TEST_URL, options).expect("Cannot create suite instance");
    let (suite_events, _) =
        collect_suite_events_with_watch(suite_instance, false, compressed).await.unwrap();
    let (debug_data_events, suite_events): (Vec<_>, Vec<_>) =
        suite_events.into_iter().partition(|e| matches!(e, RunEvent::DebugData { .. }));
    let expected_events = vec![
        RunEvent::suite_started(),
        RunEvent::case_found(case_name),
        RunEvent::case_started(case_name),
        RunEvent::case_stopped(case_name, TestCaseResult::Passed),
        RunEvent::case_finished(case_name),
        RunEvent::suite_stopped(SuiteResult::Finished),
    ];

    assert_eq!(
        suite_events.into_iter().group_by_test_case_unordered(),
        expected_events.into_iter().group_by_test_case_unordered(),
    );

    let test_run_events = stream::iter(debug_data_events);
    let num_vmos = test_run_events
        .then(|run_event| async move {
            let RunEvent::DebugData { filename, socket } = run_event else {
                unreachable!();
            };
            let content = collect_string_from_socket_helper(socket, compressed)
                .await
                .unwrap_or_else(|e| panic!("cannot read socket for {}: {:?}", &filename, e));
            content.len() == vmo_size && content.chars().all(|c| c == 'a')
        })
        .filter(|matches_vmo| futures::future::ready(*matches_vmo))
        .count()
        .await;
    assert_eq!(num_vmos, vmo_count);
}

#[fuchsia::test]
async fn debug_data_stress_test_many_vmos() {
    const NUM_EXPECTED_VMOS: usize = 3250;
    const VMO_SIZE: usize = 4096;
    const CASE_NAME: &'static str = "many_small_vmos";
    debug_data_stress_test(CASE_NAME, NUM_EXPECTED_VMOS, VMO_SIZE, true).await;
}

#[fuchsia::test]
async fn debug_data_stress_test_few_large_vmos() {
    const NUM_EXPECTED_VMOS: usize = 2;
    const VMO_SIZE: usize = 1024 * 1024 * 400;
    const CASE_NAME: &'static str = "few_large_vmos";
    debug_data_stress_test(CASE_NAME, NUM_EXPECTED_VMOS, VMO_SIZE, true).await;
}
