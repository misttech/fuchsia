# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `FuchsiaRunnerShardInfo` and `collect_runner_shard_aspect`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "//build/bazel/aspects:runner.bzl",
    "FuchsiaRunnerShardInfo",
    "collect_runner_shard_aspect",
)

def _empty_target_impl(_ctx):
    pass

_empty_target = rule(
    implementation = _empty_target_impl,
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
    extra_target_under_test_aspects = [collect_runner_shard_aspect],
    attrs = {
        "expected_shards": attr.string_list(mandatory = True),
    },
)

def runner_test_suite(name):
    """Defines a test suite for `collect_runner_shard_aspect`.

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

    native.test_suite(
        name = name,
        visibility = ["//visibility:public"],
        tests = [
            ":runner_no_shard_test",
            ":runner_direct_hint_test",
            ":runner_transitive_and_diamond_test",
            ":runner_conflicting_shards_test",
        ],
    )
