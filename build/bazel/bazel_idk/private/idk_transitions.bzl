# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Transitions used to build the IDK."""

load("@fuchsia_build_info//:args.bzl", "idk_buildable_api_levels", "idk_buildable_cpus")

visibility(["//build/bazel/bazel_idk/tests/..."])

def _cpu_api_level_transition_impl(_settings, _attr):
    combinations = []
    for api_level in idk_buildable_api_levels:
        for cpu in idk_buildable_cpus:
            combinations.append({
                "@//build/bazel:fuchsia_api_level": api_level,
                "//command_line_option:platforms": "@//build/bazel/platforms:fuchsia_platform_%s" % cpu,
            })
    return combinations

cpu_api_level_transition = transition(
    implementation = _cpu_api_level_transition_impl,
    inputs = [],
    outputs = [
        "@//build/bazel:fuchsia_api_level",
        "//command_line_option:platforms",
    ],
)

def _build_idk_combinations_impl(ctx):
    all_deps_depset = depset(direct = ctx.files.deps)

    return [
        DefaultInfo(files = all_deps_depset),
    ]

build_idk_combinations = rule(
    implementation = _build_idk_combinations_impl,
    attrs = {
        "deps": attr.label_list(cfg = cpu_api_level_transition),
    },
)
