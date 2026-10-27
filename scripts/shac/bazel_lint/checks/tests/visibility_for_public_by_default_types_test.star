# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_visibility_for_public_by_default_types."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_visibility_for_public_by_default_types",
)
load("./base_test.star", "run_lint_test")

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#always-specify-visibility-for-types-that-are-public-by-default."

_CONFIG_SETTING_ERROR_MSG = (
    "`config_setting()` must explicitly specify `visibility` because it is public by default. " +
    _DOCS_SUFFIX
)

_EXPORTS_FILES_ERROR_MSG = (
    "`exports_files()` must explicitly specify `visibility` because it is public by default. " +
    _DOCS_SUFFIX
)

def _run_visibility_for_public_by_default_types_test(files):
    return run_lint_test(
        _bazel_visibility_for_public_by_default_types,
        "bazel_visibility_for_public_by_default_types",
        files,
    )

def test_missing_explicit_visibility():
    res = _run_visibility_for_public_by_default_types_test({
        "src/BUILD.bazel": """
config_setting(name = "my_config_setting", values = {"cpu": "x64"})
exports_files(["exported_file.txt"])
""",
        # Forwarding `**kwargs` in a macro does not exempt `config_setting()` or
        # `exports_files()` from explicitly specifying `visibility` (unlike rules
        # that default to private visibility):
        "src/defs.bzl": """
def define_config_setting(): native.config_setting(name = "my_config_setting")
def export_my_files(): native.exports_files(["exported_file.txt"])
def define_config_setting_with_kwargs(**kwargs): native.config_setting(name = "forwarded_config", **kwargs)
def export_files_with_kwargs(**kwargs): native.exports_files(["forwarded_file.txt"], **kwargs)
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _CONFIG_SETTING_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 2,
            col = 1,
            end_col = 68,
        ),
        testing.finding(
            level = "error",
            message = _EXPORTS_FILES_ERROR_MSG,
            filepath = "src/BUILD.bazel",
            line = 3,
            col = 1,
            end_col = 37,
        ),
        testing.finding(
            level = "error",
            message = _CONFIG_SETTING_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 2,
            col = 30,
            end_col = 79,
        ),
        testing.finding(
            level = "error",
            message = _EXPORTS_FILES_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 3,
            col = 24,
            end_col = 67,
        ),
        testing.finding(
            level = "error",
            message = _CONFIG_SETTING_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 4,
            col = 50,
            end_col = 108,
        ),
        testing.finding(
            level = "error",
            message = _EXPORTS_FILES_ERROR_MSG,
            filepath = "src/defs.bzl",
            line = 5,
            col = 41,
            end_col = 95,
        ),
    ))

def test_allowed_explicit_visibility():
    res = _run_visibility_for_public_by_default_types_test({
        "src/BUILD.bazel": """
config_setting(
    name = "my_config_setting",
    values = {"cpu": "x64"},
    visibility = [":__pkg__"],
)
exports_files(
    ["exported_file.txt"],
    visibility = ["//src:__subpackages__"],
)
# Other rules like `filegroup()` support `visibility` as well, but default
# to private visibility when omitted, so they do not require explicit
# `visibility`.
filegroup(
    name = "other_target_type",
    srcs = ["exported_file.txt"],
)
""",
        "src/defs.bzl": """
def my_macro():
    native.config_setting(
        name = "explicit_config_setting",
        visibility = ["//visibility:private"],
    )
    native.exports_files(
        ["explicitly_exported_file.txt"],
        visibility = [":__pkg__"],
    )
""",
    })
    asserts.eq(res.findings, ())

def test_ignored_file_types():
    res = _run_visibility_for_public_by_default_types_test({
        "MODULE.bazel": 'exports_files(["exported_from_module.txt"])\n',
        "WORKSPACE.bazel": 'exports_files(["exported_from_workspace.txt"])\n',
    })
    asserts.eq(res.findings, ())

def run_visibility_for_public_by_default_types_tests():
    """Runs all visibility_for_public_by_default_types tests."""
    test_missing_explicit_visibility()
    test_allowed_explicit_visibility()
    test_ignored_file_types()
