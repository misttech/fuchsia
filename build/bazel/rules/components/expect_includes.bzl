# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Provider and rule for declaring expected component manifest shard includes."""

load(
    "//build/bazel/rules/components:cml_shard.bzl",
    "FuchsiaCmlShardInfo",
)

FuchsiaExpectedIncludesInfo = provider(
    doc = "A provider for expected component manifest includes from expect_includes() targets.",
    fields = {
        "enforced_shards": """depset[File] of manifest shard (.shard.cml / .cml) files from
        expect_includes(enforce = True) that must be checked by cmc check-includes on dependent
        components.""",
        "auto_include_shards": """depset[File] of manifest shard (.shard.cml / .cml) files from all
        expect_includes() targets (both enforce = True and enforce = False) to automatically include
        in fx_unittest_component() generated manifests.""",
        "transitive_shards": """depset[File] of all shard files from includes (and their
        cml_shard() transitive includes) and deps of expect_includes() targets, needed as sandbox
        inputs when running cmc compile or cmc check-includes.""",
    },
)

def _expect_includes_impl(ctx):
    direct_shards = ctx.files.includes
    enforced_shards = depset(
        direct_shards if ctx.attr.enforce else [],
        transitive = [
            dep[FuchsiaExpectedIncludesInfo].enforced_shards
            for dep in ctx.attr.deps
        ],
    )
    auto_include_shards = depset(
        direct_shards,
        transitive = [
            dep[FuchsiaExpectedIncludesInfo].auto_include_shards
            for dep in ctx.attr.deps
        ],
    )
    transitive_shards = depset(
        direct_shards,
        transitive = [
            dep[FuchsiaCmlShardInfo].transitive_shards
            for dep in ctx.attr.includes
            if FuchsiaCmlShardInfo in dep
        ] + [
            dep[FuchsiaExpectedIncludesInfo].transitive_shards
            for dep in ctx.attr.deps
        ],
    )
    return [
        DefaultInfo(files = transitive_shards),
        FuchsiaExpectedIncludesInfo(
            enforced_shards = enforced_shards,
            auto_include_shards = auto_include_shards,
            transitive_shards = transitive_shards,
        ),
    ]

expect_includes = rule(
    doc = """Declares that dependent components must include specific manifest shards.

Any `fx_component()` or `fx_test_component()` target that transitively depends
on this target (via `deps` or `aspect_hints`) will verify using
`cmc check-includes` that its component manifest includes the `.cml` /
`.shard.cml` files listed in `includes` (unless `enforce = False` on this target
or `check_includes = False` on the component).

In addition, `fx_unittest_component()` and `fx_unittest_package()` automatically
add the shards in `includes` to the component manifest they generate for unit
test binaries, regardless of whether `enforce` is `True` or `False`.

If a shard in `includes` itself `include`s other `.shard.cml` files, wrap it in
a `cml_shard()` target so its transitive shards are available in the Bazel
sandbox when `cmc compile` and `cmc check-includes` run.

Note that rules with strict dependency provider requirements (such as
`fx_cc_library()`, `cc_library()`, `rustc_library()`, etc.) should depend on
`expect_includes()` targets via `aspect_hints` rather than `deps`. When
converted to GN by `bazel2gn`, `aspect_hints` is mapped to `deps` so GN's
`expect_includes()` metadata walk sees the dependency as well.
""",
    implementation = _expect_includes_impl,
    attrs = {
        "includes": attr.label_list(
            doc = """The manifest shard (`cml_shard()` targets or `.shard.cml` / `.cml` files) that
            dependent components must include in their component manifests, and that are
            automatically included in generated `fx_unittest_component()` manifests.""",
            allow_files = [".cml"],
            mandatory = True,
        ),
        "enforce": attr.bool(
            doc = """Whether to enforce that dependent components include `includes` via `cmc check-includes`.

If `False`, expected includes are not enforced on components with explicit
manifests, though shard inclusion in generated unit test manifests
(`fx_unittest_component()`) is still performed.""",
            default = True,
        ),
        "deps": attr.label_list(
            doc = """Other `expect_includes()` targets whose expected includes are transitively
required by this target.""",
            providers = [FuchsiaExpectedIncludesInfo],
        ),
    },
    provides = [FuchsiaExpectedIncludesInfo],
)
