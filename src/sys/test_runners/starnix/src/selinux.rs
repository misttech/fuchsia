// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::helpers;
use anyhow::Error;
use fidl::endpoints::create_proxy;
use fidl_fuchsia_test::{self as ftest, Result_ as TestResult};
use futures::io::BufReader;
use futures::{AsyncBufReadExt, AsyncWriteExt as _};
use zx::Socket;
use {
    fidl_fuchsia_component_runner as frunner, fidl_fuchsia_data as fdata, fuchsia_async as fasync,
};

pub async fn run_selinux_test_suite_cases(
    tests: Vec<ftest::Invocation>,
    mut start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    component_runner: &frunner::ComponentRunnerProxy,
) -> Result<(), Error> {
    for test in tests {
        // `test_name` corresponds to the test name as listed in the `tests/Makefile`'s
        // `SUBDIRS` variable in the SELinux test suite.
        let test_name = test.name.as_ref().expect("No test name");
        let mut start_info = frunner::ComponentStartInfo {
            program: Some(get_program_dictionary(&mut start_info, &test_name)),
            ..helpers::clone_start_info(&mut start_info)?
        };

        // Create the numbered handles table to pass to the component, and consume the client stdout
        // handle for use by parse_test_output(), replacing it with a new stream for the parsed
        // lines to be mirrored to, so that they appear in the top-level report.
        let (numbered_handles, mut std_handles) = helpers::create_numbered_handles();
        let (stdout_writer, stdout_reader) = zx::Socket::create_stream();
        let test_stdout = std_handles.out.take().unwrap();
        std_handles.out = Some(stdout_reader);

        // Start a top-level report through which to provide the whole of the test suite output.
        let top_level_report_proxy = helpers::start_top_level_report(
            &mut start_info,
            run_listener_proxy,
            numbered_handles,
            std_handles,
        )?;

        // Run the test component and parse out the individual test results.
        let _ = helpers::start_test_component(start_info, component_runner)?;
        parse_test_output(test, test_stdout, stdout_writer, run_listener_proxy).await?;

        // Always report the test-suite as passing; failures will be reported by parse_test_output
        // for individual cases.
        top_level_report_proxy
            .finished(&TestResult { status: Some(ftest::Status::Passed), ..Default::default() })?;
    }

    Ok(())
}

/// Returns the "program" dictionary for a single SELinux test suite case.
fn get_program_dictionary(
    base_start_info: &mut frunner::ComponentStartInfo,
    test_name: &str,
) -> fidl_fuchsia_data::Dictionary {
    let mut program_entries = vec![fdata::DictionaryEntry {
        key: "environ".to_string(),
        value: Some(Box::new(fdata::DictionaryValue::StrVec(vec![format!(
            "SUBDIRS={}",
            test_name.to_owned()
        )]))),
    }];

    if let Some(fidl_fuchsia_data::Dictionary { entries: Some(entries), .. }) =
        base_start_info.program.as_ref()
    {
        for entry in entries {
            match entry.key.as_str() {
                "binary" | "uid" | "seclabel" | "fsseclabel" => {
                    program_entries.push(entry.clone());
                }
                _ => (),
            }
        }
    }
    fidl_fuchsia_data::Dictionary { entries: Some(program_entries), ..Default::default() }
}

/// Parses the output of a single SELinux test suite case and reports the results of individual
/// subcases.
///
/// In this context a test suite case corresponds to the set of tests in a single file of
/// the SELinux test suite, like "perf_event". A subcase corresponds to a single tested line within
/// that SELinux test, like "perf_event/1" for the first test expectation within the "perf_event"
/// test.
async fn parse_test_output(
    test: ftest::Invocation,
    test_stdout: Socket,
    stdout: Socket,
    run_listener_proxy: &ftest::RunListenerProxy,
) -> Result<(), Error> {
    let mut reader: BufReader<fidl::AsyncSocket> =
        BufReader::new(fasync::Socket::from_socket(test_stdout));
    let mut writer = fasync::Socket::from_socket(stdout);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        // Copy output to test's stdout.
        writer.write_all(line.as_bytes()).await?;
        // The SELinux test suite reports the passed / failed tests starting with the prefix
        // "ok {index}" or "not ok {index}" correspondingly.
        let (status, index_str) = if let Some(index_str) = line.strip_prefix("ok ") {
            (Some(ftest::Status::Passed), index_str)
        } else if let Some(index_str) = line.strip_prefix("not ok ") {
            (Some(ftest::Status::Failed), index_str)
        } else {
            (None, "")
        };
        if let Some(status) = status {
            let index =
                index_str.trim().parse::<i32>().expect("expected format \"[not] ok {index}\"");
            report_result(test.clone(), index, run_listener_proxy, status)?;
        }
        line.clear();
    }
    Ok(())
}

/// Reports the result for the `index`-th subtest within a SELinux test suite case. This will be
/// later matched against the expectations of successes/failures in the `selinux.json5` file.
fn report_result(
    mut test: ftest::Invocation,
    index: i32,
    run_listener_proxy: &ftest::RunListenerProxy,
    status: ftest::Status,
) -> Result<(), Error> {
    let (_, std_handles) = helpers::create_numbered_handles();

    test.name = test.name.map(|name| format!("{name}/{:03}", index));

    let (case_listener_proxy, case_listener) = create_proxy::<ftest::CaseListenerMarker>();

    // Multiple tests run at once and we can only know which ones were executed after having
    // processed the output. This is why we can only report that a given test "started" after it
    // finished.
    run_listener_proxy.on_test_case_started(&test, std_handles, case_listener)?;
    case_listener_proxy.finished(&TestResult { status: Some(status), ..Default::default() })?;
    Ok(())
}
