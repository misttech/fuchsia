# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust library with optional unit tests."""

load("@rules_rust//rust:defs.bzl", "rust_library")
load("//build/bazel/rules/rust:common.bzl", "with_fuchsia_rustc_flags")
load("//build/bazel/rules/rust:generate_unit_tests.bzl", "generate_unit_tests")

def _rustc_library_impl(name, with_host_unit_tests, with_unit_tests, test_deps, lint_config, rustc_flags, visibility = None, **kwargs):
    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_production"

    rustc_flags = with_fuchsia_rustc_flags(rustc_flags)

    rust_library(
        name = name,
        rustc_flags = rustc_flags,
        lint_config = lint_config,
        visibility = visibility,
        **kwargs
    )

    generate_unit_tests(name, with_host_unit_tests, with_unit_tests, test_deps, lint_config, rustc_flags, visibility, **kwargs)

rustc_library = macro(
    doc = """`rustc_library` wrapper that optionally defines a test target.

Besides being a shorthand, this is mainly used to allow easier syncing between
Bazel and GN targets. See details in https://fxbug.dev/407441714.
""",
    implementation = _rustc_library_impl,
    inherit_attrs = rust_library,
    attrs = {
        "with_host_unit_tests": attr.bool(
            doc = "If true, a `host_rustc_test` target will be created.",
            default = False,
            configurable = False,
        ),
        "with_unit_tests": attr.bool(
            doc = "If true, a `rustc_test` target will be created.",
            default = False,
            configurable = False,
        ),
        "test_deps": attr.label_list(
            doc = "Dependencies for the test target.",
            default = [],
        ),
    },
)
