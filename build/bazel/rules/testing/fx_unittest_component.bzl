# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro and helper rule for defining a Fuchsia unit test component."""

load(
    "//build/bazel/aspects:runner.bzl",
    "FuchsiaRunnerShardInfo",
    "collect_runner_shard_aspect",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
    "fx_test_component",
)
load("//build/bazel/rules/packages:fx_packaged_binary.bzl", "fx_packaged_binary")

_TEST_BINARY_INSTALL_ROOT = "bin/"

def _fx_unittest_runner_shard_impl(ctx):
    binary = ctx.attr.binary
    shards = (
        binary[FuchsiaRunnerShardInfo].shards.to_list() if FuchsiaRunnerShardInfo in binary else []
    )
    if not shards:
        runner_shard_file = ctx.file._default_runner_shard
    elif len(shards) > 1:
        fail(
            "Multiple conflicting runner shards found in dependencies of {}: {}".format(
                binary.label,
                ", ".join(sorted(["//" + s.short_path for s in shards])),
            ),
        )
    else:
        runner_shard_file = shards[0]

    return [
        DefaultInfo(files = depset([runner_shard_file])),
    ]

_fx_unittest_runner_shard = rule(
    doc = """Resolves the component runner manifest shard (.shard.cml) for a unit test binary.

    This rule applies `collect_runner_shard_aspect` to `binary` to gather any
    `FuchsiaRunnerShardInfo` providers from its transitive dependencies. If no
    runner shard is found, it falls back to `_default_runner_shard` (the ELF
    test runner). If multiple conflicting runner shards are found, analysis
    fails with an error.

    The selected `.shard.cml` file is returned in `DefaultInfo.files`, allowing
    this target to be passed directly to attributes that expect `.shard.cml`
    file labels (such as `_fx_unittest_manifest.shards` and
    `fx_component_manifest.includes`).
    """,
    implementation = _fx_unittest_runner_shard_impl,
    attrs = {
        "binary": attr.label(
            doc = "The unit test executable target.",
            mandatory = True,
            executable = True,
            cfg = "target",
            aspects = [collect_runner_shard_aspect],
        ),
        "_default_runner_shard": attr.label(
            default = "//sdk/lib/sys/testing:elf_test_runner",
            allow_single_file = [".shard.cml"],
        ),
    },
)

def _fx_unittest_manifest_impl(ctx):
    includes = [
        "//" + shard.short_path
        for shard in ctx.files.shards
    ]

    cml_content = json.encode_indent(
        {
            "include": includes,
            "program": {
                "binary": ctx.attr.binary_path,
            },
        },
        indent = "    ",
    ) + "\n"

    cml_file = ctx.actions.declare_file(ctx.label.name + ".cml")
    ctx.actions.write(
        output = cml_file,
        content = cml_content,
    )

    return [
        DefaultInfo(files = depset([cml_file])),
    ]

_fx_unittest_manifest = rule(
    doc = """Generates a component manifest (.cml) for a unit test binary.

    Writes a JSON `.cml` file setting `program.binary` to `binary_path` and
    populating `include` with the workspace-relative paths of all `.shard.cml`
    files provided in `shards`. The resulting `.cml` file is returned in
    `DefaultInfo.files` to be compiled by `fx_component_manifest()`.
    """,
    implementation = _fx_unittest_manifest_impl,
    attrs = {
        "binary_path": attr.string(
            doc = "The packaged path of the binary relative to the package root.",
            mandatory = True,
        ),
        "shards": attr.label_list(
            doc = "The manifest shard (.shard.cml) files to include in the generated manifest.",
            mandatory = True,
            allow_files = [".shard.cml"],
        ),
    },
)

def _fx_unittest_component_impl(
        name,
        binary,
        component_name,
        testonly,
        visibility,
        tags,
        **kwargs):
    if testonly != None and not testonly:
        fail("`fx_unittest_component()` targets are always testonly.")

    component_name = component_name or name
    binary_name = binary.name
    packaged_binary_name = name + ".packaged_binary"
    tags = tags or []
    fx_packaged_binary(
        name = packaged_binary_name,
        testonly = True,
        binary = binary,
        binary_name = binary_name,
        install_root = _TEST_BINARY_INSTALL_ROOT,
        tags = tags + ["manual"],
    )

    runner_shard_name = name + ".runner_shard"
    _fx_unittest_runner_shard(
        name = runner_shard_name,
        testonly = True,
        binary = binary,
        tags = tags + ["manual"],
    )

    # TODO(https://fxbug.dev/563978780): Collect shards from `expect_includes`
    # dependencies once `expect_includes` is implemented in Bazel.
    shards = [
        ":" + runner_shard_name,
        "//sdk/lib/syslog:use.shard.cml",
    ]

    generated_manifest_name = name + ".generated_manifest"
    _fx_unittest_manifest(
        name = generated_manifest_name,
        testonly = True,
        binary_path = _TEST_BINARY_INSTALL_ROOT + binary_name,
        shards = shards,
        tags = tags + ["manual"],
    )

    manifest_name = name + ".manifest"
    fx_component_manifest(
        name = manifest_name,
        testonly = True,
        component_name = component_name,
        includes = shards,
        manifest = ":" + generated_manifest_name,
        tags = tags + ["manual"],
    )

    fx_test_component(
        name = name,
        compiled_manifest = ":" + manifest_name,
        component_name = component_name,
        testonly = True,
        visibility = visibility,
        deps = [":" + packaged_binary_name],
        tags = tags,
        **kwargs
    )

fx_unittest_component = macro(
    doc = """Defines a Fuchsia test component for a single unit test binary.

    This macro automatically generates a component manifest for the provided
    `binary` target, wraps the binary with `fx_packaged_binary()`, compiles the
    manifest with `fx_component_manifest()`, and creates the test component with
    `fx_test_component()`.

    Example usage:

    ```
    fx_unittest_component(
        name = "foo-test-component",
        component_name = "foo-test",
        binary = ":foo_lib_test",
    )
    ```
    """,
    implementation = _fx_unittest_component_impl,
    inherit_attrs = fx_test_component,
    attrs = {
        "binary": attr.label(
            doc = "The unit test binary target to package and run.",
            mandatory = True,
            configurable = False,
        ),
        "component_name": attr.string(
            doc = """ An optional name for the test component.

            Defaults to `name` to match the behavior of `fx_package` and
            `fx_component`. """,
            # Otherwise an unset value reaches the impl as an always-truthy
            # select(), and the `name` fallback never applies.
            configurable = False,
        ),
        # Set internally by the macro.
        "compiled_manifest": None,
        "deps": None,
    },
)
