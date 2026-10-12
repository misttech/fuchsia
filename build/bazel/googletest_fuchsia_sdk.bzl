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

    # GoogleTest lists `pkg/syslog` as a dependency but never calls into it: on
    # Fuchsia it logs through `fdio`. There is no in-tree equivalent of the SDK's
    # `pkg/syslog` C wrapper to point at, so supply an empty library instead. If
    # a future GoogleTest roll starts calling `fx_log*`, this is what to fix.
    repo_ctx.file("pkg/syslog/BUILD.bazel", """
load("@rules_cc//cc:cc_library.bzl", "cc_library")

cc_library(
    name = "syslog_noop",
    # Propagate the gtest runner shard to any test binary that links against
    # @com_google_googletest//:gtest (which depends on @fuchsia_sdk//pkg/syslog).
    aspect_hints = ["@//src/sys/test_runners/gtest:default"],
)

alias(
    name = "syslog",
    actual = select({
        "@//build/bazel/platforms:is_fuchsia_platform": ":syslog_noop",
        "@//build/bazel/platforms:is_fuchsia_with_sdk_rules": "@fuchsia_sdk//pkg/syslog",
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
