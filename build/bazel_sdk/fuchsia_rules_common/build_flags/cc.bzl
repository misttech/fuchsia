# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Implementation of the build_flags() rule and related definitions for cc targets.

See README.md file for full technical details.
"""

load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load(
    ":build_flags.bzl",
    "ACTION_KIND_CPP_COMPILE",
    "ACTION_KIND_CPP_LINK",
    "ACTION_KIND_C_COMPILE",
    "BuildFlagsListInfo",
    "CC_ACTION_KINDS",
    "compute_final_build_flags",
    _BUILD_FLAGS_ATTRS_KWARGS = "BUILD_FLAGS_ATTRS_KWARGS",
)

# Re-export for callers' convenience.
BUILD_FLAGS_ATTRS_KWARGS = _BUILD_FLAGS_ATTRS_KWARGS

#############################################################################
#############################################################################
#####
#####    compute_build_flags_for_cc_action() and _cc_response_file()
#####

# NOTE: Intentionally public to make it usable by the Bazel C++ toolchain
# feature() implementation.
def compute_build_flags_for_cc_action(build_flags_infos, action_kind):
    """Compute the list of build flags for a given action kind.

    Args:
        build_flags_infos: A list of BuildFlagsInfo providers.
        action_kind: The kind of action to compute build flags for.
          Must be one of the ACTION_KIND_XXX constants defined in this module.

    Returns:
        A string list containing the build flags for the given action kind.
    """
    result = []
    if action_kind == ACTION_KIND_CPP_COMPILE:
        # NOTE: The GN toolchain definition in clang_toolchain() uses
        # {{defines}} {{include_dirs}} {{cflags}} {{cflags_cc}}
        for info in build_flags_infos:
            result.extend(["-D{}".format(define) for define in info.defines])
        for info in build_flags_infos:
            result.extend(["-I{}".format(include_dir) for include_dir in info.include_dirs])
        for info in build_flags_infos:
            result.extend(info.cflags)
        for info in build_flags_infos:
            result.extend(info.cflags_cc)
    elif action_kind == ACTION_KIND_C_COMPILE:
        # NOTE: The GN toolchain definition in clang_toolchain() uses
        # {{defines}} {{include_dirs}} {{cflags}} {{cflags_c}}
        for info in build_flags_infos:
            result.extend(["-D{}".format(define) for define in info.defines])
        for info in build_flags_infos:
            result.extend(["-I{}".format(include_dir) for include_dir in info.include_dirs])
        for info in build_flags_infos:
            result.extend(info.cflags)
        for info in build_flags_infos:
            result.extend(info.cflags_c)
    elif action_kind == ACTION_KIND_CPP_LINK:
        for info in build_flags_infos:
            result.extend(["-L{}".format(lib_dir) for lib_dir in info.lib_dirs])
            result.extend(info.ldflags)
    else:
        fail("Unsupported C++ action kind {}, must be one of: {}".format(
            action_kind,
            ", ".join(CC_ACTION_KINDS),
        ))
    return result

def _cc_response_file_internal_impl(ctx):
    build_flags_infos = ctx.attr.final_build_flags[BuildFlagsListInfo].infos
    flags = compute_build_flags_for_cc_action(build_flags_infos, ctx.attr.action_kind)
    output = ctx.actions.declare_file(ctx.label.name)
    args = ctx.actions.args()
    args.set_param_file_format("shell")
    args.add_all(flags)
    ctx.actions.write(output, args)

    # Ensure the response files are part of the compiler sandbox by
    # making them available through the CcInfo provider. A cleaner way
    # would be to use additional_compiler_inputs, but this attribute is
    # only available from Bazel 9.1. See TODO(https://fxbug.dev/1650836)
    # and related comment below in wrap_cc_rule_with_build_flags().
    compilation_context = cc_common.create_compilation_context(
        headers = depset([output]),
    )
    return [
        DefaultInfo(files = depset([output])),
        CcInfo(compilation_context = compilation_context),
    ]

_cc_response_file_internal = rule(
    doc = "Generate a response file containing build flags for a given C++ action kind.",
    implementation = _cc_response_file_internal_impl,
    attrs = {
        "action_kind": attr.string(
            doc = "The kind of C++ action to generate build flags for.",
            mandatory = True,
            values = CC_ACTION_KINDS,
        ),
        "final_build_flags": attr.label(
            doc = "A _compute_final_build_flags() target label.",
            mandatory = True,
            providers = [BuildFlagsListInfo],
        ),
    },
)

def _cc_response_file(target_name, action_kind, final_build_flags, testonly):
    """Generate a new response file containing build_flags() flags for a given action kind.

    Args:
        target_name: Name of the wrapped target that will use the response file.
        action_kind: The kind of C++ action to generate a response file for.
        final_build_flags: The label of a _compute_final_build_flags() target to use.
        testonly: Whether the response file target should be testonly.

    Returns:
        The label of the new response file target.
    """
    name = "{}.{}.build_flags".format(target_name, action_kind)
    _cc_response_file_internal(
        name = name,
        action_kind = action_kind,
        final_build_flags = final_build_flags,
        testonly = testonly,
    )
    return name

#############################################################################
#############################################################################
#####
#####    wrap_cc_macro_args_with_build_flags()
#####

_VALID_TARGET_TYPES = ["common", "executable", "shared_library"]

def wrap_cc_macro_args_with_build_flags(
        *,
        kwargs,
        name,
        cc_rule_name,  # buildifier: disable=unused-variable
        build_flags,
        disable_build_flags,
        target_type):
    """Wrap the keyword arguments of a given `cc_xxx()` wrapping macro.

    This is useful in macros that need to wrap regular `cc_xxx()` rules
    to add support for `build_flags` and `disable_build_flags`.

    IMPORTANT: The input 'kwargs' must include all attributes supported by the
    wrapped `cc_xxx()` rule for the wrapping to work correctly.

    Example usage:
        ```
        def my_cc_library(
            name,
            build_flags = [],
            disable_build_flags = [],
            **kwargs,
        ):
            new_kwargs = wrap_cc_macro_args_with_build_flags(
                kwargs = kwargs,
                name = name,
                cc_rule_name = "cc_library",
                build_flags = build_flags,
                disable_build_flags = disable_build_flags,
                target_type = "common",
            )

            cc_library(
                name = name,
                **new_kwargs,
            )
        ```

    Args:
       kwargs: (dict) Keyword-argument dictionary of wrapper macro.
       name: (string) Target name
       cc_rule_name: (string) Name of wrapped cc_xxxx() rule (used for debugging)
       build_flags: (list[label]) List of build_flags() labels.
       disable_build_flags: (list[label]) List of build_flags() labels to disable.
       target_type: (string) The type of target being wrapped, must
          be one of "common", "executable" or "shared_library".
    Returns:
       (dict) A new keyword-argument with updated values.
    """

    if target_type not in _VALID_TARGET_TYPES:
        fail("Invalid target_type value ({}), should be one of: {}".format(
            target_type,
            ", ".join(_VALID_TARGET_TYPES),
        ))

    # NOTE: The following is commented out because build_flags and disable_build_flags are
    # configurable attributes, their default value is something like
    # select({"//conditions:default": []}) and there is no way to look
    # inside the select() statement to exit early if nothing is selected.
    #
    #if not build_flags and not disable_build_flags: return kwargs

    testonly = kwargs.get("testonly", False)

    # Compute the final set of build flags for this target.
    final_build_flags_name = name + ".final_build_flags"
    compute_final_build_flags(
        name = final_build_flags_name,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = target_type,
        testonly = testonly,
    )

    cxx_response_name = _cc_response_file(
        target_name = name,
        action_kind = ACTION_KIND_CPP_COMPILE,
        final_build_flags = final_build_flags_name,
        testonly = testonly,
    )

    conly_response_name = _cc_response_file(
        target_name = name,
        action_kind = ACTION_KIND_C_COMPILE,
        final_build_flags = final_build_flags_name,
        testonly = testonly,
    )

    # Add the response files to the corresponding action attributes.
    result = dict(kwargs)

    result["cxxopts"] = ["@$(location {})".format(cxx_response_name)] + (kwargs.get("cxxopts") or [])
    result["conlyopts"] = ["@$(location {})".format(conly_response_name)] + (kwargs.get("conlyopts") or [])

    # TODO(https://fxbug.dev/1650836). Use "additional_compiler_inputs" when Bazel 9.1 or higher
    # is used (Fuchsia hasn't upgraded yet). In the meantime, the workaround is to export CcInfo
    # providers from the response file targets and add them to 'deps'.
    result["deps"] = (kwargs.get("deps") or []) + [
        ":" + cxx_response_name,
        ":" + conly_response_name,
    ]

    # Only generate and apply linker flags for targets that actually link (executables and shared libraries)
    if target_type != "common":
        link_response_name = _cc_response_file(
            target_name = name,
            action_kind = ACTION_KIND_CPP_LINK,
            final_build_flags = final_build_flags_name,
            testonly = testonly,
        )
        result["linkopts"] = ["@$(location {})".format(link_response_name)] + (kwargs.get("linkopts") or [])
        result["additional_linker_inputs"] = (kwargs.get("additional_linker_inputs") or []) + [
            ":" + link_response_name,
        ]

    return result
