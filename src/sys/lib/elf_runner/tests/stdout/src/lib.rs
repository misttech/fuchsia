// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use diagnostics_data::{Data, Logs, Severity};
use diagnostics_reader::ArchiveReader;
use fidl_fuchsia_component as fcomponent;
use fuchsia_async::{MonotonicDuration, TestExecutor, Timer};
use fuchsia_component_test::ScopedInstance;
use futures::StreamExt;
use test_case::test_case;

const COLLECTION_NAME: &str = "puppets";

/// Used to parameterize test cases by which log messages to expect.
enum Expected {
    Stdout,
    Stderr,
    Both,
}

impl Expected {
    fn len(&self) -> usize {
        self.stdout() as usize + self.stderr() as usize
    }

    fn stdout(&self) -> bool {
        matches!(self, Expected::Stdout | Expected::Both)
    }

    fn stderr(&self) -> bool {
        matches!(self, Expected::Stderr | Expected::Both)
    }
}

#[test_case("#meta/logs-stdout-and-stderr-cpp.cm", "logs_stdout_and_stderr_cpp", Expected::Both; "cpp logs to both")]
#[test_case("#meta/logs-default-cpp.cm", "logs_default_cpp", Expected::Both; "cpp logs to both implicitly by default")]
#[test_case("#meta/logs-stdout-cpp.cm", "logs_stdout_cpp", Expected::Stdout; "cpp logs to stdout")]
#[test_case("#meta/logs-stderr-cpp.cm", "logs_stderr_cpp", Expected::Stderr; "cpp logs to stderr")]
#[test_case("#meta/logs-stdout-and-stderr-rust.cm", "logs_stdout_and_stderr_rust", Expected::Both; "rust logs to both")]
#[test_case("#meta/logs-default-rust.cm", "logs_default_rust", Expected::Both; "rust logs to both implicitly by default")]
#[test_case("#meta/logs-stdout-rust.cm", "logs_stdout_rust", Expected::Stdout; "rust logs to stdout")]
#[test_case("#meta/logs-stderr-rust.cm", "logs_stderr_rust", Expected::Stderr; "rust logs to stderr")]
#[fuchsia::test(add_test_attr = false)]
fn launch_component_and_check_messages(url: &str, moniker: &str, expected_messages: Expected) {
    // TODO(https://fxbug.dev/42176717) inline `test_inner` once the fuchsia test macro supports it
    TestExecutor::new().run_singlethreaded(test_inner(url, moniker, expected_messages));
}

async fn test_inner(url: &str, moniker: &str, expected: Expected) {
    // start the child in our collection
    let mut instance =
        ScopedInstance::new_with_name(moniker.into(), COLLECTION_NAME.into(), url.into())
            .await
            .unwrap();
    let binder: fcomponent::BinderProxy = instance.connect_to_protocol_at_exposed_dir().unwrap();
    let full_moniker = &format!("{}:{}", COLLECTION_NAME, moniker);

    // wait a little to increase chances we detect extra messages we don't want
    Timer::new(MonotonicDuration::from_seconds(3)).await;

    // Wait for component to exit
    let mut binder_stream = binder.take_event_stream();
    assert_matches!(binder_stream.next().await, None);

    // destroy the instance to ensure all its logs have been delivered to archivist
    let waiter = instance.take_destroy_waiter();
    drop(instance);
    waiter.await.unwrap();

    // read log messages
    let mut messages = ArchiveReader::logs()
        .select_all_for_component(full_moniker.as_str()) // only return logs for this puppet
        .with_minimum_schema_count(expected.len()) // retry until we have the expected number
        .snapshot()
        .await
        .unwrap()
        .into_iter()
        .collect::<Vec<_>>();
    // stdout and stderr are not ordered w.r.t. each other, pick a stable order for assertions
    messages.sort_by_key(|l| l.metadata.severity);
    let mut messages = messages.into_iter();

    // assert on the log messages we're reading
    if expected.stdout() {
        let observed = messages.next().unwrap();
        assert_eq!(get_msg(&observed), "Hello Stdout!");
        assert_eq!(observed.metadata.severity, Severity::Info);
    }
    if expected.stderr() {
        let observed = messages.next().unwrap();
        assert_eq!(get_msg(&observed), "Hello Stderr!");
        assert_eq!(observed.metadata.severity, Severity::Warn);
    }
    assert_eq!(messages.next(), None, "no more messages expected from child");
}

fn get_msg(m: &Data<Logs>) -> String {
    m.payload_message().unwrap().get_property("value").unwrap().string().unwrap().to_string()
}
