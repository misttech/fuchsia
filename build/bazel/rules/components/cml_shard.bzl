# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Provider and rule for declaring component manifest shards and their transitive includes."""

FuchsiaCmlShardInfo = provider(
    doc = "A provider for a component manifest shard (.shard.cml) and its transitive shard includes.",
    fields = {
        "shard": "The direct `.shard.cml` File represented by this target.",
        "transitive_shards": """depset[File] containing `shard` and all transitive `.shard.cml` / `.cml` files from
        `includes`, needed as sandbox inputs when running `cmc compile` or `cmc check-includes`.""",
    },
)

def _cml_shard_impl(ctx):
    # Enforce that `name` ends in `.shard` and `shard` matches `name + ".cml"`
    # in the same package. This convention is required for compatibility with
    # `bazel2gn` for now, so that `bazel2gn` can convert `cml_shard` target
    # labels (e.g. in `expect_includes`) into `.cml` file paths without
    # cross-target lookups.
    if not ctx.label.name.endswith(".shard"):
        fail("`cml_shard` target name {} must end in '.shard' (for bazel2gn compatibility).".format(
            repr(ctx.label.name),
        ))
    expected_shard_name = ctx.label.name + ".cml"
    shard_label = ctx.attr.shard.label
    if shard_label.package != ctx.label.package or shard_label.name != expected_shard_name:
        fail("`cml_shard` target {} must reference `shard = {}` in the same package (got {}) for bazel2gn compatibility.".format(
            repr(ctx.label.name),
            repr(expected_shard_name),
            repr(str(shard_label)),
        ))

    # `includes` accepts both `cml_shard` targets (which carry `FuchsiaCmlShardInfo`
    # with their transitive shard dependencies) and raw `.cml` / `.shard.cml` leaf
    # files (which do not include other shards and only provide `DefaultInfo.files`).
    transitive_shards = depset(
        [ctx.file.shard],
        transitive = [
            dep[FuchsiaCmlShardInfo].transitive_shards if FuchsiaCmlShardInfo in dep else dep[DefaultInfo].files
            for dep in ctx.attr.includes
        ],
    )
    return [
        DefaultInfo(files = depset([ctx.file.shard])),
        FuchsiaCmlShardInfo(
            shard = ctx.file.shard,
            transitive_shards = transitive_shards,
        ),
    ]

cml_shard = rule(
    doc = """Declares a component manifest shard (`.shard.cml`) and its transitive shard dependencies.

Because Bazel actions execute in a sandbox, if a `.shard.cml` file `include`s
other `.shard.cml` files, declaring it with `cml_shard` propagates all
transitive shard files to dependent `fx_component_manifest()` and
`expect_includes()` targets.

Note: For compatibility with `bazel2gn`, `name` must end in `.shard` and `shard`
must be set to `name + ".cml"` in the same package (e.g. `name = "client.shard"`,
`shard = "client.shard.cml"`).
""",
    implementation = _cml_shard_impl,
    attrs = {
        "shard": attr.label(
            doc = "The manifest shard (`.shard.cml`) file in the same package. Must equal `name + \".cml\"`.",
            allow_single_file = [".shard.cml"],
            mandatory = True,
        ),
        "includes": attr.label_list(
            doc = "Other `cml_shard` targets or `.cml` files included by `shard`.",
            allow_files = [".cml"],
        ),
    },
    provides = [FuchsiaCmlShardInfo],
)
