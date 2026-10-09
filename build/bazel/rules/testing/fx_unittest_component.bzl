# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro and helper rule for defining a Fuchsia unit test component."""

load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust:defs.bzl", "rust_common")
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
    "fx_test_component",
)
load("//build/bazel/rules/packages:fx_packaged_binary.bzl", "fx_packaged_binary")

_TEST_BINARY_INSTALL_ROOT = "bin/"

_GTEST_REPO_NAMES = (
    "com_google_googletest",
    "googletest",
    "googletest+",
)

def _infer_runner_shard(binary):
    if rust_common.crate_info in binary:
        return "//src/sys/test_runners/rust/default.shard.cml"

    if CcInfo in binary:
        for linker_input in binary[CcInfo].linking_context.linker_inputs.to_list():
            owner = linker_input.owner
            if owner.repo_name in _GTEST_REPO_NAMES or owner.package == "third_party/googletest":
                return "//src/sys/test_runners/gtest/default.shard.cml"
            if owner.package == "zircon/system/ulib/zxtest":
                return "//src/sys/test_runners/gtest/zxtest.shard.cml"
        return "//sdk/lib/sys/testing/elf_test_runner.shard.cml"

    fail(
        "Unsupported test binary target {} for fx_unittest_component: ".format(binary.label) +
        "expected a Rust or C++ executable target.",
    )

def _fx_unittest_manifest_impl(ctx):
    runner_shard = _infer_runner_shard(ctx.attr.binary)

    # TODO(https://fxbug.dev/563978780): Collect shards from `expect_includes`
    # dependencies once `expect_includes` is implemented in Bazel.
    includes = [
        runner_shard,
        "syslog/use.shard.cml",
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
    doc = "Generates a component manifest (.cml) for a unit test binary.",
    implementation = _fx_unittest_manifest_impl,
    attrs = {
        "binary": attr.label(
            doc = "The unit test executable target.",
            mandatory = True,
            executable = True,
            cfg = "target",
        ),
        "binary_path": attr.string(
            doc = "The packaged path of the binary relative to the package root.",
            mandatory = True,
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

    generated_manifest_name = name + ".generated_manifest"
    _fx_unittest_manifest(
        name = generated_manifest_name,
        testonly = True,
        binary = binary,
        binary_path = _TEST_BINARY_INSTALL_ROOT + binary_name,
        tags = tags + ["manual"],
    )

    manifest_name = name + ".manifest"
    fx_component_manifest(
        name = manifest_name,
        testonly = True,
        component_name = component_name,
        includes = [
            "//sdk/lib/syslog:use.shard.cml",
            # The test runner shard is inferred from the binary target's
            # providers during the analysis phase of `_fx_unittest_manifest`,
            # whereas `fx_component_manifest` resolves its `includes` labels at
            # macro expansion time. Since `cmc compile` runs in a sandbox where
            # only declared `includes` are staged as inputs, we must list all
            # possible runner shards that `_fx_unittest_manifest` might emit.
            "//sdk/lib/sys/testing:elf_test_runner.shard.cml",
            "//src/sys/test_runners/gtest:default.shard.cml",
            "//src/sys/test_runners/gtest:zxtest.shard.cml",
            "//src/sys/test_runners/rust:default.shard.cml",
        ],
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
