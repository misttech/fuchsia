# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Provider, rule, and aspect for propagating Fuchsia component runner shards."""

load("//build/bazel/aspects:utils.bzl", "get_target_deps_from_attributes")

FuchsiaRunnerShardInfo = provider(
    doc = "A provider for Fuchsia component runner manifest shards (.shard.cml).",
    fields = {
        "shards": "depset[File] of runner manifest shard (.shard.cml) files.",
    },
)

def _fx_runner_shard_impl(ctx):
    return [
        DefaultInfo(files = depset([ctx.file.shard])),
        FuchsiaRunnerShardInfo(
            shards = depset([ctx.file.shard]),
        ),
    ]

fx_runner_shard = rule(
    doc = """Wrap a component runner manifest shard (.shard.cml) file as a target.

Used by `fx_unittest_component()` to collect them from its transitive
dependencies. NOTE: This does *not* work for `fx_component()`.

Note that certain rules, like `cc_library()`, only support depending to strict
target types. For these, using `aspect_hints` instead of `deps` to depend on an
`fx_runner_shared()` target is required. For example:

    fx_runner_shard(
        name = "my_shard",
        shard = "my_shared.cml",
    )

    fx_cc_binary(
         name = "my_binary",
         ...,
         # deps = [ ":my_shared" ]            # This FAILS
         aspect_hints = [ ":my_shared" ],     # This WORKS!
    )

    fx_component(
         name = "my_component",
         ...
         deps = [ ":my_binary" ],
    )

Other rules (e.g. `rust_binary`) do not have this restriction and can use either
`deps` or `aspect_hints` to depend on it (the latter is slightly more efficient
at analysis time). """,
    implementation = _fx_runner_shard_impl,
    attrs = {
        "shard": attr.label(
            doc = "The runner manifest shard (.shard.cml) file.",
            allow_single_file = [".shard.cml"],
            mandatory = True,
        ),
    },
    provides = [FuchsiaRunnerShardInfo],
)

_DEP_ATTRS = [
    "crate",
    "deps",
    "implementation_deps",
    "link_deps",
]

def _collect_runner_shard_aspect_impl(target, aspect_ctx):
    # The current target is an fx_runner_share(), visited through a regular
    # dependency. The aspect should exit immediately to avoid a duplicate
    # provider error.
    if FuchsiaRunnerShardInfo in target:
        return []

    transitive_shards = [
        dep[FuchsiaRunnerShardInfo].shards
        for dep in get_target_deps_from_attributes(
            aspect_ctx.rule.attr,
            attr_names = _DEP_ATTRS + ["aspect_hints"],
        )
        if FuchsiaRunnerShardInfo in dep
    ]

    return [
        FuchsiaRunnerShardInfo(
            shards = depset(transitive = transitive_shards),
        ),
    ]

collect_runner_shard_aspect = aspect(
    doc = "An aspect that collects FuchsiaRunnerShardInfo providers from transitive dependencies.",
    implementation = _collect_runner_shard_aspect_impl,
    attr_aspects = _DEP_ATTRS,
)
