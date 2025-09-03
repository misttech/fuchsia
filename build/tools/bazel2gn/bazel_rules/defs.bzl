# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("//build/tools/bazel2gn/bazel_rules:rustc_binary.bzl", _rustc_binary = "rustc_binary")
load("//build/tools/bazel2gn/bazel_rules:rustc_library.bzl", _rustc_library = "rustc_library")

rustc_binary = _rustc_binary
rustc_library = _rustc_library

# idk_host_tool does nothing in Bazel right now. It exists to facilitate target
# syncing between GN and Bazel.
# TODO(https://fxbug.dev/442025401): Add a proper implementation
#
# `target_compatible_with = HOST_CONSTRAINTS` must be specified for bazel2gn to
# generate the correct condition statement.
# TODO(https://fxbug.dev/442025401): Consider implementing this within bazel2gn
# rather than requiring it at each call site.
def idk_host_tool(
        # GN note: Unlike the GN template, this should not include "_sdk"/"_idk".
        # TODO(https://fxbug.dev/442025401): `idk_atom()` will append "_idk" to it.
        name,
        idk_name,
        category,
        api_area,
        # TODO(https://fxbug.dev/442025401): Consider whether
        # `implementation_deps` would be more appropriate for the tool
        # dependency. Use `deps` for IDK atom dependencies if needed.
        implementation_deps,  # GN equivalent: `deps`
        target_compatible_with,
        output_name = None,  # GN note: Defaults to `idk_name` instead of the other way around.
        visibility = ["//visibility:private"]):
    if not output_name:
        output_name = idk_name

    pass

# install_host_tools does nothing in Bazel right now. It exists to facilitate
# target syncing between GN and Bazel.
#
# `target_compatible_with = HOST_CONSTRAINTS` must be specified for bazel2gn to
# generate the correct condition statement.
# TODO(https://fxbug.dev/442025401): Consider implementing this within bazel2gn
# rather than requiring it at each call site.
def install_host_tools(
        name,
        tool_output_names,  # GN equivalent: `outputs`
        implementation_deps,  # GN equivalent: `deps`
        target_compatible_with,
        visibility = ["//visibility:private"]):
    pass
