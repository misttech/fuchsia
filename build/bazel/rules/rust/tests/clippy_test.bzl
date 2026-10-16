# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for disable_clippy across Fuchsia Bazel Rust macros."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_rust//rust:defs.bzl", "rust_clippy_aspect", "rust_lint_config")
load("//build/bazel/rules/host_tests:host_rustc_test.bzl", "host_rustc_test")
load("//build/bazel/rules/rust:rustc_binary.bzl", "rustc_binary")
load("//build/bazel/rules/rust:rustc_dylib.bzl", "rustc_dylib")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")
load("//build/bazel/rules/rust:rustc_proc_macro.bzl", "rustc_proc_macro")
load("//build/bazel/rules/rust:rustc_test.bzl", "rustc_test")

_TagsInfo = provider(
    doc = "Records the target's rule tags for analysis tests.",
    fields = ["tags"],
)

def _tags_aspect_impl(_target, ctx):
    return [_TagsInfo(tags = list(ctx.rule.attr.tags))]

_tags_aspect = aspect(
    implementation = _tags_aspect_impl,
)

def _clippy_analysis_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    actions = target.actions

    clippy_actions = [a for a in actions if a.mnemonic == "Clippy"]
    rustc_actions = [a for a in actions if a.mnemonic == "Rustc"]

    has_clippy_checks = OutputGroupInfo in target and hasattr(target[OutputGroupInfo], "clippy_checks")
    tags = target[_TagsInfo].tags
    noclippy_count = len([t for t in tags if t == "noclippy"])

    if ctx.attr.expect_clippy:
        asserts.equals(env, 1, len(clippy_actions), "Expected 1 Clippy action.")
        asserts.true(env, has_clippy_checks, "Expected clippy_checks in OutputGroupInfo.")
        asserts.equals(env, 0, noclippy_count, "Expected noclippy tag to be absent.")
    else:
        asserts.equals(env, 0, len(clippy_actions), "Expected 0 Clippy actions when disable_clippy = True.")
        asserts.false(env, has_clippy_checks, "Expected no clippy_checks in OutputGroupInfo when disable_clippy = True.")
        asserts.equals(env, 1, noclippy_count, "Expected exactly 1 noclippy tag when disable_clippy = True.")

    for expected_tag in ctx.attr.expected_tags:
        asserts.true(
            env,
            expected_tag in tags,
            "Expected tag '{}' in tags {}.".format(expected_tag, tags),
        )

    asserts.equals(env, 1, len(rustc_actions), "Expected 1 Rustc action.")

    # @rules_rust suppresses lint_config flags on Rustc actions in the exec configuration
    # (`build_for_host` / `quick_targets_to_build_in_host_configuration`).
    if "-exec" not in ctx.genfiles_dir.path and rustc_actions:
        asserts.true(
            env,
            ctx.attr.expected_rustc_flag in rustc_actions[0].argv,
            "Expected '{}' in Rustc action argv.".format(ctx.attr.expected_rustc_flag),
        )

    if env.failures:
        # Call fail explicitly so it fails on `bazel build`, which is needed to execute
        # these tests as build-time tests in our GN/Bazel setup today.
        fail("\n".join(env.failures))
    return analysistest.end(env)

_clippy_analysis_test = analysistest.make(
    _clippy_analysis_test_impl,
    extra_target_under_test_aspects = [rust_clippy_aspect, _tags_aspect],
    attrs = {
        "expect_clippy": attr.bool(mandatory = True),
        "expected_rustc_flag": attr.string(default = "--deny=deref_nullptr"),
        "expected_tags": attr.string_list(default = []),
    },
)

def clippy_test_suite(name):
    """Defines fixtures and analysis tests for disable_clippy."""
    tests = []

    rust_lint_config(
        name = "custom_lints",
        rustc = {
            "unused_results": "deny",
        },
    )

    # rustc_library
    rustc_library(
        name = "lib_default",
        srcs = ["lib.rs"],
        edition = "2024",
        with_unit_tests = True,
    )
    rustc_library(
        name = "lib_disabled",
        srcs = ["lib.rs"],
        disable_clippy = True,
        edition = "2024",
        with_unit_tests = True,
    )
    rustc_library(
        name = "lib_custom_lints_disabled",
        srcs = ["lib.rs"],
        disable_clippy = True,
        edition = "2024",
        lint_config = ":custom_lints",
        tags = ["custom_tag", "noclippy"],
        with_unit_tests = True,
    )

    # rustc_dylib
    rustc_dylib(
        name = "dylib_default",
        srcs = ["lib.rs"],
        edition = "2024",
        with_unit_tests = True,
    )
    rustc_dylib(
        name = "dylib_disabled",
        srcs = ["lib.rs"],
        disable_clippy = True,
        edition = "2024",
        with_unit_tests = True,
    )

    # rustc_binary
    rustc_binary(
        name = "bin_default",
        srcs = ["main.rs"],
        edition = "2024",
        with_unit_tests = True,
    )
    rustc_binary(
        name = "bin_disabled",
        srcs = ["main.rs"],
        disable_clippy = True,
        edition = "2024",
        with_unit_tests = True,
    )

    # rustc_proc_macro
    rustc_proc_macro(
        name = "proc_macro_default",
        srcs = ["lib.rs"],
        edition = "2024",
        with_unit_tests = True,
    )
    rustc_proc_macro(
        name = "proc_macro_disabled",
        srcs = ["lib.rs"],
        disable_clippy = True,
        edition = "2024",
        with_unit_tests = True,
    )

    # rustc_test
    rustc_test(
        name = "test_default",
        srcs = ["main.rs"],
        edition = "2024",
    )
    rustc_test(
        name = "test_disabled",
        srcs = ["main.rs"],
        disable_clippy = True,
        edition = "2024",
    )

    # host_rustc_test
    host_rustc_test(
        name = "host_test_default",
        srcs = ["main.rs"],
        edition = "2024",
    )
    host_rustc_test(
        name = "host_test_disabled",
        srcs = ["main.rs"],
        disable_clippy = True,
        edition = "2024",
    )

    cases = [
        ("lib_default", ":lib_default", True, "--deny=deref_nullptr", []),
        ("lib_default_unit_test", ":lib_default_test", True, "--deny=deref_nullptr", []),
        ("lib_disabled", ":lib_disabled", False, "--deny=deref_nullptr", []),
        ("lib_disabled_unit_test", ":lib_disabled_test", False, "--deny=deref_nullptr", []),
        ("lib_custom_lints_disabled", ":lib_custom_lints_disabled", False, "--deny=unused_results", ["custom_tag", "noclippy"]),
        ("lib_custom_lints_disabled_unit_test", ":lib_custom_lints_disabled_test", False, "--deny=unused_results", ["custom_tag", "noclippy"]),
        ("dylib_default", ":dylib_default", True, "--deny=deref_nullptr", []),
        ("dylib_default_unit_test", ":dylib_default_test", True, "--deny=deref_nullptr", []),
        ("dylib_disabled", ":dylib_disabled", False, "--deny=deref_nullptr", []),
        ("dylib_disabled_unit_test", ":dylib_disabled_test", False, "--deny=deref_nullptr", []),
        ("bin_default", ":bin_default", True, "--deny=deref_nullptr", []),
        ("bin_default_unit_test", ":bin_default_test", True, "--deny=deref_nullptr", []),
        ("bin_disabled", ":bin_disabled", False, "--deny=deref_nullptr", []),
        ("bin_disabled_unit_test", ":bin_disabled_test", False, "--deny=deref_nullptr", []),
        ("proc_macro_default", ":proc_macro_default", True, "--deny=deref_nullptr", []),
        ("proc_macro_default_unit_test", ":proc_macro_default_test", True, "--deny=deref_nullptr", []),
        ("proc_macro_disabled", ":proc_macro_disabled", False, "--deny=deref_nullptr", []),
        ("proc_macro_disabled_unit_test", ":proc_macro_disabled_test", False, "--deny=deref_nullptr", []),
        ("test_default", ":test_default", True, "--deny=deref_nullptr", []),
        ("test_disabled", ":test_disabled", False, "--deny=deref_nullptr", []),
        ("host_test_default", ":host_test_default_bin", True, "--deny=deref_nullptr", ["manual"]),
        ("host_test_disabled", ":host_test_disabled_bin", False, "--deny=deref_nullptr", ["manual", "noclippy"]),
    ]

    for case_name, target_under_test, expect_clippy, expected_rustc_flag, expected_tags in cases:
        test_name = case_name + "_clippy_test"
        _clippy_analysis_test(
            name = test_name,
            target_under_test = target_under_test,
            expect_clippy = expect_clippy,
            expected_rustc_flag = expected_rustc_flag,
            expected_tags = expected_tags,
        )
        tests.append(":" + test_name)

    native.test_suite(
        name = name,
        visibility = ["//visibility:public"],
        tests = tests,
    )
