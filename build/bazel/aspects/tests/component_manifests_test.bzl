# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `collect_component_manifests_aspect`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentInfo",
)
load(
    "//build/bazel/aspects:component_manifests.bzl",
    "collect_component_manifests_aspect",
)
load("//build/bazel/aspects:runner.bzl", "FuchsiaRunnerShardInfo")
load(
    "//build/bazel/rules/components:expect_includes.bzl",
    "FuchsiaExpectedIncludesInfo",
    "expect_includes",
)

def _empty_target_impl(_ctx):
    pass

_empty_target = rule(
    implementation = _empty_target_impl,
    attrs = {
        "deps": attr.label_list(),
    },
)

def _fake_component_target_impl(_ctx):
    return [
        FuchsiaComponentInfo(
            name = "fake",
            manifest = None,
            resources = [],
            moniker = None,
            is_driver = False,
            is_test = False,
            test_realm = None,
            run_tag = "fake",
        ),
    ]

_fake_component_target = rule(
    implementation = _fake_component_target_impl,
    attrs = {
        "deps": attr.label_list(),
    },
)

def _runner_shard_aspect_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    asserts.true(
        env,
        FuchsiaRunnerShardInfo in target,
        "Expected FuchsiaRunnerShardInfo on target under test.",
    )
    actual_shards = sorted([
        "//" + f.short_path
        for f in target[FuchsiaRunnerShardInfo].shards.to_list()
    ])
    asserts.equals(
        env,
        sorted(ctx.attr.expected_shards),
        actual_shards,
    )
    if env.failures:
        # Call fail explicitly so it fails on `bazel build`, which is needed to
        # execute these tests as build-time tests in our GN/Bazel setup today.
        fail("\n".join(env.failures))
    return analysistest.end(env)

_runner_shard_aspect_test = analysistest.make(
    _runner_shard_aspect_test_impl,
    extra_target_under_test_aspects = [collect_component_manifests_aspect],
    attrs = {
        "expected_shards": attr.string_list(mandatory = True),
    },
)

def _expected_includes_aspect_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    asserts.true(
        env,
        FuchsiaExpectedIncludesInfo in target,
        "Expected FuchsiaExpectedIncludesInfo on target under test.",
    )
    info = target[FuchsiaExpectedIncludesInfo]
    actual_enforced = sorted([
        "//" + f.short_path
        for f in info.enforced_shards.to_list()
    ])
    actual_auto_include = sorted([
        "//" + f.short_path
        for f in info.auto_include_shards.to_list()
    ])
    actual_transitive = sorted([
        "//" + f.short_path
        for f in info.transitive_shards.to_list()
    ])
    asserts.equals(
        env,
        sorted(ctx.attr.expected_enforced_shards),
        actual_enforced,
    )
    asserts.equals(
        env,
        sorted(ctx.attr.expected_auto_include_shards),
        actual_auto_include,
    )
    asserts.equals(
        env,
        sorted(ctx.attr.expected_transitive_shards),
        actual_transitive,
    )
    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_expected_includes_aspect_test = analysistest.make(
    _expected_includes_aspect_test_impl,
    extra_target_under_test_aspects = [collect_component_manifests_aspect],
    attrs = {
        "expected_enforced_shards": attr.string_list(mandatory = True),
        "expected_auto_include_shards": attr.string_list(mandatory = True),
        "expected_transitive_shards": attr.string_list(mandatory = True),
    },
)

def component_manifests_test_suite(name):
    """Defines a test suite for `collect_component_manifests_aspect`.

    Args:
      name: Test suite's name.
    """
    _empty_target(
        name = "runner_no_shard_leaf",
    )

    _empty_target(
        name = "runner_no_shard_root",
        deps = [":runner_no_shard_leaf"],
    )

    _runner_shard_aspect_test(
        name = "runner_no_shard_test",
        target_under_test = ":runner_no_shard_root",
        expected_shards = [],
    )

    _empty_target(
        name = "runner_direct_hint",
        aspect_hints = ["//src/sys/test_runners/rust:default"],
    )

    _runner_shard_aspect_test(
        name = "runner_direct_hint_test",
        target_under_test = ":runner_direct_hint",
        expected_shards = ["//src/sys/test_runners/rust/default.shard.cml"],
    )

    _empty_target(
        name = "runner_gtest_leaf",
        aspect_hints = ["//src/sys/test_runners/gtest:default"],
    )

    _empty_target(
        name = "runner_mid_a",
        deps = [":runner_gtest_leaf"],
    )

    _empty_target(
        name = "runner_mid_b",
        deps = [":runner_gtest_leaf"],
    )

    _empty_target(
        name = "runner_diamond_root",
        deps = [
            ":runner_mid_a",
            ":runner_mid_b",
            ":runner_no_shard_leaf",
        ],
    )

    _runner_shard_aspect_test(
        name = "runner_transitive_and_diamond_test",
        target_under_test = ":runner_diamond_root",
        expected_shards = ["//src/sys/test_runners/gtest/default.shard.cml"],
    )

    _empty_target(
        name = "runner_zxtest_leaf",
        aspect_hints = ["//src/sys/test_runners/gtest:zxtest"],
    )

    _empty_target(
        name = "runner_conflicting_root",
        deps = [
            ":runner_mid_a",
            ":runner_zxtest_leaf",
        ],
    )

    _runner_shard_aspect_test(
        name = "runner_conflicting_shards_test",
        target_under_test = ":runner_conflicting_root",
        expected_shards = [
            "//src/sys/test_runners/gtest/default.shard.cml",
            "//src/sys/test_runners/gtest/zxtest.shard.cml",
        ],
    )

    # Expected includes tests.
    _expected_includes_aspect_test(
        name = "expected_includes_empty_test",
        target_under_test = ":runner_no_shard_root",
        expected_enforced_shards = [],
        expected_auto_include_shards = [],
        expected_transitive_shards = [],
    )

    expect_includes(
        name = "unenforced_includes",
        enforce = False,
        includes = ["//src/sys/test_runners/rust:next-vdso.shard.cml"],
    )

    expect_includes(
        name = "chained_includes",
        includes = ["//src/sys/test_runners/gtest:zxtest.shard.cml"],
        deps = ["//sdk/lib/syslog:client_includes"],
    )

    _empty_target(
        name = "includes_leaf_enforced",
        aspect_hints = [":chained_includes"],
    )

    _empty_target(
        name = "includes_leaf_unenforced",
        aspect_hints = [":unenforced_includes"],
    )

    _empty_target(
        name = "includes_mid_a",
        deps = [
            ":includes_leaf_enforced",
            ":includes_leaf_unenforced",
        ],
    )

    _empty_target(
        name = "includes_mid_b",
        deps = [":includes_leaf_enforced"],
    )

    _empty_target(
        name = "includes_diamond_root",
        deps = [
            ":includes_mid_a",
            ":includes_mid_b",
        ],
    )

    _expected_includes_aspect_test(
        name = "expected_includes_enforced_and_unenforced_test",
        target_under_test = ":includes_diamond_root",
        expected_enforced_shards = [
            "//sdk/lib/syslog/client.shard.cml",
            "//src/sys/test_runners/gtest/zxtest.shard.cml",
        ],
        expected_auto_include_shards = [
            "//sdk/lib/syslog/client.shard.cml",
            "//src/sys/test_runners/gtest/zxtest.shard.cml",
            "//src/sys/test_runners/rust/next-vdso.shard.cml",
        ],
        expected_transitive_shards = [
            "//sdk/lib/syslog/client.shard.cml",
            "//sdk/lib/syslog/offer.shard.cml",
            "//sdk/lib/syslog/use.shard.cml",
            "//src/sys/test_runners/gtest/zxtest.shard.cml",
            "//src/sys/test_runners/rust/next-vdso.shard.cml",
        ],
    )

    # Component barrier test: a target providing FuchsiaComponentInfo stops
    # propagation of both runner shards and expected includes.
    _fake_component_target(
        name = "child_component_barrier",
        deps = [
            ":includes_diamond_root",
            ":runner_gtest_leaf",
        ],
    )

    _empty_target(
        name = "parent_of_child_component",
        deps = [":child_component_barrier"],
    )

    _expected_includes_aspect_test(
        name = "expected_includes_component_barrier_test",
        target_under_test = ":parent_of_child_component",
        expected_enforced_shards = [],
        expected_auto_include_shards = [],
        expected_transitive_shards = [],
    )

    _runner_shard_aspect_test(
        name = "runner_shard_component_barrier_test",
        target_under_test = ":parent_of_child_component",
        expected_shards = [],
    )

    native.test_suite(
        name = name,
        visibility = ["//visibility:public"],
        tests = [
            ":expected_includes_component_barrier_test",
            ":expected_includes_empty_test",
            ":expected_includes_enforced_and_unenforced_test",
            ":runner_conflicting_shards_test",
            ":runner_direct_hint_test",
            ":runner_no_shard_test",
            ":runner_shard_component_barrier_test",
            ":runner_transitive_and_diamond_test",
        ],
    )
