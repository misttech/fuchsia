# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for defining IDK host tools."""

visibility(["//build/bazel/bazel_idk/..."])

def _idk_host_tool_impl(
        name,
        idk_name,
        category,
        api_area,
        implementation_deps,
        output_name,
        target_compatible_with,
        visibility = ["//visibility:private"]):
    if not output_name:
        output_name = idk_name

    # TODO(https://fxbug.dev/442025401): Append "_idk" to `name` when calling `idk_atom()`.
    # Note that Buildifier complains when this target and the underlying
    # binary have the same name. We may want to rename some targets

    pass

# idk_host_tool does nothing in Bazel right now. It exists to facilitate target
# syncing between GN and Bazel.
# TODO(https://fxbug.dev/442025401): Add a proper implementation.
idk_host_tool = macro(
    doc = """Defines a host tool in the IDK.

GN note: Unlike the GN template, `name` should not include "_sdk"/"_idk".""",
    implementation = _idk_host_tool_impl,
    attrs = {
        "idk_name": attr.string(
            doc = """Name of the library in the IDK. Usually matches `name`.
GN equivalent: `sdk_name`""",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Publication level of the library in the IDK. See _create_idk_atom().",
            values = ["partner"],
            mandatory = True,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = """The API area responsible for maintaining this library.
GN equivalent: `sdk_area`""",
            mandatory = True,
        ),
        "implementation_deps": attr.label_list(
            doc = """List of labels this element depends on at build time.
GN equivalent: `deps`.""",
            default = [],
            configurable = False,
        ),
        "output_name": attr.string(
            doc = """The tool's name. Defaults to `idk_name`.
GN note: The default relationship to `idk_name` is different from GN.""",
        ),
        # TODO(https://fxbug.dev/442025401): Consider implementing this within
        # bazel2gn rather than requiring it at each call site.
        "target_compatible_with": attr.label_list(
            doc = "`target_compatible_with = HOST_CONSTRAINTS` must be specified " +
                  "for bazel2gn to generate the correct condition statement.",
            mandatory = True,
            configurable = False,
        ),
    },
)
