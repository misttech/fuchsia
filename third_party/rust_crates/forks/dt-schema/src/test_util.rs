// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use tracing::Level;

#[allow(unused)]
/// This is intended to help with debugging test failures.
/// Call `crate::test_util::enable_all_logs()` at the start of your test to get full tracing output.
pub fn enable_all_logs() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .with_line_number(true)
            .finish(),
    )
    .unwrap();
}
