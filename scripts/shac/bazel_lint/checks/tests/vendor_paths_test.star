# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_vendor_paths."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_vendor_paths",
)
load("./base_test.star", "run_lint_test")

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-reference-vendor-subpaths."

_ERROR_MSG = (
    "Do not reference vendor subpaths in public Fuchsia code. " +
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, ' +
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` ' +
    "are permitted for target visibility. " +
    _DOCS_SUFFIX
)

_VIOLATION_TARGET = 'filegroup(name = "bad_vendor", visibility = ["//vendor/google/foo:__pkg__"])\n'

def _run_vendor_paths_test(files):
    return run_lint_test(_bazel_vendor_paths, "bazel_vendor_paths", files)

def test_disallowed_vendor_load_visibility():
    res = _run_vendor_paths_test({
        "src/bad_subpath.bzl": 'visibility(["//vendor/google/tools/subpath/..."])\n',
        "src/bad_vendor.bzl": 'visibility(["//vendor/google/foo/..."])\n',
        "src/relative_vendor.bzl": 'visibility(["vendor/foo/..."])\n',
        "src/typo_relative_vendor.bzl": 'visibility(["vendorfoo/..."])\n',
        "src/typo_vendor.bzl": 'visibility(["//vendorfoo/..."])\n',
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/bad_subpath.bzl",
            line = 1,
            col = 14,
            end_col = 47,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/bad_vendor.bzl",
            line = 1,
            col = 14,
            end_col = 37,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/relative_vendor.bzl",
            line = 1,
            col = 14,
            end_col = 28,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/typo_relative_vendor.bzl",
            line = 1,
            col = 14,
            end_col = 27,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/typo_vendor.bzl",
            line = 1,
            col = 14,
            end_col = 29,
        ),
    ))

def test_disallowed_vendor_package_groups():
    res = _run_vendor_paths_test({
        "src/BUILD.bazel": """
package_group(name = "pg", packages = ["//vendor/google/foo/..."])
package_group(name = "pg_typo_rel", packages = ["vendorfoo/..."])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 41,
            end_col = 64,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 50,
            end_col = 63,
        ),
    ))

def test_disallowed_vendor_default_visibility():
    res = _run_vendor_paths_test({
        "src/default_vis/BUILD.bazel": """
package(default_visibility = ["//vendor/foo:__pkg__"])
package(default_visibility = ["vendor/foo:__pkg__"])
package(default_visibility = ["vendorfoo:__pkg__"])
package(default_visibility = ["//vendor/google/foo:__subpackages__"])
package(default_visibility = ["//vendorfoo:__pkg__"])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/default_vis/BUILD.bazel",
            line = 2,
            col = 32,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/default_vis/BUILD.bazel",
            line = 3,
            col = 32,
            end_col = 50,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/default_vis/BUILD.bazel",
            line = 4,
            col = 32,
            end_col = 49,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/default_vis/BUILD.bazel",
            line = 5,
            col = 32,
            end_col = 67,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/default_vis/BUILD.bazel",
            line = 6,
            col = 32,
            end_col = 51,
        ),
    ))

def test_disallowed_vendor_target_visibility():
    res = _run_vendor_paths_test({
        "src/target_vis/BUILD.bazel": """
filegroup(name = "bad_vendor", visibility = ["//vendor/google/foo:__pkg__"])
filegroup(name = "bad_subpath", visibility = ["//vendor/google/tools/subpath:__pkg__"])
filegroup(name = "relative_vendor", visibility = ["vendor/foo:__pkg__"])
filegroup(name = "typo_relative_vendor", visibility = ["vendorfoo:__pkg__"])
filegroup(name = "typo_vendor", visibility = ["//vendorfoo:__pkg__"])
filegroup(name = "canonical_vendor", visibility = ["@@//vendor/google/foo:__pkg__"])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 2,
            col = 47,
            end_col = 74,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 3,
            col = 48,
            end_col = 85,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 4,
            col = 52,
            end_col = 70,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 5,
            col = 57,
            end_col = 74,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 6,
            col = 48,
            end_col = 67,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/target_vis/BUILD.bazel",
            line = 7,
            col = 53,
            end_col = 82,
        ),
    ))

def test_disallowed_general_vendor_paths():
    res = _run_vendor_paths_test({
        "MODULE.bazel": _VIOLATION_TARGET,
        "WORKSPACE.bazel": _VIOLATION_TARGET,
        "src/BUILD.bazel": """
load("//vendor/google/foo:defs.bzl", "foo_macro")
filegroup(name = "vendor_dep", deps = ["//vendor/google/foo:lib"])
filegroup(name = "relative_dep", deps = ["vendor/foo:bar"])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "MODULE.bazel",
            line = 1,
            col = 47,
            end_col = 74,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "WORKSPACE.bazel",
            line = 1,
            col = 47,
            end_col = 74,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 7,
            end_col = 35,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 41,
            end_col = 64,
        ),
        testing.finding(
            level = "error",
            message = _ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 4,
            col = 43,
            end_col = 57,
        ),
    ))

def test_allowed_vendor_paths():
    res = _run_vendor_paths_test({
        "build/bazel/toplevel.MODULE.bazel": """
fuchsia_sdk_ext.local(
    visibility_templates = {
        "hlcpp": ["@@//vendor/*/build:hlcpp_allowlist"],
    },
)
""",
        "src/BUILD.bazel": """
package(default_visibility = ["//vendor:__subpackages__", "//vendor/google:__pkg__"])

package_group(
    name = "allowed_vendor_pg",
    packages = [
        "//vendor",
        "//vendor/...",
        "//vendor/google",
        "//vendor/google/...",
    ],
)

ALLOWED_VIS = [
    "//vendor:__pkg__",
    "//vendor:__subpackages__",
    "//vendor/google:__pkg__",
    "//vendor/google:__subpackages__",
]

filegroup(
    name = "allowed_vendor_target",
    srcs = ["vendor.fidl", "vendor_specific.fidl", "vendorfoo.fidl"],
    visibility = [
        "//vendor:__pkg__",
        "//vendor:__subpackages__",
        "//vendor/google:__pkg__",
        "@@//vendor/google:__subpackages__",
    ],
)

board_config(
    name = "board",
    hardware_info = {"vendor_id": "0x0000", "vendor": "some_vendor"},
    description = "vendor specific configuration",
)
""",
        "src/defs.bzl": '''
"""Module docstring referencing //vendor/google/foo:target."""

visibility([
    "//vendor",
    "//vendor/...",
    "//vendor/google",
    "//vendor/google/...",
])

def my_macro(package_path):
    if package_path == "//vendor/google/foo":
        fail("Disallowed //vendor/google/foo")
    if package_path.startswith("//vendor/"):
        pass

my_rule = rule(
    implementation = _impl,
    attrs = {
        "vendor": attr.label(doc = "Example //vendor/google/foo:target"),
    },
    doc = "Rule doc //vendor/google/foo:target",
)
''',
    })
    asserts.eq(res.findings, ())

def test_policy_exempt_vendor_directory():
    res = _run_vendor_paths_test({
        "vendor/google/foo/BUILD.bazel": _VIOLATION_TARGET,
        "vendor/foo/bar/BUILD.bazel": _VIOLATION_TARGET,
    })
    asserts.eq(res.findings, ())

def run_vendor_paths_tests():
    """Runs all vendor_paths tests."""
    test_disallowed_vendor_load_visibility()
    test_disallowed_vendor_package_groups()
    test_disallowed_vendor_default_visibility()
    test_disallowed_vendor_target_visibility()
    test_disallowed_general_vendor_paths()
    test_allowed_vendor_paths()
    test_policy_exempt_vendor_directory()
