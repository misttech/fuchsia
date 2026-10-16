# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines the ffx_tool macro for building ffx subtools."""

load("//build/bazel/rules/host:defs.bzl", "rustc_binary_host_tool")
load("//src/developer/ffx/lib/version/build:ffx_apply_version.bzl", "ffx_apply_version")

def _ffx_tool_impl(
        name,
        sdk_target_name,
        sdk_category,
        sdk_area,
        sdk_deps,
        **kwargs):
    if sdk_category and not sdk_target_name:
        fail("Must specify `sdk_target_name` when `sdk_category` is set on ffx_tool.")
    if sdk_category and sdk_category != "partner":
        fail("Unexpected SDK category '{}'.".format(sdk_category))
    if (sdk_area or sdk_deps) and not sdk_category:
        fail("`sdk_area` and `sdk_deps` require `sdk_category` to be set on ffx_tool.")

    rustc_binary_host_tool(
        name = name,
        **kwargs
    )

    ffx_apply_version(
        name = name + "_versioned",
        unversioned_binary = name,
        target_compatible_with = kwargs.get("target_compatible_with"),
        visibility = kwargs.get("visibility"),
    )

ffx_tool = macro(
    doc = """Defines an ffx-compatible subtool.

    Wraps `rustc_binary_host_tool` to create an unversioned binary, applies version info
    to create the main versioned binary.

    Two subtargets are created by this macro:

      - name: The unversioned binary.
      - name + "_versioned": The versioned binary.

    An unversioned binary is created to avoid build cache invalidations. This makes incremental
    builds much faster for users who don't need version information in ffx binaries.

    A versioned binary is created for official builds, SDK distributions, and runtime compatibility
    checks. The version metadata is required by ffx and the daemon to verify compatibility between
    the subtools and the target device.

    TODO(https://fxbug.dev/512640761): Match the support implemented in ffx_tool.gni
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
