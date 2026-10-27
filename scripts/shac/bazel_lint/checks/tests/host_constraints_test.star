# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_host_constraints."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_host_constraints",
)
load("./base_test.star", "run_lint_test")

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#target-compatible-with-host-targets."

_LOAD_ERROR_MSG = (
    "Do not load `HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl`; " +
    "load `HOST_OS_CONSTRAINTS` from `//build/bazel/platforms:constraints.bzl` instead. " +
    _DOCS_SUFFIX
)

_USAGE_ERROR_MSG = (
    "Do not use `HOST_CONSTRAINTS` in `target_compatible_with`; " +
    "use `HOST_OS_CONSTRAINTS` from `//build/bazel/platforms:constraints.bzl` instead. " +
    _DOCS_SUFFIX
)

_VIOLATION_BUILD = """
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
filegroup(name = "host_tool", target_compatible_with = HOST_CONSTRAINTS)
"""

def _run_host_constraints_test(files):
    return run_lint_test(_bazel_host_constraints, "bazel_host_constraints", files)

def test_disallowed_host_constraints():
    res = _run_host_constraints_test({
        "MODULE.bazel": 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n',
        "WORKSPACE.bazel": 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n',
        "src/BUILD.bazel": """
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
filegroup(name = "direct_host_constraints", target_compatible_with = HOST_CONSTRAINTS)
filegroup(name = "select_host_constraints", target_compatible_with = select({"//conditions:default": HOST_CONSTRAINTS}))
""",
        "src/defs.bzl": """
load("@@platforms//host:constraints.bzl", _HC = "HOST_CONSTRAINTS")
def aliased_macro(): native.filegroup(name = "aliased_target", target_compatible_with = _HC)
def direct_macro(): native.filegroup(name = "direct_target", target_compatible_with = HOST_CONSTRAINTS)
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _LOAD_ERROR_MSG,
            filepath = "MODULE.bazel",
            line = 1,
            col = 42,
            end_col = 60,
        ),
        testing.finding(
            level = "error",
            message = _LOAD_ERROR_MSG,
            filepath = "WORKSPACE.bazel",
            line = 1,
            col = 42,
            end_col = 60,
        ),
        testing.finding(
            level = "error",
            message = _LOAD_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 42,
            end_col = 60,
        ),
        testing.finding(
            level = "error",
            message = _USAGE_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 70,
            end_col = 86,
        ),
        testing.finding(
            level = "error",
            message = _USAGE_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 102,
            end_col = 118,
        ),
        testing.finding(
            level = "error",
            message = _LOAD_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 2,
            col = 43,
            end_col = 67,
        ),
        testing.finding(
            level = "error",
            message = _USAGE_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 3,
            col = 89,
            end_col = 92,
        ),
        testing.finding(
            level = "error",
            message = _USAGE_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 4,
            col = 87,
            end_col = 103,
        ),
    ))

def test_allowed_host_constraints():
    res = _run_host_constraints_test({
        "src/BUILD.bazel": """
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")

filegroup(
    name = "host_tool",
    target_compatible_with = HOST_OS_CONSTRAINTS,
)

filegroup(
    name = "fuchsia_target",
    target_compatible_with = ["@platforms//os:fuchsia"],
)
""",
        "src/defs.bzl": """
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")

def my_macro(name, target_compatible_with = HOST_OS_CONSTRAINTS, **kwargs):
    if target_compatible_with == "HOST_CONSTRAINTS":
        fail("Do not use HOST_CONSTRAINTS")
    native.filegroup(
        name = name,
        target_compatible_with = target_compatible_with,
        **kwargs
    )
""",
    })
    asserts.eq(res.findings, ())

def test_policy_exempt_paths():
    res = _run_host_constraints_test({
        "build/bazel/update-rustc-third-party/BUILD.bazel": _VIOLATION_BUILD,
        "build/bazel/update-rustc-third-party/subpkg/BUILD.bazel": _VIOLATION_BUILD,
        "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/tools/BUILD.bazel": _VIOLATION_BUILD,
        "build/bazel_sdk/fuchsia_rules_common/packages/BUILD.bazel": _VIOLATION_BUILD,
    })
    asserts.eq(res.findings, ())

def run_host_constraints_tests():
    """Runs all host_constraints tests."""
    test_disallowed_host_constraints()
    test_allowed_host_constraints()
    test_policy_exempt_paths()
