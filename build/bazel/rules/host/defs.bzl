# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for host tools."""

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("@rules_cc//cc:defs.bzl", "cc_binary")

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
    if ctx.attr.name != ctx.attr.actual[0].label.name + "_tool":
        fail("By convention, name must be `%s_tool`, got `%s`" % (
            ctx.attr.actual[0].label.name,
            ctx.attr.name,
        ))

    output = ctx.actions.declare_file(ctx.attr.name)

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
    },
)

def _cc_binary_host_tool_impl(name, target_compatible_with, testonly, visibility, **kwargs):
    if target_compatible_with != HOST_CONSTRAINTS:
        fail("`target_compatible_with` must be `%s`." % HOST_CONSTRAINTS)

    cc_binary(
        name = name,
        target_compatible_with = HOST_CONSTRAINTS,
        testonly = testonly,
        # Prevent use of the tool directly without going through the fuchsia-less transition.
        visibility = ["//visibility:private"],
        **kwargs
    )

    _to_fuchsia_less_config(
        name = name + "_tool",
        actual = name,
        target_compatible_with = HOST_CONSTRAINTS,
        testonly = testonly,
        visibility = visibility,
    )

cc_binary_host_tool = macro(
    doc = """A cc_binary to be used as a host tool.

All C++ host tools used during the Fuchsia build should be defined with this
macro to help ensure they are only built once. Rules using the tool should
depend on the `name + "_tool"` label and use `cfg = "exec"`.

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
            default = "@//tools/fidl/fidlc:fidlc_tool",
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
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_binary()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_binary,
    attrs = {
        # Ideally, `target_compatible_with` would never be specified because
        # we can set the value in the implementation. However, it must be
        # specified for bazel2gn to work.
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
