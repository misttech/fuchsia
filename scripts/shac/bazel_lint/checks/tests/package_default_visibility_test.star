# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_package_default_visibility."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_package_default_visibility",
)

_BROAD_ERROR_MSG = (
    "Do not use `default_visibility` in `package()` for broad visibility " +
    "(such as `//visibility:public` or top-level `__subpackages__`). " +
    "Specify `visibility` on individual targets instead. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#avoid-package-default-visibility."
)

_VENDOR_ERROR_MSG = (
    "Do not include vendor subpaths in `default_visibility`. " +
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, ' +
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` ' +
    "are permitted in public Fuchsia code. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-reference-vendor-subpaths."
)

_NARROW_WARNING_MSG = (
    "Prefer specifying `visibility` on individual targets rather than setting " +
    "`default_visibility` in `package()`. `default_visibility` should only be " +
    "used when every target in the file (with no private helper targets) " +
    "shares the same narrow visibility. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#avoid-package-default-visibility."
)

_EMPTY_ALLOWLIST = testing.file(
    content = json.encode({"entries": []}),
    affected = False,
)

def test_broad_package_default_visibility_errors():
    res = testing.run(
        _bazel_package_default_visibility,
        files = {
            "scripts/shac/bazel_lint/allowlists/bazel_package_default_visibility.json": _EMPTY_ALLOWLIST,
            "broad_public/BUILD.bazel": 'package(default_visibility = ["//visibility:public"])\n',
            "broad_root/BUILD.bazel": 'package(default_visibility = ["//:__subpackages__"])\n',
            "broad_top/BUILD.bazel": 'package(default_visibility = ["//src:__subpackages__"])\n',
        },
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _BROAD_ERROR_MSG,
            filepath = "broad_public/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 53,
        ),
        testing.finding(
            level = "error",
            message = _BROAD_ERROR_MSG,
            filepath = "broad_root/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _BROAD_ERROR_MSG,
            filepath = "broad_top/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 55,
        ),
    ))

def test_narrow_package_default_visibility_warnings():
    res = testing.run(
        _bazel_package_default_visibility,
        files = {
            "scripts/shac/bazel_lint/allowlists/bazel_package_default_visibility.json": _EMPTY_ALLOWLIST,
            "narrow_pkg/BUILD.bazel": 'package(default_visibility = ["//tools/mdlint:__pkg__"])\n',
            "narrow_sub/BUILD.bazel": 'package(default_visibility = ["//src/lib:__subpackages__"])\n',
            "narrow_top_pkg/BUILD.bazel": 'package(default_visibility = ["//src:__pkg__"])\n',
            "vendor_mixed/BUILD.bazel": 'package(default_visibility = ["//vendor:__subpackages__", "//src/lib:__subpackages__"])\n',
            "vendor_repo_subpackages/BUILD.bazel": 'package(default_visibility = ["//vendor/google:__subpackages__"])\n',
            "vendor_subpackages/BUILD.bazel": 'package(default_visibility = ["//vendor:__subpackages__"])\n',
        },
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "narrow_pkg/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 56,
        ),
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "narrow_sub/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 59,
        ),
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "narrow_top_pkg/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 47,
        ),
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "vendor_mixed/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 87,
        ),
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "vendor_repo_subpackages/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 65,
        ),
        testing.finding(
            level = "warning",
            message = _NARROW_WARNING_MSG,
            filepath = "vendor_subpackages/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 58,
        ),
    ))

def test_disallowed_vendor_package_default_visibility():
    res = testing.run(
        _bazel_package_default_visibility,
        files = {
            "scripts/shac/bazel_lint/allowlists/bazel_package_default_visibility.json": _EMPTY_ALLOWLIST,
            "disallowed_vendor_pkg/BUILD.bazel": 'package(default_visibility = ["//vendor/foo:__pkg__"])\n',
            "disallowed_vendor_relative/BUILD.bazel": 'package(default_visibility = ["vendor/foo:__pkg__"])\n',
            "disallowed_vendor_sub/BUILD.bazel": 'package(default_visibility = ["//vendor/google/foo:__subpackages__"])\n',
            "disallowed_vendor_typo/BUILD.bazel": 'package(default_visibility = ["//vendorfoo:__pkg__"])\n',
        },
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "disallowed_vendor_pkg/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 54,
        ),
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "disallowed_vendor_relative/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "disallowed_vendor_sub/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 69,
        ),
        testing.finding(
            level = "error",
            message = _VENDOR_ERROR_MSG,
            filepath = "disallowed_vendor_typo/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 53,
        ),
    ))

def test_allowed_package_default_visibility():
    res = testing.run(
        _bazel_package_default_visibility,
        files = {
            "clean/BUILD.bazel": 'package(default_applicable_licenses = ["//:license"])\n',
            "no_package/BUILD.bazel": 'filegroup(name = "foo")\n',
        },
    )
    asserts.eq(res.findings, ())

def test_allowlisted_prefix():
    allowlist = testing.file(
        content = json.encode({
            "entries": [
                {
                    "bug": "https://fxbug.dev/570203009",
                    "path_prefix": "allowlisted_dir/",
                },
            ],
        }),
        affected = False,
    )
    res = testing.run(
        _bazel_package_default_visibility,
        files = {
            "scripts/shac/bazel_lint/allowlists/bazel_package_default_visibility.json": allowlist,
            "allowlisted_dir/foo/BUILD.bazel": 'package(default_visibility = ["//visibility:public"])\n',
        },
    )
    asserts.eq(res.findings, ())

def run_package_default_visibility_tests():
    """Runs all package_default_visibility tests."""
    test_broad_package_default_visibility_errors()
    test_narrow_package_default_visibility_warnings()
    test_disallowed_vendor_package_default_visibility()
    test_allowed_package_default_visibility()
    test_allowlisted_prefix()
