# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@io_bazel_rules_go//go:def.bzl", "go_test")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load(":host_test.bzl", "host_test")

def legacy_host_go_test(
        name,
        binary_name = "",
        test_name = "",
        test_args = [],
        test_data = [],
        tags = [],
        **kwargs):
    """Define a host test wrapping a Go binary that can be used with Fuchsia test runners.

    This is a convenience macro to call go_test() and host_test() together.

    Unlike go_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    The "manual" tag will be set on the go_test() target. In practice something like
    `fx bazel test --config=host //build/bazel/host_tests/go_tests/...`
    will correctly only run one test target, instead of two for each host_go_test()
    definition.

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the go_test target, defaults to 'name + "_bin"'.
      test_name: Optional. The name of the test, as seen by `fx test` and `botanist`,
         defaults to 'name'.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      **kwargs: Arguments to pass to `go_test`.
    """
    if "args" in kwargs:
        fail("Use `test_args` to pass test arguments instead of `args`")
    binary_name = binary_name if binary_name else name + "_bin"

    if "manual" not in tags:
        tags = tags + ["manual"]

    go_test(
        name = binary_name,
        tags = tags,
        **kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
        test_name = test_name,
        test_args = test_args,
        data = test_data,
        target_compatible_with = HOST_CONSTRAINTS,
    )

def _host_go_test_impl(
        name,
        visibility,
        binary_name = "",
        test_name = "",
        test_args = [],
        test_data = [],
        **kwargs):
    binary_name = binary_name if binary_name else name + "_bin"

    if "manual" not in tags:
        tags = tags + ["manual"]

    go_test(
        name = binary_name,
        tags = tags,
        **kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
        test_name = test_name,
        test_args = test_args,
        data = test_data,
        target_compatible_with = HOST_CONSTRAINTS,
    )

# TODO(https://fxbug.dev/349341932): Switch to symbolic macro once the inherit_attrs error is fixed.
#
# This is a broken attempt to create a symbolic macro that would inherit all attributes.
#
# Unfortunately, it requires an inherit_attrs line that fails with go_test.
# Hopefully this could be fixed with a future Bazel upgrade.
broken_symbolic_host_go_test = macro(
    implementation = _host_go_test_impl,
    doc = """
Define a host test wrapping a Go binary that can be used with Fuchsia test runners.

This is a convenience macro to call go_test() and host_test() together.

Unlike go_test(), these tests will be usable with `fx test` and `botanist`, and can
still be run locally using `fx bazel test --config=host <label>`.

The "manual" tag will be set on the go_test() target. In practice something like
`fx bazel test --config=host //build/bazel/host_tests/go_tests/...`
will correctly only run one test target, instead of two for each host_go_test()
definition.

Accepts all go_test() attributes, plus `binary_name` and `test_xxx` ones.
""",
    inherit_attrs = go_test,
    attrs = {
        "binary_name": attr.string(default = "", doc = "The name of the go_test target, defaults to 'name + \"_bin\"'."),
        "test_name": attr.string(default = "", doc = "The name of the test, as seen by `fx test` and `botanist`, defaults to 'name'."),
        "test_args": attr.string_list(default = [], doc = "Arguments to pass to the test binary. Do not use `args`."),
        "test_data": attr.label_list(default = [], doc = "Data dependencies for the test target itself."),
    },
)

# Switch to symbolic macro once the inherit_attrs error is fixed.
host_go_test = legacy_host_go_test
