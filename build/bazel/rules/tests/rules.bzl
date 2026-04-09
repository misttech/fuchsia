# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for the tests in this package."""

load(
    "//build/bazel/rules:stamp_group.bzl",
    "STAMP_GROUP_NON_DEPS_ATTRS",
    "stamp_group_impl",
)

visibility("private")

def _apply_clang_platform_api_level_impl(settings, _attr):
    _platform_api_level_copt = "-ffuchsia-api-level=4293918720"

    copt = list(settings["//command_line_option:copt"])

    # Avoid adding the same flag twice, which would create a different Bazel config.
    # This allows, for example, specifying the API level on the Bazel command line.
    if _platform_api_level_copt not in copt:
        # Set Clang's Fuchsia API level to the uint32 representation of "PLATFORM".
        copt.append(_platform_api_level_copt)

    return {"//command_line_option:copt": copt}

# Adds an entry to the `copt` build setting that defines Clang's Fuchsia
# API level as "PLATFORM".
_apply_clang_platform_api_level = transition(
    implementation = _apply_clang_platform_api_level_impl,
    inputs = ["//command_line_option:copt"],
    outputs = [
        "//command_line_option:copt",
    ],
)

# TODO(https://fxbug.dev/443766378): Remove this rule once the API level is
# defined by default.
build_with_platform_api_level = rule(
    doc = """Builds `deps` with the Fuchsia API level set to "PLATFORM".
    Currently, only C/C++ targets will have the API level set.

    This is a workaround for the fact that the API level is not yet set by a
    platform toolchain.
    """,
    implementation = stamp_group_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "List of Labels to build at the 'PLATFORM' API level.",
            mandatory = True,
            cfg = _apply_clang_platform_api_level,
        ),
    } | STAMP_GROUP_NON_DEPS_ATTRS,
)
