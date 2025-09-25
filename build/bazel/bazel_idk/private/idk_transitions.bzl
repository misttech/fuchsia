# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Transitions used to build the IDK."""

visibility(["//build/bazel/bazel_idk/tests/..."])

def _cpu_api_level_transition_impl(_settings, _attr):
    return [
        {"//command_line_option:platforms": "@//build/bazel/platforms:fuchsia_platform_arm64"},
        {"//command_line_option:platforms": "@//build/bazel/platforms:fuchsia_platform_riscv64"},
        {"//command_line_option:platforms": "@//build/bazel/platforms:fuchsia_platform_x64"},
    ]

cpu_api_level_transition = transition(
    implementation = _cpu_api_level_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:platforms",
        # TODO(https://fxbug.dev/443825617): Add API level when available.
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
