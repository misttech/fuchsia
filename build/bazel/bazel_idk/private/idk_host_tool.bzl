# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for defining IDK host tools."""

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load(":idk_atom.bzl", "idk_atom")
load(
    ":idk_common.bzl",
    "get_atom_visibility",
    "json_encode_dict_values",
)

visibility(["//build/bazel/bazel_idk/..."])

def _idk_host_tool_impl(
        name,
        idk_name,
        category,
        api_area,
        tool,
        output_name,
        target_compatible_with,
        visibility):
    if target_compatible_with != HOST_CONSTRAINTS:
        fail("`target_compatible_with` must be `%s`." % HOST_CONSTRAINTS)

    if not output_name:
        output_name = idk_name

    file_base = "tools"
    idk_path = file_base + "/$<current_cpu>/%s" % idk_name

    additional_prebuild_info_values = {
        "file_base": file_base,
    }

    idk_atom(
        name = name,
        idk_name = idk_name,
        id = "sdk://" + idk_path,
        meta_dest = idk_path + "-meta.json",
        type = "host_tool",
        category = category,
        stable = True,
        api_area = api_area,
        files_map = {idk_path: tool},
        atom_build_deps = [],
        additional_prebuild_info = json_encode_dict_values(additional_prebuild_info_values),
        visibility = get_atom_visibility(visibility),
        target_compatible_with = HOST_CONSTRAINTS,
    )

_idk_host_tool = macro(
    doc = """Defines a host tool in the IDK.

GN note: Unlike the GN template, `name` should not include "_sdk"/"_idk".""",
    implementation = _idk_host_tool_impl,
    attrs = {
        "idk_name": attr.string(
            doc = """Name of the tool in the IDK. Usually matches `name`.
GN equivalent: `sdk_name`""",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Publication level of the tool in the IDK. See _create_idk_atom().",
            values = ["partner"],
            mandatory = True,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = """The API area responsible for maintaining this tool.
GN equivalent: `sdk_area`""",
            mandatory = True,
        ),
        "tool": attr.label(
            doc = """Label of the tool to be added to the IDK.
It will be built with `cfg = "exec"`.
GN equivalent: `deps`.""",
            allow_single_file = True,
            mandatory = True,
            configurable = False,
            cfg = "exec",
        ),
        # TODO(https://fxbug.dev/425931839): Remove if unused after all tools are migrated.
        "output_name": attr.string(
            doc = """The tool's name. Defaults to `idk_name`.
GN note: The default relationship to `idk_name` is different from GN.""",
        ),
        # TODO(https://fxbug.dev/442025401): Consider implementing this within
        # bazel2gn rather than requiring it at each call site.
        # TODO(https://fxbug.dev/460538634): Replace with the following once
        # bazel2gn is no longer being used for host tools.
        # "target_compatible_with": None,
        "target_compatible_with": attr.string_list(
            doc = "Standard meaning. Must be `HOST_CONSTRAINTS`.",
            mandatory = False,
            configurable = False,
            default = HOST_CONSTRAINTS,
        ),
    },
)

# A wrapper that appends "_idk" to the name. This avoids duplicate name errors
# that could occur if using the symbolic macro above directly.
# TODO(https://fxbug.dev/442025401): Consider removing this or the
# language-specific versions after migrating all tools to macros like
# `cc_binary_host_tool()`.
def idk_host_tool(name, tool, **kwargs):
    """Defines a host tool in the IDK.

    GN note: Unlike the GN template, `name` should not include "_sdk"/"_idk".

    Args:
        name: The name of the tool binary.
        tool: A list containing a single label of the tool binary.
        **kwargs: See `_idk_host_tool()` for details.
    """
    _idk_host_tool(name = name + "_idk", tool = tool[0], **kwargs)

# A wrapper that appends "_idk" to the name. This avoids duplicate name errors
# that could occur if using the symbolic macro above directly.
# It also handles converting `tool` from something compatible with bazel2gn
# conversion to what the macro expects, including converting a target string
# before it becomes a label.
def idk_cc_binary_host_tool(name, tool, **kwargs):
    """Defines a host tool in the IDK for a `cc_binary()` tool.

    Args:
        name: The name of the tool binary.
        tool: A list containing a single label of the tool binary.
        **kwargs: See `_idk_host_tool()` for details.

    GN note: Unlike the GN template, `name` should not include "_sdk"/"_idk".
    """
    _idk_host_tool(name = name + "_idk", tool = tool[0] + "_tool", **kwargs)
