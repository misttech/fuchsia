# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for host tools."""

load("@io_bazel_rules_go//go:def.bzl", "go_binary")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("@rules_cc//cc:defs.bzl", "cc_binary")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")

def _fuchsia_less_transition_impl(_settings, _attr):
    # Set build settings to their `build_setting_default` value.
    return {"@//build/bazel:fuchsia_api_level": "PLATFORM"}

# Undoes any Fuchsia-specific build settings by resetting them to their
# `build_setting_default` value.
#
# This is useful for host tools so that they have consistent build settings,
# which avoids building them multiple times (e.g., for IDK prebuilds).
fuchsia_less_transition = transition(
    implementation = _fuchsia_less_transition_impl,
    inputs = [],
    outputs = [
        "@//build/bazel:fuchsia_api_level",
    ],
)

# A rule cannot return an executable created by another rule. To work around
# that, we create a symlink to the actual executable and return that.
#
# The symlink cannot have the same name as the original actual executable
# because then there would be two targets with the same name in the case where
# the build setting is already the default value that the fuchsia-less
# transition would set it to.
def _to_fuchsia_less_config_impl(ctx):
    # The link should be named after the file it links to, which could be
    # different from the target's label name.
    output_file_path = ctx.file.actual.basename

    if ctx.attr.subdirectory_name != "":
        output_file_path = ctx.attr.subdirectory_name + "/" + output_file_path

    output = ctx.actions.declare_file(output_file_path)

    ctx.actions.symlink(output = output, target_file = ctx.file.actual)

    return [DefaultInfo(files = depset([output]), executable = output)]

_to_fuchsia_less_config = rule(
    doc = "Creates a symlink to the actual executable with `fuchsia_less_transition` applied.",
    implementation = _to_fuchsia_less_config_impl,
    attrs = {
        "actual": attr.label(
            mandatory = True,
            allow_single_file = True,
            cfg = fuchsia_less_transition,
        ),
        "subdirectory_name": attr.string(
            doc = "The name of a subdirectory into which the symlink is placed.",
            mandatory = False,
            default = "",
        ),
    },
)

def _cc_binary_host_tool_impl(name, target_compatible_with, testonly, visibility, **kwargs):
    if target_compatible_with != HOST_CONSTRAINTS and target_compatible_with != HOST_OS_CONSTRAINTS:
        fail("`target_compatible_with` must be `%s` or `%s`." % (HOST_CONSTRAINTS, HOST_OS_CONSTRAINTS))

    actual_binary_location = name + ".actual/" + name
    cc_binary(
        name = actual_binary_location,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        # Prevent use of the tool directly without going through the fuchsia-less transition.
        visibility = ["//visibility:private"],
        **kwargs
    )

    _to_fuchsia_less_config(
        name = name,
        actual = actual_binary_location,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

cc_binary_host_tool = macro(
    doc = """A cc_binary to be used as a host tool.

All C++ host tools used during the Fuchsia build should be defined with this
macro to help ensure they are only built once. Rules using the tool should
depend on the `name` label and use `cfg = "exec"`.

For example:
    Define the tool:
        cc_binary_host_tool(
            name = "fidlc",
            srcs = ["cmd/fidlc/main.cc"],
            visibility = ["//visibility:public"],
            deps = [":lib"],
        )

    Then use the tool in a rule:
        "_fidlc": attr.label(
            default = "@//tools/fidl/fidlc:fidlc",
            executable = True,
            cfg = "exec",
        ),

This macro works by ensuring the tool is built in a configuration matching the
default "PLATFORM" build by resetting build settings that may have been changed,
such as the API level, when building IDK prebuilts.

This only works for the single host configuration represented by "exec". Further
work would be needed to support cross-compiling for other host configurations.
""",
    implementation = _cc_binary_host_tool_impl,
    # rules_cc wraps the `native.cc_binary()` rule in a legacy macro, which
    # cannot be used with `inherit_attrs`. Use the rule directly instead.
    inherit_attrs = native.cc_binary,
    attrs = {
        "target_compatible_with": attr.string_list(
            doc = "Standard meaning. Must be `HOST_CONSTRAINTS` or `HOST_OS_CONSTRAINTS`.",
            mandatory = False,
            configurable = False,
            default = HOST_CONSTRAINTS,
        ),
    },
)

# This must be a legacy macro with `**kwargs` because rules_go wraps the
# `go_binary()` rule in a legacy macro, which cannot be used with
# `inherit_attrs` in a symbolic macro.
def go_binary_host_tool(
        name,
        target_compatible_with,
        testonly = False,
        visibility = None,
        **kwargs):
    """A go_binary to be used as a host tool.

    All Go host tools used during the Fuchsia build should be defined with this
    macro to help ensure they are only built once. Rules using the tool should
    depend on the `name` label and use `cfg = "exec"`.

    For example:
        Define the tool:
            go_binary_host_tool(
                name = "some_tool_name",
                srcs = ["path/to/main.go"],
                visibility = ["//visibility:public"],
                deps = [":lib"],
            )

        Then use the tool in a rule:
            "_some_tool_name": attr.label(
                default = "@//path/to:some_tool_name",
                executable = True,
                cfg = "exec",
            ),

    This macro works by ensuring the tool is built in a configuration matching the
    default "PLATFORM" build by resetting build settings that may have been changed,
    such as the API level, when building IDK prebuilts.

    This only works for the single host configuration represented by "exec". Further
    work would be needed to support cross-compiling for other host configurations.

    Args:
        name: The name of the tool.
        target_compatible_with: Standard meaning. Must be `HOST_CONSTRAINTS` or `HOST_OS_CONSTRAINTS`.
        testonly: Standard meaning.
        visibility: Standard meaning.
        **kwargs: Passed to `go_binary()`.
    """
    if target_compatible_with != HOST_CONSTRAINTS and target_compatible_with != HOST_OS_CONSTRAINTS:
        fail("`target_compatible_with` must be `%s` or `%s`." % (HOST_CONSTRAINTS, HOST_OS_CONSTRAINTS))

    actual_binary_location = name + ".actual/" + name
    go_binary(
        name = actual_binary_location,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        # Prevent use of the tool directly without going through the fuchsia-less transition.
        visibility = ["//visibility:private"],
        **kwargs
    )

    _to_fuchsia_less_config(
        name = name,
        actual = actual_binary_location,
        # `go_binary()` from `rules_go` in Bazel places the executable in a
        # subdirectory of the output directory. Replicate this for the symlink.
        subdirectory_name = name + "_",
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

def _rustc_binary_host_tool_impl(
        name,
        crate_name,
        target_compatible_with,
        testonly,
        visibility,
        **kwargs):
    if target_compatible_with != HOST_CONSTRAINTS and target_compatible_with != HOST_OS_CONSTRAINTS:
        fail("`target_compatible_with` must be `%s` or `%s`." % (HOST_CONSTRAINTS, HOST_OS_CONSTRAINTS))

    actual_binary_location = name + ".actual/" + name
    rustc_binary(
        name = actual_binary_location,
        crate_name = crate_name if crate_name else name,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        # Prevent use of the tool directly without going through the fuchsia-less transition.
        visibility = ["//visibility:private"],
        **kwargs
    )

    _to_fuchsia_less_config(
        name = name,
        actual = actual_binary_location,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

rustc_binary_host_tool = macro(
    doc = """A rust_binary to be used as a host tool.

All Rust host tools used during the Fuchsia build should be defined with this
macro to help ensure they are only built once. Rules using the tool should
depend on the `name` label and use `cfg = "exec"`.

For example:
    Define the tool:
        rustc_binary_host_tool(
            name = "some_tool_name",
            srcs = ["path/to/main.rs"],
            visibility = ["//visibility:public"],
            deps = [":lib"],
        )

    Then use the tool in a rule:
        "_some_tool_name": attr.label(
            default = "@//path/to:some_tool_name",
            executable = True,
            cfg = "exec",
        ),

This macro works by ensuring the tool is built in a configuration matching the
default "PLATFORM" build by resetting build settings that may have been changed,
such as the API level, when building IDK prebuilts.

This only works for the single host configuration represented by "exec". Further
work would be needed to support cross-compiling for other host configurations.
""",
    implementation = _rustc_binary_host_tool_impl,
    inherit_attrs = rustc_binary,
    attrs = {
        "crate_name": attr.string(
            doc = "Crate name to use for this target. Defaults to `name`.",
            mandatory = False,
            configurable = False,
        ),
        "target_compatible_with": attr.string_list(
            doc = "Standard meaning. Must be `HOST_CONSTRAINTS` or `HOST_OS_CONSTRAINTS`.",
            mandatory = False,
            configurable = False,
            default = HOST_CONSTRAINTS,
        ),
    },
)
