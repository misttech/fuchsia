# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("//build/bazel/rules/rust:rustc_test.bzl", "rustc_test")
load(":host_test.bzl", "host_test")

def legacy_host_rustc_test(name, binary_name = "", test_name = "", test_args = [], test_data = [], **kwargs):
    """Define a host test wrapping a Rust test that can be used with Fuchsia test runners.

    This is a convenience macro to call rustc_test() and host_test() together.

    Unlike rustc_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the rustc_test target, defaults to 'name + "_bin"'.
      test_name: Optional. The name of the test, as seen by `fx test` and `botanist`,
         defaults to 'name'.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      **kwargs: Arguments to pass to `rustc_test`.
    """

    if "args" in kwargs:
        fail("Use `test_args` to pass test arguments instead of `args`")

    binary_name = binary_name if binary_name else name + "_bin"

    rustc_test(
        name = binary_name,
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

def _host_rustc_test_impl(name, visibility, binary_name = "", test_name = "", test_args = [], test_data = [], **kwargs):
    binary_name = binary_name if binary_name else name + "_bin"

    rustc_test(
        name = binary_name,
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
# Unfortunately, it requires an inherit_attrs line that fails with rustc_test.
# Hopefully this could be fixed with a future Bazel upgrade.
broken_symbolic_host_rustc_test = macro(
    implementation = _host_rustc_test_impl,
    doc = """
Define a host test wrapping a Rust test that can be used with Fuchsia test runners.

This is a convenience macro to call rustc_test() and host_test() together.

Unlike rustc_test(), these tests will be usable with `fx test` and `botanist`, and can
still be run locally using `fx bazel test --config=host <label>`.

Accepts all rustc_test() attributes, plus `binary_name` and `test_xxx` ones.
""",
    inherit_attrs = rustc_test,
    attrs = {
        "binary_name": attr.string(default = "", doc = "The name of the rustc_test target, defaults to 'name + \"_bin\"'."),
        "test_name": attr.string(default = "", doc = "The name of the test, as seen by `fx test` and `botanist`, defaults to 'name'."),
        "test_args": attr.string_list(default = [], doc = "Arguments to pass to the test binary. Do not use `args`."),
        "test_data": attr.label_list(default = [], doc = "Data dependencies for the test target itself."),
    },
)

# Switch to symbolic macro once the inherit_attrs error is fixed.
host_rustc_test = legacy_host_rustc_test
