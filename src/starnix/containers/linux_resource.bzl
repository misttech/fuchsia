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

# The CPUs that have a //build/bazel/platforms:linux_starnix_${cpu} platform
# for a Fuchsia target CPU, using Fuchsia CPU names.
_LINUX_CPUS = ("x64", "arm64", "riscv64")

def _linux_platforms(cpu):
    return {"//command_line_option:platforms": ["@//build/bazel/platforms:linux_starnix_" + cpu]}

def _target_linux_transition_impl(settings, _attr):
    current_platforms = settings["//command_line_option:platforms"]
    if len(current_platforms) == 1:
        platform = str(current_platforms[0])
        for cpu in _LINUX_CPUS:
            if platform.endswith("_" + cpu):
                return _linux_platforms(cpu)
    if target_cpu in _LINUX_CPUS:
        return _linux_platforms(target_cpu)
    return {"//command_line_option:platforms": current_platforms}

_target_linux_transition = transition(
    implementation = _target_linux_transition_impl,
    inputs = ["//command_line_option:platforms"],
    outputs = ["//command_line_option:platforms"],
)

def _arch32_linux_transition_impl(_settings, _attr):
    return _linux_platforms("arm")

_arch32_linux_transition = transition(
    implementation = _arch32_linux_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _linux_resource_impl(ctx):
    if len(ctx.attr.dest) == 0:
        fail("dest must not be an empty string")

    resources = [make_resource_struct(
        src = ctx.files.src[0],
        dest = ctx.attr.dest,
    )]

    if ctx.attr.arch32_src:
        if not ctx.target_platform_has_constraint(ctx.attr._arm64[platform_common.ConstraintValueInfo]):
            fail("arch32_src is only supported when building for arm64")
        if len(ctx.attr.arch32_dest) == 0:
            fail("arch32_dest must be set when arch32_src is set")
        resources.append(make_resource_struct(
            src = ctx.files.arch32_src[0],
            dest = ctx.attr.arch32_dest,
        ))

    return package_resources_providers(ctx, resources)

linux_resource = rule(
    doc = """Declares a Linux binary resource to be included in a Fuchsia package.

    `src` is built for the Starnix Linux platform with the same CPU as the
    current Fuchsia target (//build/bazel/platforms:linux_starnix_${cpu}).

    On arm64, `arch32_src` can additionally be built for the 32-bit arm
    Starnix Linux platform (//build/bazel/platforms:linux_starnix_arm), for
    Starnix's arch32 support.""",
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
        "arch32_src": attr.label(
            doc = """Optional Linux target to build for 32-bit arm and include in
            the package. Only supported for arm64, so use a select() to
            set it.""",
            allow_single_file = True,
            cfg = _arch32_linux_transition,
        ),
        "arch32_dest": attr.string(
            doc = "The path where arch32_src will be installed in the package.",
        ),
        "_arm64": attr.label(
            default = "@platforms//cpu:arm64",
            providers = [platform_common.ConstraintValueInfo],
        ),
    },
)
