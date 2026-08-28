# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Implementation of the build_flags_toolchain_instance() rule."""

load(":providers.bzl", "BuildFlagsInfo", "DefaultBuildFlagsSetInfo")

#############################################################################
#############################################################################
#####
#####    build_flags_toolchain_instance()
#####
#####    This is a Bazel toolchain type to provide different lists of
#####    default build_flags() labels, for different types of targets.
#####
#####

def _build_flags_toolchain_instance_impl(ctx):
    return [
        platform_common.ToolchainInfo(
            default_flags = DefaultBuildFlagsSetInfo(
                cxx_common_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.cxx_common_build_flags
                ],
                cxx_shared_library_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.cxx_shared_library_build_flags
                ],
                cxx_executable_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.cxx_executable_build_flags
                ],
                rust_common_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.rust_common_build_flags
                ],
                rust_shared_library_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.rust_shared_library_build_flags
                ],
                rust_executable_infos = [
                    target[BuildFlagsInfo]
                    for target in ctx.attr.rust_executable_build_flags
                ],
            ),
        ),
    ]

build_flags_toolchain_instance = rule(
    implementation = _build_flags_toolchain_instance_impl,
    doc = "A toolchain() instance target which provides lists of default build flags by target type.",
    attrs = {
        "cxx_common_build_flags": attr.label_list(
            doc = "The build_flags() targets to use by default for all C/C++ targets.",
            providers = [BuildFlagsInfo],
        ),
        "cxx_shared_library_build_flags": attr.label_list(
            doc = "Extra build_flags() targets for C/C++ shared library targets.",
            providers = [BuildFlagsInfo],
        ),
        "cxx_executable_build_flags": attr.label_list(
            doc = "Extra build_flags() targets for C/C++ executable targets.",
            providers = [BuildFlagsInfo],
        ),
        "rust_common_build_flags": attr.label_list(
            doc = "The build_flags() targets to use by default for all Rust targets.",
            providers = [BuildFlagsInfo],
        ),
        "rust_shared_library_build_flags": attr.label_list(
            doc = "Extra build_flags() targets for Rust shared library targets.",
            providers = [BuildFlagsInfo],
        ),
        "rust_executable_build_flags": attr.label_list(
            doc = "Extra build_flags() targets for Rust executable targets.",
            providers = [BuildFlagsInfo],
        ),
    },
)
