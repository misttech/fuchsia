# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines the ffx_tool macro for building ffx subtools."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host:defs.bzl", "rustc_binary_host_tool")
load("//build/bazel/rules/host_tests:host_test_data.bzl", "host_test_data_files")
load("//src/developer/ffx/lib/version/build:ffx_apply_version.bzl", "ffx_apply_version")

def _ffx_tool_metadata_impl(ctx):
    output = ctx.actions.declare_file(ctx.attr.output_name)

    args = ctx.actions.args()
    args.add_all([
        "--strict",
        "--machine",
        "raw",
        "--log-output",
        "/dev/null",
        "--config",
        "ffx.analytics.disabled=true,log.enabled=false",
        "metadata",
    ])
    args.add(output)

    ctx.actions.run(
        executable = ctx.executable.tool,
        arguments = [args],
        outputs = [output],
        mnemonic = "FfxToolMetadata",
        progress_message = "Generating ffx tool metadata %s" % output.short_path,
    )

    return [DefaultInfo(files = depset([output]))]

_ffx_tool_metadata = rule(
    doc = "Runs the unversioned ffx subtool binary to emit its `<output_name>.json` metadata.",
    implementation = _ffx_tool_metadata_impl,
    attrs = {
        "tool": attr.label(
            doc = "Unversioned ffx subtool binary.",
            mandatory = True,
            executable = True,
            cfg = "exec",
        ),
        "output_name": attr.string(
            doc = "Filename of the emitted metadata JSON.",
            mandatory = True,
        ),
    },
)

def _ffx_tool_impl(
        name,
        binary_name,
        sdk_target_name,
        sdk_category,
        sdk_area,
        sdk_deps,
        target_compatible_with,
        visibility,
        testonly,
        **kwargs):
    if sdk_category and not sdk_target_name:
        fail("Must specify `sdk_target_name` when `sdk_category` is set on ffx_tool.")
    if sdk_category and sdk_category != "partner":
        fail("Unexpected SDK category '{}'.".format(sdk_category))
    if (sdk_area or sdk_deps) and not sdk_category:
        fail("`sdk_area` and `sdk_deps` require `sdk_category` to be set on ffx_tool.")
    if sdk_category:
        fail("TODO(https://fxbug.dev/512640761): `sdk_category` on `ffx_tool` is not yet supported in Bazel.")

    output_name = binary_name or name
    metadata_name = name + "_metadata"
    versioned_name = name + "_versioned"
    test_data_name = name + "_test_data"
    subtarget_compatible_with = (
        target_compatible_with if target_compatible_with != None else HOST_OS_CONSTRAINTS
    )

    rustc_binary_host_tool(
        name = name,
        binary_name = output_name + "_unversioned",
        target_compatible_with = target_compatible_with,
        visibility = visibility,
        testonly = testonly,
        **kwargs
    )

    _ffx_tool_metadata(
        name = metadata_name,
        tool = ":" + name,
        output_name = output_name + ".json",
        target_compatible_with = subtarget_compatible_with,
        visibility = visibility,
        testonly = testonly,
    )

    ffx_apply_version(
        name = versioned_name,
        unversioned_binary = ":" + name,
        output_name = output_name,
        runtime_files = [":" + metadata_name],
        target_compatible_with = subtarget_compatible_with,
        visibility = visibility,
        testonly = testonly,
    )

    host_test_data_files(
        name = test_data_name,
        srcs = [
            ":" + versioned_name,
            ":" + metadata_name,
        ],
        dest_dir = "host-tools",
        target_compatible_with = subtarget_compatible_with,
        visibility = visibility,
        testonly = testonly,
    )

ffx_tool = macro(
    doc = """Defines an ffx-compatible subtool.

    Wraps `rustc_binary_host_tool` to create an unversioned binary, generates its
    subtool metadata JSON, applies version info to create the versioned binary,
    and exposes host test data.

    Subtargets created by this macro:

      - name: The unversioned binary (`<output_name>_unversioned`).
      - name + "_metadata": The subtool metadata JSON (`<output_name>.json`).
      - name + "_versioned": The versioned binary (`<output_name>`), with `<output_name>.json` in runfiles.
      - name + "_test_data": Host test data placing `<output_name>` and `<output_name>.json` under `host-tools/`.

    An unversioned binary is created to avoid build cache invalidations. This makes incremental
    builds much faster for users who don't need version information in ffx binaries.

    A versioned binary is created for official builds, SDK distributions, and runtime compatibility
    checks. The version metadata is required by ffx and the daemon to verify compatibility between
    the subtools and the target device.

    TODO(https://fxbug.dev/512640761): Match SDK atom support implemented in ffx_tool.gni
    """,
    implementation = _ffx_tool_impl,
    inherit_attrs = rustc_binary_host_tool,
    attrs = {
        "sdk_target_name": attr.string(
            doc = "Target name for the SDK atom definition (required if `sdk_category` is set).",
            configurable = False,
        ),
        "sdk_category": attr.string(
            doc = "SDK category this tool will be installed to (e.g. \"partner\").",
            configurable = False,
        ),
        "sdk_area": attr.string(
            doc = "API area responsible for maintaining this tool.",
            configurable = False,
        ),
        "sdk_deps": attr.label_list(
            doc = "Dependencies for inclusion in the SDK.",
            default = [],
            configurable = False,
        ),
    },
)
