// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::execution::crash_reporter::{CRASH_LOOP_LIMIT, CrashReporter};
use starnix_core::testing::spawn_kernel_and_run;
use zx;

#[fuchsia::test]
async fn begin_crash_report_throttling_ends() {
    spawn_kernel_and_run(|_, current_task| {
        let crash_reporter = CrashReporter::new(
            &fuchsia_inspect::Node::default(),
            /*proxy=*/ None,
            zx::Duration::from_millis(200),
            /*enable_throttling=*/ true,
        );

        for _ in 0..CRASH_LOOP_LIMIT {
            assert!(crash_reporter.begin_crash_report(current_task).is_some());
        }
        assert!(crash_reporter.begin_crash_report(current_task).is_none());

        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(crash_reporter.begin_crash_report(current_task).is_some());
    });
}
