# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("//build/bazel/rules/rust:rustc_test.bzl", "rustc_test")
load(":host_test.bzl", "host_test")
load(":host_test_data.bzl", "host_test_data_files")

def wrap_host_rustc_test(
        name,
        binary_name,
        test_label = None,
        test_args = [],
        test_data = [],
        visibility = None):
    """Generate a host_test() target to run a rustc_test().

    This ensures that a host rustc_test() binary can be run with 'fx test'
    and in infra builders. This is called implicitely by 'host_rustc_test()'
    and other macros wrapping Rust test targets.

    Args:
       name: (string) host_test() target name
       binary_name: (string) label to rustc_test() binary.
       test_label: (Optional[string]) Optional test_label to pass to host_test(). Default to None.
       test_args: (list[string]) List of test arguments.
       test_data: (list[string]) List of labels to test's runtime requirements.
       visibility: (list[string]) Visibility of the final host_test() target.
    """
    binary_as_test_data = binary_name + "_as_test_data"
    host_test_data_files(
        name = binary_as_test_data,
        srcs = [":" + binary_name],
        testonly = True,
        visibility = ["//visibility:private"],
    )

    # Wrap the binary invocation with //tools/rust_test_parser
    # LINT.IfChange(rustc_test_invocation)
    wrapper_script = "//tools/rust_test_parser:rust_test_parser"

    # In Bazel, and unlike GN, rustc_test() binaries always contain debug
    # symbols (see //build/bazel/debug_symbols/README.md for details).
    #
    # Enable Rust stack traces by default. In GN, when an ASan or Coverage
    # toolchain is detected, this is disabled and the test uses the stripped
    # binary instead. This is not implemented yet  because there is no good
    # way to track these in Bazel at the moment.
    #
    # TODO(https://fxbug.dev/495755822): Disable for ASan and Coverage build
    # configurations.
    test_args = [
        "env RUST_BACKTRACE=1",
        "./{}".format(binary_name),
    ] + test_args

    test_data = test_data + [":" + binary_as_test_data]

    # LINT.ThenChange(//build/rust/rustc_test.gni:rustc_test_invocation)

    host_test(
        name = name,
        binary = wrapper_script,
        test_label = test_label,
        test_args = test_args,
        data = test_data,
        target_compatible_with = HOST_CONSTRAINTS,
        visibility = visibility,
    )

def legacy_host_rustc_test(
        name,
        binary_name = "",
        test_label = None,
        test_args = [],
        test_data = [],
        tags = [],
        visibility = None,
        **kwargs):
    """Define a host test wrapping a Rust test that can be used with Fuchsia test runners.

    This is a convenience macro to call rustc_test() and host_test() together.

    Unlike rustc_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    The "manual" tag will be set on the rusc_test() target. In practice something like
    `fx bazel test --config=host //build/bazel/host_tests/rust_tests/...`
    will correctly only run one test target, instead of two for each host_rustc_test()
    definition.

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the rustc_test target, defaults to 'name + "_bin"'.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      tags: Optional: List of test tags.
      **kwargs: Arguments to pass to `rustc_test`.
    """

    if "args" in kwargs:
        fail("Use `test_args` to pass test arguments instead of `args`")

    binary_name = binary_name if binary_name else name + "_bin"
    binary_as_test_data = binary_name + "_as_test_data"

    if "manual" not in tags:
        tags = tags + ["manual"]

    rustc_test(
        name = binary_name,
        tags = tags,
        visibility = ["//visibility:private"],
        target_compatible_with = HOST_CONSTRAINTS,
        **kwargs
    )

    wrap_host_rustc_test(
        name,
        test_label = test_label,
        binary_name = binary_name,
        test_args = test_args,
        test_data = test_data,
        visibility = visibility,
    )

def _host_rustc_test_impl(
        name,
        visibility,
        binary_name,
        test_label,
        test_args,
        test_data,
        tags,
        **kwargs):
    legacy_host_rustc_test(
        name = name,
        binary_name = binary_name,
        test_label = test_label,
        test_args = test_args,
        test_data = test_data,
        tags = tags,
        visibility = visibility,
        **kwargs
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

The "manual" tag will be set on the rusc_test() target. In practice something like
`fx bazel test --config=host //build/bazel/host_tests/rust_tests/...`
will correctly only run one test target, instead of two for each host_rustc_test()
definition.

Accepts all rustc_test() attributes, plus `binary_name` and `test_xxx` ones.
""",
    inherit_attrs = rustc_test,
    attrs = {
        "binary_name": attr.string(default = "", doc = "The name of the rustc_test target, defaults to 'name + \"_bin\"'."),
        "test_label": attr.label(default = None, doc = "Optional override for the test_label passed to host_test()."),
        "test_args": attr.string_list(default = [], doc = "Arguments to pass to the test binary. Do not use `args`."),
        "test_data": attr.label_list(default = [], doc = "Data dependencies for the test target itself."),
    },
)

# Switch to symbolic macro once the inherit_attrs error is fixed.
host_rustc_test = legacy_host_rustc_test
