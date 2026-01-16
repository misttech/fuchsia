# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust test with Fuchsia-specific lint config by default."""

load("@rules_rust//rust:defs.bzl", "rust_test")
load("//build/bazel/rules/rust:common.bzl", "with_fuchsia_rustc_flags")

def _rustc_test_impl(name, lint_config, rustc_flags, **kwargs):
    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_default"

    rust_test(
        name = name,
        lint_config = lint_config,
        rustc_flags = with_fuchsia_rustc_flags(rustc_flags),
        **kwargs
    )

rustc_test = macro(
    doc = "rustc_test defines a Rust test target with Fuchsia-specific lint config by default.",
    implementation = _rustc_test_impl,
    inherit_attrs = rust_test,
)
