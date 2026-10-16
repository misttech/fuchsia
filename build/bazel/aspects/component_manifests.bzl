# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Aspect for propagating component manifest shards."""

load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentInfo",
)
load("//build/bazel/aspects:runner.bzl", "FuchsiaRunnerShardInfo")
load("//build/bazel/aspects:utils.bzl", "get_target_deps_from_attributes")
load(
    "//build/bazel/rules/components:expect_includes.bzl",
    "FuchsiaExpectedIncludesInfo",
)

_DEP_ATTRS = [
    "binary",
    "crate",
    "deps",
    "implementation_deps",
    "link_deps",
    "runtime_deps",
    "shared_library",
]

def _collect_component_manifests_aspect_impl(target, aspect_ctx):
    # Stop propagating manifest shards across component boundaries so a parent
    # component does not inherit a child component's expected includes or runner
    # shards, matching GN's `expect_includes_barrier = []` on
    # `fuchsia_component`.
    if FuchsiaComponentInfo in target:
        return [
            FuchsiaRunnerShardInfo(shards = depset()),
            FuchsiaExpectedIncludesInfo(
                enforced_shards = depset(),
                auto_include_shards = depset(),
                transitive_shards = depset(),
            ),
        ]

    # The current target is an fx_runner_shard() or expect_includes() target
    # visited through a regular dependency. Exit immediately to avoid a
    # duplicate provider error.
    if FuchsiaRunnerShardInfo in target or FuchsiaExpectedIncludesInfo in target:
        return []

    deps = get_target_deps_from_attributes(
        aspect_ctx.rule.attr,
        attr_names = _DEP_ATTRS + ["aspect_hints"],
    )

    runner_shards = [
        dep[FuchsiaRunnerShardInfo].shards
        for dep in deps
        if FuchsiaRunnerShardInfo in dep
    ]
    enforced_shards = [
        dep[FuchsiaExpectedIncludesInfo].enforced_shards
        for dep in deps
        if FuchsiaExpectedIncludesInfo in dep
    ]
    auto_include_shards = [
        dep[FuchsiaExpectedIncludesInfo].auto_include_shards
        for dep in deps
        if FuchsiaExpectedIncludesInfo in dep
    ]
    transitive_shards = [
        dep[FuchsiaExpectedIncludesInfo].transitive_shards
        for dep in deps
        if FuchsiaExpectedIncludesInfo in dep
    ]

    return [
        FuchsiaRunnerShardInfo(
            shards = depset(transitive = runner_shards),
        ),
        FuchsiaExpectedIncludesInfo(
            enforced_shards = depset(transitive = enforced_shards),
            auto_include_shards = depset(transitive = auto_include_shards),
            transitive_shards = depset(transitive = transitive_shards),
        ),
    ]

collect_component_manifests_aspect = aspect(
    doc = "An aspect that collects FuchsiaRunnerShardInfo and FuchsiaExpectedIncludesInfo providers from transitive dependencies.",
    implementation = _collect_component_manifests_aspect_impl,
    attr_aspects = _DEP_ATTRS,
)
