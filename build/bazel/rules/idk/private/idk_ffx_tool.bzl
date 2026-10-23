# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for defining IDK ffx subtools."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//src/developer/ffx/build:ffx_tool.bzl", "ffx_tool")
load(":idk_atom.bzl", "idk_atom")
load(
    ":idk_common.bzl",
    "get_atom_visibility",
    "json_encode_dict_values",
    "verify_target_is_in_allowlist",
)

visibility([
    "//build/bazel/rules/idk/...",
])

def _idk_ffx_tool_impl(
        name,
        idk_name,
        category,
        visibility,
        testonly,
        **kwargs):
    if "idk" in name or "sdk" in name:
        fail('`name`s must not include "idk" or "sdk".')

    ffx_tool(
        name = name,
        binary_name = idk_name,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
        testonly = testonly,
        **kwargs
    )

    file_base = "tools"
    idk_path = file_base + "/$<current_cpu>/ffx_tools/%s" % idk_name
    metadata_path = idk_path + ".json"

    additional_prebuild_info_values = {
        "arch": "$<current_cpu>",
        "file_base": file_base,
    }

    atom_type = "ffx_tool"

    # Verify the allowlist here to catch cases where this macro is used but
    # there is no dependency on the atom target.
    verify_target_is_in_allowlist(
        name = name,
        type = atom_type,
        category = category,
        stable = True,
        testonly = testonly,
    )

    idk_atom(
        name = name + "_idk",
        idk_name = idk_name,
        id = "sdk://" + idk_path,
        meta_dest = idk_path + "-meta.json",
        type = atom_type,
        category = category,
        stable = True,
        api_area = "Developer",
        files_map = {
            idk_path: ":" + name + "_versioned",
            metadata_path: ":" + name + "_metadata",
        },
        atom_build_deps = [],
        additional_prebuild_info = json_encode_dict_values(additional_prebuild_info_values),
        visibility = get_atom_visibility(visibility),
        target_compatible_with = HOST_OS_CONSTRAINTS,
        testonly = testonly,
    )

idk_ffx_tool = macro(
    doc = """Defines an `ffx_tool()` subtool in the IDK.

    GN note: Unlike some GN templates, `name` should not include "_sdk"/"_idk".
    """,
    implementation = _idk_ffx_tool_impl,
    inherit_attrs = ffx_tool,
    attrs = {
        "idk_name": attr.string(
            doc = "The name of the ffx tool in the IDK (also used as the versioned binary name).",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Publication level of the tool in the IDK. See _create_idk_atom().",
            values = ["partner"],
            mandatory = True,
            configurable = False,
        ),
        # Do not inherit. These are set internally.
        "binary_name": None,
        "target_compatible_with": None,
    },
)
