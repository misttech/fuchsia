# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for verifying `BuildFlagsInfo` providers in host tests."""

load("@fuchsia_rules_common//build_flags:providers.bzl", "BuildFlagsInfo")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")

visibility("private")

def _assert_list_equals(failures, field_name, expected, actual):
    if expected != actual:
        failures.append(
            "For field '{}': expected {}, got {}".format(field_name, expected, actual),
        )

def _verify_build_flags_bin_impl(ctx):
    target = ctx.attr.target_under_test
    if BuildFlagsInfo not in target:
        fail("Expected BuildFlagsInfo on target under test {}".format(target.label))

    info = target[BuildFlagsInfo]
    failures = []
    _assert_list_equals(failures, "cflags", ctx.attr.expected_cflags, info.cflags)
    _assert_list_equals(failures, "cflags_c", ctx.attr.expected_cflags_c, info.cflags_c)
    _assert_list_equals(failures, "cflags_cc", ctx.attr.expected_cflags_cc, info.cflags_cc)
    _assert_list_equals(failures, "defines", ctx.attr.expected_defines, info.defines)
    _assert_list_equals(failures, "include_dirs", ctx.attr.expected_include_dirs, info.include_dirs)
    _assert_list_equals(failures, "ldflags", ctx.attr.expected_ldflags, info.ldflags)
    _assert_list_equals(failures, "lib_dirs", ctx.attr.expected_lib_dirs, info.lib_dirs)
    _assert_list_equals(
        failures,
        "link_inputs",
        [f.short_path for f in ctx.files.expected_link_inputs],
        [f.short_path for f in info.link_inputs],
    )
    _assert_list_equals(
        failures,
        "link_deps",
        [dep[CcInfo] for dep in ctx.attr.expected_link_deps],
        info.link_deps,
    )
    _assert_list_equals(
        failures,
        "executable_link_deps",
        [dep[CcInfo] for dep in ctx.attr.expected_executable_link_deps],
        info.executable_link_deps,
    )
    _assert_list_equals(failures, "rustenv", ctx.attr.expected_rustenv, info.rustenv)
    _assert_list_equals(failures, "rustflags", ctx.attr.expected_rustflags, info.rustflags)

    if failures:
        fail("BuildFlagsInfo mismatch on {}:\n  {}".format(
            target.label,
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

_verify_build_flags_bin = rule(
    implementation = _verify_build_flags_bin_impl,
    executable = True,
    attrs = {
        "target_under_test": attr.label(
            mandatory = True,
            providers = [BuildFlagsInfo],
        ),
        "expected_cflags": attr.string_list(default = []),
        "expected_cflags_c": attr.string_list(default = []),
        "expected_cflags_cc": attr.string_list(default = []),
        "expected_defines": attr.string_list(default = []),
        "expected_include_dirs": attr.string_list(default = []),
        "expected_ldflags": attr.string_list(default = []),
        "expected_lib_dirs": attr.string_list(default = []),
        "expected_link_inputs": attr.label_list(allow_files = True, default = []),
        "expected_link_deps": attr.label_list(providers = [CcInfo], default = []),
        "expected_executable_link_deps": attr.label_list(providers = [CcInfo], default = []),
        "expected_rustenv": attr.string_list(default = []),
        "expected_rustflags": attr.string_list(default = []),
    },
)

def build_flags_info_test(name, **kwargs):
    """Define a host test verifying the BuildFlagsInfo provider of `target_under_test`."""
    bin_name = name + "_bin"
    _verify_build_flags_bin(
        name = bin_name,
        testonly = True,
        **kwargs
    )
    host_test(
        name = name,
        binary = ":" + bin_name,
    )
