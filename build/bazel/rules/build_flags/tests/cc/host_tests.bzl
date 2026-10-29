# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper macros and verification rules for C++ `build_flags()` host tests."""

load(
    "@fuchsia_rules_common//build_flags:aspects.bzl",
    "BuildFlagsLabelsInfo",
    "collect_build_flags_labels_aspect",
)
load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "wrap_cc_macro_args_with_build_flags",
)
load("@fuchsia_rules_common//build_flags:providers.bzl", "BuildFlagsInfo")
load("@rules_cc//cc:cc_binary.bzl", "cc_binary")
load("@rules_cc//cc:cc_library.bzl", "cc_library")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")

visibility("private")

def build_flags_cc_binary_host_test(
        name,
        build_flags = [],
        disable_build_flags = [],
        **kwargs):
    """Define a new C++ or C host test for build_flags() support.

    Args:
       name: (string) Name of the host_test() target, the binary will be named "{name}_bin".
       build_flags: (list[label]) optional list of labels to build_flags() targets.
       disable_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to cc_binary().
    """
    binary_name = "{}_bin".format(name)

    new_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs,
        name = name,
        cc_rule_name = "cc_binary",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = "cxx_executable",
    )

    cc_binary(
        name = binary_name,
        **new_kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
    )

def build_flags_cc_library(
        name,
        build_flags = [],
        disable_build_flags = [],
        **kwargs):
    """Define a new C++ library for build_flags() support.

    Args:
       name: (string) Name of the library target.
       build_flags: (list[label]) optional list of labels to build_flags() targets.
       disable_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to cc_library().
    """
    new_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs,
        name = name,
        cc_rule_name = "cc_library",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = "cxx_common",
    )

    cc_library(
        name = name,
        **new_kwargs
    )

def _verify_build_flags_labels_bin_impl(ctx):
    actual_labels = ctx.attr.target_under_test[BuildFlagsLabelsInfo].labels
    failures = []

    # Verify every expected build_flags() label is present and
    # appears strictly after the preceding expected label, confirming flag
    # precedence order (e.g., no-fat-lto-objects-linking after caller flags).
    prev_index = -1
    prev_label = None
    for expected_target in ctx.attr.expected_ordered_labels:
        expected_label = expected_target[BuildFlagsInfo].label
        if expected_label not in actual_labels:
            failures.append("Expected build_flags label {} not found in {}".format(
                expected_label,
                actual_labels,
            ))
            continue
        idx = actual_labels.index(expected_label)
        if idx <= prev_index:
            failures.append(
                "Expected build_flags label {} (index {}) to appear after {} (index {}) in {}".format(
                    expected_label,
                    idx,
                    prev_label,
                    prev_index,
                    actual_labels,
                ),
            )
        prev_index = idx
        prev_label = expected_label

    for unexpected_target in ctx.attr.unexpected_labels:
        unexpected_label = unexpected_target[BuildFlagsInfo].label
        if unexpected_label in actual_labels:
            failures.append("Unexpected build_flags label {} found in {}".format(
                unexpected_label,
                actual_labels,
            ))

    if failures:
        fail("Final build_flags mismatch on {}:\n  {}".format(
            ctx.attr.target_under_test.label,
            "\n  ".join(failures),
        ))

    output = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        output = output,
        content = "#!/bin/sh\nexit 0\n",
        is_executable = True,
    )
    return [
        DefaultInfo(
            files = depset([output]),
            executable = output,
            runfiles = ctx.runfiles(files = [output]),
        ),
    ]

_verify_build_flags_labels_bin = rule(
    doc = "Asserts ordered presence and absence of build_flags() labels on a target.",
    implementation = _verify_build_flags_labels_bin_impl,
    executable = True,
    attrs = {
        "expected_ordered_labels": attr.label_list(
            doc = "build_flags() targets that must be applied to the target, in this relative order.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
        "target_under_test": attr.label(
            doc = "The target under test, wrapped with build_flags() support.",
            mandatory = True,
            aspects = [collect_build_flags_labels_aspect],
        ),
        "unexpected_labels": attr.label_list(
            doc = "build_flags() targets that must not be applied to the target.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
    },
)

def verify_build_flags_labels_test(
        name,
        target_under_test,
        expected_ordered_labels = [],
        unexpected_labels = []):
    """Defines a host test asserting the build_flags() applied to `target_under_test`.

    Args:
        name: (string) Name of the host_test() target.
        target_under_test: (label) Label of a target wrapped with build_flags() support (e.g. `fx_cc_test`).
        expected_ordered_labels: (list[label]) `build_flags` labels expected in order.
        unexpected_labels: (list[label]) `build_flags` labels expected to be absent.
    """
    bin_name = name + "_bin"
    _verify_build_flags_labels_bin(
        name = bin_name,
        testonly = True,
        target_under_test = target_under_test,
        expected_ordered_labels = expected_ordered_labels,
        unexpected_labels = unexpected_labels,
    )
    host_test(
        name = name,
        binary = ":" + bin_name,
    )
