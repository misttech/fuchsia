# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("//build/bazel/host_tests:host_rustc_test.bzl", "host_rustc_test")
load("//build/bazel/rules/rust:rustc_test.bzl", "rustc_test")

def generate_unit_tests(name, with_host_unit_tests, with_unit_tests, test_deps, lint_config, rustc_flags, visibility, **kwargs):
    if with_host_unit_tests and with_unit_tests:
        fail("Cannot specify both with_host_unit_tests and with_unit_tests on {}".format(name))

    if with_host_unit_tests:
        host_rustc_test(
            name = "{}_test".format(name),
            crate = ":{}".format(name),
            rustc_flags = rustc_flags,
            lint_config = lint_config,
            deps = test_deps,
            crate_features = kwargs.get("crate_features", []),
            visibility = visibility,
        )

    if with_unit_tests:
        test_kwargs = {}
        if "target_compatible_with" in kwargs:
            test_kwargs["target_compatible_with"] = kwargs["target_compatible_with"]
        if "tags" in kwargs:
            test_kwargs["tags"] = kwargs["tags"]

        rustc_test(
            name = "{}_test".format(name),
            crate = ":{}".format(name),
            rustc_flags = rustc_flags,
            lint_config = lint_config,
            deps = test_deps,
            crate_features = kwargs.get("crate_features", []),
            visibility = visibility,
            **test_kwargs
        )
