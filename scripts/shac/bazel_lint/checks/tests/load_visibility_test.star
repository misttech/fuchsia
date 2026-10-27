# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_load_visibility."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_load_visibility",
)
load("./base_test.star", "run_lint_test")

_PUBLIC_ERROR_MSG = (
    'Do not use `visibility("public")`. ' +
    "Specify the packages or subtrees that may load this file " +
    '(e.g., `visibility(["//src/..."])`), or `visibility("private")` ' +
    "if only used within the same package. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-public-visibility."
)

_BROAD_WARNING_MSG = (
    'Avoid broad `visibility(["//..."])` in `.bzl` files outside ' +
    "`//build/bazel/rules/`. Prefer scoping load visibility to specific " +
    'packages or subtrees (e.g., `visibility(["//src/..."])`). ' +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/bzl_files#load-visibility."
)

def _run_load_visibility_test(files):
    return run_lint_test(_bazel_load_visibility, "bazel_load_visibility", files)

def test_disallowed_public_visibility():
    res = _run_load_visibility_test({
        "src/public_kw.bzl": 'visibility(value = "public")\n',
        "src/public_kw_list.bzl": 'visibility(value = ["public"])\n',
        "src/public_list.bzl": 'visibility(["public"])\n',
        "src/public_str.bzl": 'visibility("public")\n',
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _PUBLIC_ERROR_MSG,
            filepath = "src/public_kw.bzl",
            line = 1,
            col = 21,
            end_col = 27,
        ),
        testing.finding(
            level = "error",
            message = _PUBLIC_ERROR_MSG,
            filepath = "src/public_kw_list.bzl",
            line = 1,
            col = 22,
            end_col = 28,
        ),
        testing.finding(
            level = "error",
            message = _PUBLIC_ERROR_MSG,
            filepath = "src/public_list.bzl",
            line = 1,
            col = 14,
            end_col = 20,
        ),
        testing.finding(
            level = "error",
            message = _PUBLIC_ERROR_MSG,
            filepath = "src/public_str.bzl",
            line = 1,
            col = 13,
            end_col = 19,
        ),
    ))

def test_broad_load_visibility_warnings():
    res = _run_load_visibility_test({
        "src/broad_kw.bzl": 'visibility(value = "//...")\n',
        "src/broad_list.bzl": 'visibility(["//...", "//vendor/..."])\n',
        "src/broad_str.bzl": 'visibility("//...")\n',
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = _BROAD_WARNING_MSG,
            filepath = "src/broad_kw.bzl",
            line = 1,
            col = 21,
            end_col = 26,
        ),
        testing.finding(
            level = "warning",
            message = _BROAD_WARNING_MSG,
            filepath = "src/broad_list.bzl",
            line = 1,
            col = 14,
            end_col = 19,
        ),
        testing.finding(
            level = "warning",
            message = _BROAD_WARNING_MSG,
            filepath = "src/broad_str.bzl",
            line = 1,
            col = 13,
            end_col = 18,
        ),
    ))

def test_allowed_load_visibility():
    res = _run_load_visibility_test({
        "src/allowed_vendor.bzl": 'visibility(["//vendor/..."])\n',
        "src/mixed_allowed_vendor.bzl": 'visibility(["//vendor/...", "//src/..."])\n',
        "src/private.bzl": 'visibility("private")\n',
        "src/scoped.bzl": 'visibility(["//src/..."])\n',
    })
    asserts.eq(res.findings, ())

def test_build_bazel_rules_exemption():
    res = _run_load_visibility_test({
        "build/bazel/rules/allowed_broad.bzl": 'visibility(["//..."])\n',
        "build/bazel/rules/disallowed_public.bzl": 'visibility("public")\n',
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _PUBLIC_ERROR_MSG,
            filepath = "build/bazel/rules/disallowed_public.bzl",
            line = 1,
            col = 13,
            end_col = 19,
        ),
    ))

def test_policy_exempt_sdk_directories():
    res = _run_load_visibility_test({
        "build/bazel_sdk/bazel_rules_fuchsia/foo/defs.bzl": 'visibility("public")\n',
        "build/bazel_sdk/fuchsia_rules_common/foo/defs.bzl": 'visibility("//...")\n',
    })
    asserts.eq(res.findings, ())

def test_ignored_calls():
    res = _run_load_visibility_test({
        # Target visibility is checked by bazel_target_visibility, not bazel_load_visibility.
        "src/target.bzl": 'filegroup(name = "foo", visibility = ["//visibility:public"])\n',
        # Nested visibility() calls inside functions are not top-level load visibility declarations.
        "src/nested_call.bzl": 'def foo():\n    visibility("public")\n',
    })
    asserts.eq(res.findings, ())

def run_load_visibility_tests():
    """Runs all load_visibility tests."""
    test_disallowed_public_visibility()
    test_broad_load_visibility_warnings()
    test_allowed_load_visibility()
    test_build_bazel_rules_exemption()
    test_policy_exempt_sdk_directories()
    test_ignored_calls()
