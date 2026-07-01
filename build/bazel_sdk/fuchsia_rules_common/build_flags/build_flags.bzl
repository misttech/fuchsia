# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Implementation of the build_flags() rule and related definitions.

See README.md file for full technical details.
"""

#############################################################################
#############################################################################
#####
#####    BuildFlagsInfo
#####

_BUILD_FLAGS_INFO_DOC = """A provider to store compiler and linker flags for C++ and Rust.

Similar to GN configs, except for subtle differences documented in
//build/bazel_sdk/fuchsia_rules_common/build_flags/README.md.

Note also that Bazel targets cannot set 'all_dependent_configs' and 'public_configs', as these
modify the build graphs in ways that Bazel doesn't support. However, most GN use cases are covered
by using Bazel builtin attributes, such as 'defines' or 'includes' in 'cc_library()' which apply to
the target and all its dependents (unlike 'local_defines').

For the other rare cases where these are used in the GN graph, a Bazel-specific solution is
required to implement the same feature.
"""

def _build_flags_info_init(
        *,
        label,
        defines = [],
        cflags = [],
        cflags_c = [],
        cflags_cc = [],
        include_dirs = [],
        ldflags = [],
        lib_dirs = [],
        rustenv = [],
        rustflags = []):
    # Consistency check for rustenv list.
    for item in rustenv:
        equal_pos = item.find("=")
        if equal_pos < 1:
            fail("Invalid rustenv item {}, must follow NAME=value format".format(item))

    return {
        "label": label,
        "defines": defines,
        "cflags": cflags,
        "cflags_c": cflags_c,
        "cflags_cc": cflags_cc,
        "include_dirs": include_dirs,
        "ldflags": ldflags,
        "lib_dirs": lib_dirs,
        "rustenv": rustenv,
        "rustflags": rustflags,
    }

BuildFlagsInfo, _ = provider(
    doc = _BUILD_FLAGS_INFO_DOC,
    # LINT.IfChange(BuildFlagsInfo)
    fields = {
        "label": "(Label) The canonical Label of the build_flags() rule, used for deduplication and debugging.",
        "defines": "(list[string]) A list of macro definitions for C and C++ compile actions (e.g ['FOO=1']).",
        "cflags": "(list[string]) A list of C and C++ compiler flags.",
        "cflags_c": "(list[string]) A list of C compiler flags.",
        "cflags_cc": "(list[string]) A list of C++ compiler flags.",
        "include_dirs": "(list[string]) A list of include directories, relative to the workspace root.",
        "ldflags": "(list[string]) A list of linker flags.",
        "lib_dirs": "(list[string]) A list of library search directories, relative to the workspace root.",
        "rustenv": "(list[string]) A list of strings in the format 'VARNAME=VARVALUE'.",
        "rustflags": "(list[string]) A list of Rust compiler flags.",
    },
    # LINT.ThenChange(
    #    //build/bazel/starlark/expand_build_args.cquery:BuildFlagsInfo,
    #    //build/bazel/starlark/expand_build_args_json.cquery:BuildFlagsInfo,
    # )
    init = _build_flags_info_init,
)

#############################################################################
#############################################################################
#####
#####    build_flags() rule
#####

def _build_flags_impl(ctx):
    cflags = list(ctx.attr.cflags)
    cflags_c = list(ctx.attr.cflags_c)
    cflags_cc = list(ctx.attr.cflags_cc)
    defines = list(ctx.attr.defines)
    include_dirs = list(ctx.attr.include_dirs)
    ldflags = list(ctx.attr.ldflags)
    lib_dirs = list(ctx.attr.lib_dirs)
    rustenv = list(ctx.attr.rustenv)
    rustflags = list(ctx.attr.rustflags)

    # Similar to what GN does with the `configs` argument, `subflags`
    # appends the sub-flags directly to the current item's flags.

    # Hence, there is no way for targets to disable a sub-flag by listing
    # it in disable_build_flags (just like there is no way to remove a sub-config)
    # label from the `configs` list that only includes a label to the parent
    # config()).
    for subtarget in ctx.attr.subflags:
        info = subtarget[BuildFlagsInfo]
        cflags.extend(info.cflags)
        cflags_c.extend(info.cflags_c)
        cflags_cc.extend(info.cflags_cc)
        defines.extend(info.defines)
        include_dirs.extend(info.include_dirs)
        ldflags.extend(info.ldflags)
        lib_dirs.extend(info.lib_dirs)
        rustenv.extend(info.rustenv)
        rustflags.extend(info.rustflags)

    return [
        BuildFlagsInfo(
            label = ctx.label,
            cflags = cflags,
            cflags_c = cflags_c,
            cflags_cc = cflags_cc,
            defines = defines,
            include_dirs = include_dirs,
            ldflags = ldflags,
            lib_dirs = lib_dirs,
            rustenv = rustenv,
            rustflags = rustflags,
        ),
    ]

build_flags = rule(
    doc = "Define a target exposing toolchain build flags to its dependents.",
    implementation = _build_flags_impl,
    provides = [BuildFlagsInfo],
    attrs = {
        "cflags": attr.string_list(
            doc = "List of C and C++ compiler flags.",
            default = [],
        ),
        "cflags_c": attr.string_list(
            doc = "List of C only compiler flags. Always appear after 'cflags' on the command-line.",
            default = [],
        ),
        "cflags_cc": attr.string_list(
            doc = "List of C++ only compiler flags. Always appear after 'cflags' on the command-line.",
            default = [],
        ),
        "defines": attr.string_list(
            doc = "List of C and C++ macro definitions for compile actions",
            default = [],
        ),
        "include_dirs": attr.string_list(
            doc = "List of header search paths, must be relative to the workspace, not the package",
            default = [],
        ),
        "ldflags": attr.string_list(
            doc = "List of linker flags.",
            default = [],
        ),
        "lib_dirs": attr.string_list(
            doc = "List of library search paths, relative to the workspace root.",
            default = [],
        ),
        "rustenv": attr.string_list(
            doc = "List of Rust environment variable definitions, each item should be a 'NAME=value' string",
            default = [],
        ),
        "rustflags": attr.string_list(
            doc = "List of Rust compiler flags.",
            default = [],
        ),
        "subflags": attr.label_list(
            doc = "List of other build_flags() targets whose flags will be appended to this rule's flags.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
    },
)

# Common attributes for all rules that support build_flags().
BUILD_FLAGS_ATTRS_KWARGS = {
    "build_flags": attr.label_list(
        doc = "List of `build_flags()` targets.",
        providers = [BuildFlagsInfo],
        default = [],
    ),
    "disable_build_flags": attr.label_list(
        doc = "List of `build_flags()` targets whose flags should be excluded.",
        providers = [BuildFlagsInfo],
        default = [],
    ),
}

#############################################################################
#############################################################################
#####
#####    BuildFlagsListInfo and compute_final_build_flags()
#####

BuildFlagsListInfo = provider(
    doc = "A provider for a list of BuildFlagsInfo values.",
    fields = {
        "infos": "A list of BuildFlagsInfo values.",
    },
)

def _compute_final_build_flags_from(build_flags_infos, disable_build_flags_infos):
    """Compute the final ordered list of BuildFlags for a given target.

    Args:
      build_flags_infos: A list of BuildFlagsInfo from a 'build_flags' target
        attribute.
      disable_build_flags_infos: A list of BuildFlagsInfo from a 'disable_build_flags' target
        attribute.
    Returns:
      A list of BuildFlagsInfo values.
    """

    # Remove duplicate labels. First label wins, so
    # ["//foo", "//bar", "//foo"] is equivalent to ["//foo", "//bar"]
    # and *not* ["//bar", "//foo"]. This is similar to GN.
    known_labels_set = set()
    for info in disable_build_flags_infos:
        known_labels_set.add(info.label)
    final_infos = []
    for info in build_flags_infos:
        if info.label not in known_labels_set:
            known_labels_set.add(info.label)
            final_infos.append(info)
    return final_infos

compute_final_build_flags = rule(
    doc = "Provides the final ordered list of BuildFlagsInfo values. This is used " +
          "to generate response files for different action types.",
    provides = [BuildFlagsListInfo],
    attrs = BUILD_FLAGS_ATTRS_KWARGS | {
        "target_type": attr.string(
            doc = "The type of target being wrapped.",
            mandatory = True,
            values = ["common", "executable", "shared_library"],
        ),
    },
    implementation = lambda ctx: [
        BuildFlagsListInfo(
            infos = _compute_final_build_flags_from(
                [target[BuildFlagsInfo] for target in ctx.attr.build_flags],
                [target[BuildFlagsInfo] for target in ctx.attr.disable_build_flags],
            ),
        ),
    ],
)

# Constants used to identify the type of actions that require build flags.
# These are NOT the @rules_cc//cc:action_names.bzl names.
# LINT.IfChange(action_kinds)
ACTION_KIND_CPP_COMPILE = "cpp_compile"
ACTION_KIND_C_COMPILE = "c_compile"
ACTION_KIND_CPP_LINK = "cpp_link"

CC_ACTION_KINDS = [
    ACTION_KIND_CPP_COMPILE,
    ACTION_KIND_C_COMPILE,
    ACTION_KIND_CPP_LINK,
]

ACTION_KIND_RUST_COMPILE = "rust_compile"

RUST_ACTION_KINDS = [
    ACTION_KIND_RUST_COMPILE,
]

ACTION_KINDS = CC_ACTION_KINDS + RUST_ACTION_KINDS
# LINT.ThenChange(//build/bazel/scripts/bazel_build_args.py:action_kinds)
