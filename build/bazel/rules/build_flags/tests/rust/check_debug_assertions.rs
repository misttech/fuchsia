// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// `expect_debug_assertions` is set from the GN `rust_debug_assertions` build
// arg, so this file only compiles if the toolchain's default flags agree with
// it.

#[cfg(all(debug_assertions, not(expect_debug_assertions)))]
compile_error!("debug assertions are enabled, but rust_debug_assertions is false");

#[cfg(all(not(debug_assertions), expect_debug_assertions))]
compile_error!("debug assertions are disabled, but rust_debug_assertions is true");

// The real check is the compile_error! guards above; this runtime restatement
// only makes the test report a passing case.
#[test]
fn debug_assertions_match_build_arg() {
    assert_eq!(cfg!(debug_assertions), cfg!(expect_debug_assertions));
}
