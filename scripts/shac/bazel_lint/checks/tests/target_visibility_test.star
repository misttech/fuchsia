# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_target_visibility."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_target_visibility",
)
load("./base_test.star", "run_lint_test")

_ERROR_MSG = (
    'Do not use `"//visibility:public"`. ' +
    "Scope visibility to specific packages or subtrees " +
    '(e.g., `"//src:__subpackages__"`), or `"//:__subpackages__"` ' +
    "if truly needed across the codebase. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-public-visibility."
)

_VENDOR_ERROR_MSG = (
    "Do not include vendor subpaths in target visibility. " +
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, ' +
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` ' +
    "are permitted in public Fuchsia code. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-reference-vendor-subpaths."
)

def _run_target_visibility_test(files):
    return run_lint_test(
        _bazel_target_visibility,
        "bazel_target_visibility",
        files,
    )

def test_disallowed_public_target_visibility():
    res = _run_target_visibility_test({
        "build/bazel_sdk/tests/foo/BUILD.bazel": 'filegroup(name = "test", visibility = ["//visibility:public"])\n',
        "src/BUILD.bazel": """
filegroup(
    name = "flagged_target",
    visibility = ["//visibility:public"],
)
""",
        "src/defs.bzl": """
def my_macro():
    native.filegroup(
        name = "inner",
        visibility = ["//visibility:public"],
    )
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "build/bazel_sdk/tests/foo/BUILD.bazel",
            line = 1,
            col = 41,
            end_col = 60,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 20,
            end_col = 39,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 5,
            col = 24,
            end_col = 43,
        ),
    ))

def test_disallowed_vendor_target_visibility():
    res = _run_target_visibility_test({
        "src/BUILD.bazel": """
filegroup(
    name = "bad_vendor_target",
    visibility = ["//vendor/google/secret:__pkg__"],
)
filegroup(
    name = "relative_vendor_target",
    visibility = ["vendor/foo:__pkg__"],
)
filegroup(
    name = "typo_vendor_target",
    visibility = ["//vendorfoo:__pkg__"],
)
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 20,
            end_col = 50,
        ),
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 8,
            col = 20,
            end_col = 38,
        ),
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 12,
            col = 20,
            end_col = 39,
        ),
    ))

def test_allowed_target_visibility():
    res = _run_target_visibility_test({
        "src/BUILD.bazel": """
filegroup(
    name = "allowed_vendor_target",
    visibility = ["//vendor/google:__subpackages__"],
)
filegroup(
    name = "mixed_allowed_vendor_target",
    visibility = [
        "//vendor:__subpackages__",
        "//src:__subpackages__",
    ],
)
filegroup(
    name = "scoped_target",
    visibility = ["//src:__subpackages__"],
)
""",
        "src/defs.bzl": """
def my_macro(vis):
    if vis == "//visibility:public":
        fail("Do not pass //visibility:public")
""",
    })
    asserts.eq(res.findings, ())

def test_policy_exempt_directories():
    res = _run_target_visibility_test({
        "build/bazel_sdk/bazel_rules_fuchsia/foo/BUILD.bazel": 'filegroup(name = "sdk", visibility = ["//visibility:public"])\n',
        "build/bazel_sdk/fuchsia_rules_common/foo/defs.bzl": 'VIS = ["//visibility:public"]\n',
        "build/config/foo/BUILD.bazel": 'filegroup(name = "flags", visibility = ["//visibility:public"])\n',
    })
    asserts.eq(res.findings, ())

def run_target_visibility_tests():
    """Runs all target_visibility tests."""
    test_disallowed_public_target_visibility()
    test_disallowed_vendor_target_visibility()
    test_allowed_target_visibility()
    test_policy_exempt_directories()
