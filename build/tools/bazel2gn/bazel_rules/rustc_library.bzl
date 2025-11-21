# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust library with optional unit tests."""

load("@rules_rust//rust:defs.bzl", "rust_library", "rust_test")

def _rustc_library_impl(name, with_unit_tests, test_deps, lint_config, **kwargs):
    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_production"

    rust_library(
        name = name,
        **kwargs
    )

    if with_unit_tests:
        rust_test(
            name = "{}_test".format(name),
            crate = ":{}".format(name),
            deps = test_deps,
            crate_features = kwargs.get("crate_features", []),
            lint_config = "//build/config/rust/lints:clippy_warn_default",
        )

rustc_library = macro(
    doc = """`rustc_library` wrapper that optionally defines a test target.

Besides being a shorthand, this is mainly used to allow easier syncing between
Bazel and GN targets. See details in http://fxbug.dev/407441714.
""",
    implementation = _rustc_library_impl,
    inherit_attrs = rust_library,
    attrs = {
        "with_unit_tests": attr.bool(
            doc = "If true, a `rust_test` target will be created.",
            default = False,
            configurable = False,
        ),
        "test_deps": attr.label_list(
            doc = "Dependencies for the test target.",
            default = [],
        ),
    },
)
