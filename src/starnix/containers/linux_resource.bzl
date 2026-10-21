# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for packaging a Linux binary as a Fuchsia package resource."""

load("@fuchsia_build_info//:args.bzl", "target_cpu")
load("@fuchsia_rules_common//:utils.bzl", "make_resource_struct")
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "package_resources_providers",
)

visibility(["//src/starnix/containers/..."])

def _target_linux_transition_impl(settings, _attr):
    current_platforms = settings["//command_line_option:platforms"]
    if len(current_platforms) == 1:
        platform = str(current_platforms[0])
        if platform.endswith("_x64"):
            return {"//command_line_option:platforms": ["@//build/bazel/platforms:linux_x64"]}
        if platform.endswith("_arm64"):
            return {"//command_line_option:platforms": ["@//build/bazel/platforms:linux_arm64"]}
    if target_cpu in ("x64", "arm64"):
        return {"//command_line_option:platforms": ["@//build/bazel/platforms:linux_" + target_cpu]}
    return {"//command_line_option:platforms": current_platforms}

_target_linux_transition = transition(
    implementation = _target_linux_transition_impl,
    inputs = ["//command_line_option:platforms"],
    outputs = ["//command_line_option:platforms"],
)

def _linux_resource_impl(ctx):
    if len(ctx.attr.dest) == 0:
        fail("dest must not be an empty string")

    return package_resources_providers(ctx, [make_resource_struct(
        src = ctx.files.src[0],
        dest = ctx.attr.dest,
    )])

linux_resource = rule(
    doc = "Declares a Linux binary resource to be included in a Fuchsia package.",
    implementation = _linux_resource_impl,
    attrs = {
        "src": attr.label(
            doc = "The Linux target to include in the package.",
            mandatory = True,
            allow_single_file = True,
            cfg = _target_linux_transition,
        ),
        "dest": attr.string(
            doc = "The path where this will be installed in the package.",
            mandatory = True,
        ),
    },
)
