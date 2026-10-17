# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_disallowed_workspace_root_package_labels."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_disallowed_workspace_root_package_labels",
)

_DOCS_SUFFIX = "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#prohibited-label-patterns."

def test_disallowed_workspace_root_package_labels():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "foo/BUILD.bazel": """
load("//:foo/bar.bzl", "symbol")
src = '//:bar/baz'
my_rule(url = "https://example.com/#readme", dep = "//:baz/qux")
my_rule(url = 'https://example.com/#doc', tool = '//:tools/helper')
my_rule(val = 'foo "#" bar', extra = "//:extra/target")
colon_label = "//:foo/bar:baz"
slash_label = "//:foo/bar/baz"
""",
        },
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:foo/bar.bzl`. " +
                "Use `//foo:bar.bzl` instead, and add a `BUILD.bazel` file to `//foo` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 2,
            col = 7,
            end_col = 21,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:bar/baz`. " +
                "Use `//bar:baz` instead, and add a `BUILD.bazel` file to `//bar` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 3,
            col = 8,
            end_col = 18,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:baz/qux`. " +
                "Use `//baz:qux` instead, and add a `BUILD.bazel` file to `//baz` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 4,
            col = 53,
            end_col = 63,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:tools/helper`. " +
                "Use `//tools:helper` instead, and add a `BUILD.bazel` file to `//tools` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 5,
            col = 51,
            end_col = 66,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:extra/target`. " +
                "Use `//extra:target` instead, and add a `BUILD.bazel` file to `//extra` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 6,
            col = 39,
            end_col = 54,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:foo/bar:baz`. " +
                "Use `//foo/bar:baz` instead, and add a `BUILD.bazel` file to `//foo/bar` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 7,
            col = 16,
            end_col = 30,
        ),
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:foo/bar/baz`. " +
                "Use `//foo/bar:baz` instead, and add a `BUILD.bazel` file to `//foo/bar` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "foo/BUILD.bazel",
            line = 8,
            col = 16,
            end_col = 30,
        ),
    ))

def test_allowed_labels_in_build_files():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "foo/BUILD.bazel": """
# Comment line //:foo/bar/baz
normal_target = "//src/lib:lib"
valid_root = "//:license"
no_slash_colon = "//:foo:bar"
external_ref = "@prebuilt_clang//:bin/clang"
my_rule(url = "https://example.com/#readme") # dep = "//:skipped/in/comment"
my_rule(val = 'foo "#" bar')
""",
        },
    )
    asserts.eq(res.findings, ())

def test_policy_exempt_workspace_root_exports():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "exports/BUILD.bazel": """
prebuilt_clang = "//:prebuilt/third_party/clang/bin/clang"
jiri_snapshot = "//:fuchsia_build_generated/jiri_snapshot.xml"
milestone = "//:integration/MILESTONE"
""",
        },
    )
    asserts.eq(res.findings, ())

def test_policy_exempt_external_repository_files():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/repository_utils.bzl": 'tool = "//:bin/clang"\n',
            "build/bazel/toolchains/rust/rust.BUILD.bazel": 'tool = "//:bin/rustc"\n',
            "build/bazel/update-rustc-third-party/BUILD.bazel": 'cargo = "//:rust_toolchain/bin/cargo"\n',
            "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/workspace/sdk_templates/fuchsia_sdk.BUILD.bazel": """
manifest = "//:meta/manifest.json"
sysroot = "//:arch/x64/sysroot"
""",
        },
    )
    asserts.eq(res.findings, ())

def test_disallowed_external_repo_label_in_regular_build_file():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "src/BUILD.bazel": 'tool = "//:bin/clang"\n',
        },
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = (
                "Disallowed workspace root package label: `//:bin/clang`. " +
                "Use `//bin:clang` instead, and add a `BUILD.bazel` file to `//bin` if needed. " +
                _DOCS_SUFFIX
            ),
            filepath = "src/BUILD.bazel",
            line = 1,
            col = 9,
            end_col = 21,
        ),
    ))

def run_root_package_labels_tests():
    """Runs all root_package_labels tests."""
    test_disallowed_workspace_root_package_labels()
    test_allowed_labels_in_build_files()
    test_policy_exempt_workspace_root_exports()
    test_policy_exempt_external_repository_files()
    test_disallowed_external_repo_label_in_regular_build_file()
