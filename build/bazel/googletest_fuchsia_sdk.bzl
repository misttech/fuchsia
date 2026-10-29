# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Repository rule routing @com_google_googletest's @fuchsia_sdk targets."""

def _googletest_fuchsia_sdk_impl(repo_ctx):
    repo_ctx.file("pkg/fdio/BUILD.bazel", """
alias(
    name = "fdio",
    actual = select({
        "@//build/bazel/platforms:is_fuchsia_platform": "@//sdk/lib/fdio",
        "@//build/bazel/platforms:is_fuchsia_with_sdk_rules": "@fuchsia_sdk//pkg/fdio",
    }),
    visibility = ["//visibility:public"],
)
""")

    # GoogleTest 1.17.0 lists `pkg/syslog` as a dependency in its BUILD.bazel
    # (removed upstream in newer GoogleTest), even though it never calls into it
    # and logs through `fdio` on Fuchsia. Supply an empty library until the
    # in-tree GoogleTest version is rolled past 1.17.0.
    repo_ctx.file("pkg/syslog/BUILD.bazel", """
load("@rules_cc//cc:cc_library.bzl", "cc_library")

cc_library(
    name = "syslog",
    # Propagate the gtest runner shard to any test binary that links against
    # @com_google_googletest//:gtest (which depends on @fuchsia_sdk//pkg/syslog)
    # in fuchsia_platform builds.
    aspect_hints = select({
        "@//build/bazel/platforms:is_fuchsia_platform": ["@//src/sys/test_runners/gtest:default"],
        "@//build/bazel/platforms:is_fuchsia_with_sdk_rules": [],
    }),
    visibility = ["//visibility:public"],
)
""")
    repo_ctx.file("pkg/zx/BUILD.bazel", """
alias(
    name = "zx",
    actual = select({
        "@//build/bazel/platforms:is_fuchsia_platform": "@//zircon/system/ulib/zx",
        "@//build/bazel/platforms:is_fuchsia_with_sdk_rules": "@fuchsia_sdk//pkg/zx",
    }),
    visibility = ["//visibility:public"],
)
""")

googletest_fuchsia_sdk = repository_rule(
    doc = "Routes @com_google_googletest's @fuchsia_sdk dependencies to in-tree platform targets in fuchsia_platform builds and to @fuchsia_sdk in SDK builds.",
    implementation = _googletest_fuchsia_sdk_impl,
)
