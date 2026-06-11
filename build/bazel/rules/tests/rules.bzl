# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for the tests in this package."""

load(
    "//build/bazel/rules:stamp_group.bzl",
    "STAMP_GROUP_NON_DEPS_ATTRS",
    "stamp_group_impl",
)

visibility(["//build/bazel/bazel_idk/tests/..."])

# TODO(https://fxbug.dev/521882370): Remove this rule now that the API level
# is handled by the toolchain.
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
        ),
    } | STAMP_GROUP_NON_DEPS_ATTRS,
)

build_for_host = rule(
    doc = "Builds `deps` using the host configuration.",
    implementation = stamp_group_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "List of Labels to build using the host configuration.",
            mandatory = True,
            cfg = "exec",
        ),
    } | STAMP_GROUP_NON_DEPS_ATTRS,
)
