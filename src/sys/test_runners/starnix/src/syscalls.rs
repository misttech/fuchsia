// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::debian_guest::DebianGuest;
use crate::{gtest, helpers, results_parser};

use anyhow::{self, Context};
use cm_types::NamespacePath;
use fidl::endpoints::{self, Proxy};
use fidl_fuchsia_test::{self as ftest, CaseListenerProxy, Result_ as TestResult, Status};
use fuchsia_fs::{self, directory};
use gtest_runner_lib::parser::TestSuiteOutput;
use helpers::TestType;
use namespace::Namespace;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::sync::Arc;
use {fidl_fuchsia_component_runner as frunner, fidl_fuchsia_io as fio, runner};

// TODO(https://fxbug.dev/436831317): Execute within a proper working directory.
const GUEST_TEST_ROOT: &str = "/";
const GUEST_DEPS_PATH: &str = "/data/tests/deps/";
const HOST_TEST_DEPS_PATH: &str = "data/tests/deps/";
const HOST_TMP_DIR: &str = "/tmp/";
const CML_TARGET_KERNEL_FIELD: &str = "test_target_kernel";

/// Shallow helper struct, simply for encapsulating the various ways that tests report.
struct TestRunnerReport {
    stdout: zx::Socket,
    stderr: zx::Socket,
    top_level_report_proxy: CaseListenerProxy,
    individual_report_proxies: HashMap<String, CaseListenerProxy>,
}

pub async fn run_syscall_tests(
    tests: Vec<ftest::Invocation>,
    mut test_start_info: frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    component_runner: &frunner::ComponentRunnerProxy,
    debian_guest: Arc<DebianGuest>,
) -> Result<(), anyhow::Error> {
    let program_data = test_start_info.program.as_mut().unwrap();
    let test_target_kernel =
        helpers::take_opt_str_value_from_dict(program_data, CML_TARGET_KERNEL_FIELD)?;

    match test_target_kernel.as_deref() {
        Some("linux") => {
            log::info!(
                "Linux {} specified, bootstrapping the Machina guest.",
                CML_TARGET_KERNEL_FIELD
            );

            run_on_debian_guest(&tests, &mut test_start_info, run_listener_proxy, debian_guest)
                .await
        }
        None => {
            log::info!(
                "No {} specified, defaulting to Starnix environment for execution.",
                CML_TARGET_KERNEL_FIELD
            );

            // Forward Starnix kernel tests over to the vanilla Gtest runner.
            gtest::run_gtest_cases(
                tests,
                test_start_info,
                run_listener_proxy,
                component_runner,
                TestType::Gtest,
            )
            .await
        }
        Some(unexpected_target) => {
            anyhow::bail!(
                "Unexpected and unknown {} specified. Value: {}",
                CML_TARGET_KERNEL_FIELD,
                unexpected_target
            )
        }
    }
}

/// Runs a set of tests within a Debian guest and reports the results. This function handles setup,
/// pushing test dependencies, executing the tests, and retrieving and parsing the output.
async fn run_on_debian_guest(
    tests: &Vec<ftest::Invocation>,
    test_start_info: &mut frunner::ComponentStartInfo,
    run_listener_proxy: &ftest::RunListenerProxy,
    debian_guest: Arc<DebianGuest>,
) -> Result<(), anyhow::Error> {
    // We need to take() the namespace from the start_info, but will immediately clone it back.
    let test_component_ns = Namespace::try_from(test_start_info.ns.take().unwrap())?;
    test_start_info.ns = Some(test_component_ns.clone().try_into()?);

    // Initialize the environment.
    let test_runner_report =
        initialize_test_runner_reporting(&tests, run_listener_proxy, &test_start_info)?;
    let guest_binary_location =
        push_test_dependencies(test_component_ns, debian_guest.clone(), &test_start_info).await?;
    let (exec_command, guest_output_filename) = format_exec_command(&tests, guest_binary_location);

    // Execute the tests and retrieve the results.
    debian_guest
        .execute(
            &exec_command,
            &[],
            None,
            Some(test_runner_report.stdout),
            Some(test_runner_report.stderr),
        )
        .await?;
    let test_results = get_test_results(debian_guest.clone(), guest_output_filename).await?;

    // Report results back to the test runner. We mark the overall test suite as "Passed,"
    // but we will parse and report the actual status of individual tests afterwards.
    test_runner_report
        .top_level_report_proxy
        .finished(&TestResult { status: Some(Status::Passed), ..Default::default() })?;
    gtest::report_test_results(test_runner_report.individual_report_proxies, test_results)?;

    Ok(())
}

/// Pushes the test binary and dependencies to the virtualized guest, returning the location
/// of the test binary on the guest.
async fn push_test_dependencies(
    mut test_component_ns: Namespace,
    debian_guest: Arc<DebianGuest>,
    test_start_info: &frunner::ComponentStartInfo,
) -> Result<String, anyhow::Error> {
    let test_pkg_dir = test_component_ns
        .remove(&NamespacePath::new("/pkg")?)
        .ok_or(anyhow::anyhow!("Could not find /pkg in namespace!"))?
        .into_proxy();

    // The data dependencies are shared across all syscall tests, and therefore only need to be
    // pushed a single time. Due to state management issues, the DebianGuest stores the push state.
    if !debian_guest.are_deps_pushed() {
        push_data_deps_to_guest(&test_pkg_dir, debian_guest.clone()).await?;
    }

    Ok(push_test_binary_to_guest(test_start_info, &test_pkg_dir, debian_guest.clone()).await?)
}

/// Transfers a test results file from a guest and parses its contents. In the case of transfer or
/// parsing errors, it logs a warning and returns an empty vector. The test reporting mechanism
/// should handle interpreting the empty vector appropriately.
async fn get_test_results(
    debian_guest: Arc<DebianGuest>,
    guest_output_filename: String,
) -> Result<Vec<TestSuiteOutput>, anyhow::Error> {
    // Firstly, transfer the results file from the guest back to the host.
    let host_test_output_path = format!("{}{}", HOST_TMP_DIR, guest_output_filename);
    let guest_test_output_path = get_guest_test_output_path(&guest_output_filename);
    let mut host_test_output_file =
        OpenOptions::new().write(true).create_new(true).open(&host_test_output_path)?;
    let file_channel: zx::Channel = fdio::transfer_fd(host_test_output_file)?.into();
    let file_client_end = endpoints::ClientEnd::from(file_channel);
    let transfer_result =
        debian_guest.get_file(guest_test_output_path.as_str(), file_client_end).await;

    match transfer_result {
        Ok(_) => {
            // Read and parse the results. Since the backing file handle is consumed by the transfer_fd
            // call, we need to reopen the file to read the contents.
            host_test_output_file = OpenOptions::new().read(true).open(&host_test_output_path)?;
            let mut gtest_output_buffer = String::new();
            let test_results = host_test_output_file
                .read_to_string(&mut gtest_output_buffer)
                .with_context(|| {
                    format!("Failed to read {}{}", HOST_TMP_DIR, guest_output_filename)
                })
                .and_then(|_| {
                    results_parser::parse_results(TestType::Gtest, gtest_output_buffer.trim())
                        .with_context(|| {
                            format!("Failed to parse {}{}", HOST_TMP_DIR, guest_output_filename)
                        })
                });

            // If tests crashes then we may fail to read or parse the output file. We should handle that
            // edge case gracefully so that callers can decide how to proceed.
            let test_result_list = match test_results {
                Ok(results) => results.testsuites,
                Err(e) => {
                    log::error!("Tests crashed whilst running: {}", e);
                    vec![]
                }
            };

            Ok(test_result_list)
        }
        Err(e) => {
            // Transfer errors could be an issue with the Machina guest, but are most likely
            // indicative of the test result file not being present. That is most likely indicative
            // of the tests crashing while running. In any case, we'll return an empty list. The
            // parsing contract will handle reporting these as failures.
            log::warn!(
                "Failed to transfer results file, which likely indicates the tests crashed while running: {}",
                e
            );
            Ok(vec![])
        }
    }
}

/// Pushes all data dependencies to the running guest.
async fn push_data_deps_to_guest(
    pkg_dir_proxy: &fio::DirectoryProxy,
    debian_guest: Arc<DebianGuest>,
) -> Result<(), anyhow::Error> {
    let deps_dir =
        directory::open_directory(pkg_dir_proxy, HOST_TEST_DEPS_PATH, fio::PERM_READABLE)
            .await
            .with_context(|| format!("Failed to open deps directory: {}", HOST_TEST_DEPS_PATH))?;

    let entries = directory::readdir(&deps_dir).await?;
    for entry in entries {
        match entry.kind {
            directory::DirentKind::File => {
                let file_name = entry.name;
                let source_file =
                    fuchsia_fs::directory::open_file(&deps_dir, &file_name, fio::PERM_READABLE)
                        .await
                        .with_context(|| format!("Failed to open dep file: {}", file_name))?;

                let source = source_file.into_client_end().map_err(|s| {
                    anyhow::anyhow!("Failed to convert source file to client end: {:?}", s)
                })?;

                let guest_dest = format!("{}{}", GUEST_DEPS_PATH, file_name);
                debian_guest.push_data_to_guest(source, &guest_dest).await?;
            }
            _ => {
                // We don't currently anticipate nested subdirectories in the deps folder, and so
                // haven't written the code to mirror such a structure in the Machina guest. If this
                // assumption breaks, tests will begin failing and we should complain loudly.
                anyhow::bail!("Unexpected file / folder structure in deps folder: {}", entry.name);
            }
        }
    }

    debian_guest.mark_deps_pushed();
    Ok(())
}

/// Pushes the test binary from the ComponentStartInfo to the Debian guest.
async fn push_test_binary_to_guest(
    test_start_info: &frunner::ComponentStartInfo,
    pkg_dir_proxy: &fio::DirectoryProxy,
    debian_guest: Arc<DebianGuest>,
) -> Result<String, anyhow::Error> {
    let host_binary_location = runner::get_program_binary(test_start_info)?;
    let guest_dest = get_guest_test_binary_path(&host_binary_location)?;

    let source =
        directory::open_file(pkg_dir_proxy, host_binary_location.as_str(), fio::PERM_READABLE)
            .await?
            .into_client_end()
            .map_err(|_| anyhow::anyhow!("Converting test bin file to client end failed"))?;

    debian_guest.push_data_to_guest(source, &guest_dest).await?;
    Ok(guest_dest)
}

/// Formats the guest exec command, handling the appropriate gtest filters as well
/// as the JSON output file for test results. Returns the (exec_command, output_filepath)
fn format_exec_command(
    tests: &Vec<ftest::Invocation>,
    guest_binary_location: String,
) -> (String, String) {
    let test_filter_arg = gtest::create_tests_filter_arg(tests, TestType::Gtest);
    let guest_output_filename = helpers::unique_test_result_filename();
    let guest_output_path = get_guest_test_output_path(&guest_output_filename);
    let output_arg =
        helpers::format_arg(TestType::Gtest, &format!("output={}:{}", "json", guest_output_path));
    let exec_command = format!("{} {} {}", guest_binary_location, test_filter_arg, output_arg);

    return (exec_command, guest_output_filename);
}

/// Gets the absolute filepath for the test results, given the unique filename.
fn get_guest_test_output_path(guest_output_filename: &String) -> String {
    return format!("{}{}", GUEST_TEST_ROOT, guest_output_filename);
}

/// Gets an absolute filepath for the test binary, given the source location.
fn get_guest_test_binary_path(source_location: &String) -> Result<String, anyhow::Error> {
    let binary_name =
        source_location.split('/').last().expect("Binary path format was unexpected.").to_string();
    Ok(format!("{}{}", GUEST_TEST_ROOT, binary_name))
}

/// Initializes the necessary plumbing for capturing stdout and stderr,
/// and for reporting test results to the framework's run listener(s).
fn initialize_test_runner_reporting(
    tests: &Vec<ftest::Invocation>,
    run_listener_proxy: &ftest::RunListenerProxy,
    test_start_info: &frunner::ComponentStartInfo,
) -> Result<TestRunnerReport, anyhow::Error> {
    let (test_stdout, stdout_client) = zx::Socket::create_stream();
    let (test_stderr, stderr_client) = zx::Socket::create_stream();
    let std_handles = ftest::StdHandles {
        out: Some(stdout_client),
        err: Some(stderr_client),
        ..Default::default()
    };
    let (top_level_report_proxy, overall_test_listener) =
        endpoints::create_proxy::<ftest::CaseListenerMarker>();
    run_listener_proxy.on_test_case_started(
        &ftest::Invocation {
            name: Some(test_start_info.resolved_url.clone().unwrap_or_default()),
            tag: None,
            ..Default::default()
        },
        std_handles,
        overall_test_listener,
    )?;

    let test_report_proxies = helpers::start_tests(&tests, run_listener_proxy)?;
    Ok(TestRunnerReport {
        stdout: test_stdout,
        stderr: test_stderr,
        top_level_report_proxy: top_level_report_proxy,
        individual_report_proxies: test_report_proxies,
    })
}
