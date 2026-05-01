# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro for defining a Rust proc-macro with optional unit tests."""

load("@rules_rust//rust:defs.bzl", "rust_proc_macro")
load("//build/bazel/host_tests:host_rustc_test.bzl", "host_rustc_test")
load("//build/bazel/rules/rust:common.bzl", "with_fuchsia_rustc_flags")

def _rustc_proc_macro_impl(name, with_host_unit_tests, test_deps, lint_config, rustc_flags, **kwargs):
    if lint_config == None:
        lint_config = "//build/config/rust/lints:clippy_warn_production"

    rustc_flags = with_fuchsia_rustc_flags(rustc_flags)

    rust_proc_macro(
        name = name,
        rustc_flags = rustc_flags,
        lint_config = lint_config,
        **kwargs
    )

    if with_host_unit_tests:
        host_rustc_test(
            name = "{}_test".format(name),
            crate = ":{}".format(name),
            rustc_flags = rustc_flags,
            deps = test_deps,
            crate_features = kwargs.get("crate_features", []),
        )

rustc_proc_macro = macro(
    doc = """`rustc_proc_macro` wrapper that optionally defines a test target.

Besides being a shorthand, this is mainly used to allow easier syncing between
Bazel and GN targets. See details in https://fxbug.dev/407441714.
""",
    implementation = _rustc_proc_macro_impl,
    inherit_attrs = rust_proc_macro,
    attrs = {
        "with_host_unit_tests": attr.bool(
            doc = "If true, a `host_rustc_test` target will be created.",
            default = False,
            configurable = False,
        ),
        "test_deps": attr.label_list(
            doc = "Dependencies for the test target.",
            default = [],
        ),
    },
)
