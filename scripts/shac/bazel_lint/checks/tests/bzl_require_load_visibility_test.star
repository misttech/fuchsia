# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_bzl_require_load_visibility."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_bzl_require_load_visibility",
)
load("./base_test.star", "run_lint_test")

_ERROR_MSG = (
    "`.bzl` files must declare top-level load `visibility(...)`. " +
    'Use `visibility("private")` if only used within the same package, ' +
    "or specify the packages or subtrees that may load this file " +
    '(e.g., `visibility(["//src/..."])`). ' +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/bzl_files#load-visibility."
)

_MISSING_VISIBILITY_CONTENT = 'FOO = "bar"\n'

def _run_bzl_require_load_visibility_test(files):
    return run_lint_test(
        _bazel_bzl_require_load_visibility,
        "bazel_bzl_require_load_visibility",
        files,
    )

def test_missing_load_visibility():
    res = _run_bzl_require_load_visibility_test({
        "src/empty.bzl": "",
        "src/missing.bzl": _MISSING_VISIBILITY_CONTENT,
        # A `visibility()` call nested inside a function is not a top-level load
        # visibility declaration:
        "src/nested_only.bzl": """
def my_macro():
    visibility("private")
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/empty.bzl",
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/missing.bzl",
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/nested_only.bzl",
        ),
    ))

def test_allowed_load_visibility():
    res = _run_bzl_require_load_visibility_test({
        "src/kwarg.bzl": 'visibility(value = "private")\n',
        "src/list.bzl": """
\"\"\"Module docstring.\"\"\"

load("//src:other.bzl", "other")

visibility(["//src/..."])

def my_macro(name):
    other(name = name)
""",
        "src/private.bzl": 'visibility("private")\n',
    })
    asserts.eq(res.findings, ())

def test_ignored_file_types():
    res = _run_bzl_require_load_visibility_test({
        "MODULE.bazel": _MISSING_VISIBILITY_CONTENT,
        "WORKSPACE.bazel": _MISSING_VISIBILITY_CONTENT,
        "src/BUILD.bazel": _MISSING_VISIBILITY_CONTENT,
    })
    asserts.eq(res.findings, ())

def run_bzl_require_load_visibility_tests():
    """Runs all bzl_require_load_visibility tests."""
    test_missing_load_visibility()
    test_allowed_load_visibility()
    test_ignored_file_types()
