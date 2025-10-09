# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK atom."""

load("@fuchsia_build_info//:args.bzl", "warn_on_sdk_changes")
load("//build/bazel/bazel_idk:providers.bzl", "FuchsiaIdkAtomInfo")
load("//build/bazel/rules:current_platform_info.bzl", "CurrentPlatformInfo")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load(":idk_common.bzl", "get_allowlist_target")

visibility(["//build/bazel/bazel_idk/..."])

_TYPES_SUPPORTING_UNSTABLE_ATOMS = [
    # LINT.IfChange(unstable_atom_types)
    "cc_source_library",
    "fidl_library",

    # LINT.ThenChange(//build/sdk/sdk_atom.gni:unstable_atom_types, //build/sdk/generate_idk/__init__.py:unstable_atom_types, //build/sdk/generate_prebuild_idk/idk_generator.py)
]
_TYPES_NOT_REQUIRING_COMPATIBILITY = [
    # LINT.IfChange(non_compatibility_types)
    "bind_library",
    "companion_host_tool",
    "dart_library",
    "data",
    "documentation",
    "experimental_python_e2e_test",
    "ffx_tool",
    "host_tool",
    "package",
    "version_history",
    # LINT.ThenChange(//build/sdk/sdk_atom.gni:non_compatibility_types)
]

def _get_current_cpu_arch(ctx):
    """Returns the CPU architecture of the current build."""
    current_platform = ctx.attr._current_platform[CurrentPlatformInfo]
    return current_platform.cpu

def _get_prebuilt_libraries_dir_name(cpu_arch, target_api_level):
    """Returns the IDK directory name for prebuilt libraries."""
    if (target_api_level == "PLATFORM"):
        return cpu_arch
    else:
        return "%s-api-%s" % (cpu_arch, target_api_level)

def _get_prebuilt_libraries_base_path(cpu_arch, target_api_level):
    """Returns the base path in the IDK for prebuilt libraries."""
    if (target_api_level == "PLATFORM"):
        return "arch/%s" % _get_prebuilt_libraries_dir_name(cpu_arch, target_api_level)
    else:
        return "obj/%s" % _get_prebuilt_libraries_dir_name(cpu_arch, target_api_level)

def _compute_atom_api_impl(ctx):
    args = ctx.actions.args()
    args.add("--output", ctx.outputs.generated_api_file.path)

    for dest_path, source_target in ctx.attr.api_contents_map.items():
        source_label = source_target.label
        source_path = source_label.package + "/" + source_label.name

        # `add()` supports at most two parameters, so add the third separately.
        args.add("--file", dest_path)
        args.add(source_path)

    # `ctx.files.api_contents_map` contains just the source files.
    inputs_depset = depset(ctx.files.api_contents_map)

    ctx.actions.run(
        outputs = [ctx.outputs.generated_api_file],
        inputs = inputs_depset,
        executable = ctx.executable._script,
        arguments = [args],
        mnemonic = "ComputeAtomApi",
        progress_message = "Computing API for %s" % ctx.outputs.generated_api_file.short_path,
    )

    return [DefaultInfo(files = depset([ctx.outputs.generated_api_file]))]

_compute_atom_api = rule(
    doc = "Computes the contents of the .api file for an atom.",
    implementation = _compute_atom_api_impl,
    provides = [DefaultInfo],
    attrs = {
        "api_contents_map": attr.string_keyed_label_dict(
            doc = "A dictionary of files that make up the API for this atom, " +
                  "mapping the destination path of a file relative to the " +
                  "IDK  root to its source file label.",
            mandatory = True,
            allow_empty = False,
            default = {},
            allow_files = True,
        ),
        "generated_api_file": attr.output(
            mandatory = True,
            doc = "The output API file.",
        ),
        "_script": attr.label(
            default = Label("//build/sdk:compute_atom_api"),
            executable = True,
            cfg = "exec",
        ),
    },
)

def _get_additional_info(ctx):
    """Adds additional fields to `files_map` and `additional_prebuild_info` if appropriate.

    Some prebuild info can only be obtained inside the rule implementation. This
    function adds such information to the `files_map` and
    `additional_prebuild_info` attributes and returns the result.

    Returns:
        A tuple containing an updated version of `files_map` and `additional_prebuild_info`.
    """

    if ctx.attr.underlying_library:
        # Assume the atom is a C++ prebuilt library.
        # If changing this, also change
        # //build/sdk/idk_prebuild_manifest.gni:cc_prebuilt_library.

        # The first file in DefaultInfo is the final binary.
        first_output_file = ctx.attr.underlying_library[DefaultInfo].files.to_list()[0]
        lib_name = first_output_file.basename

        cpu_arch = _get_current_cpu_arch(ctx)

        # TODO(https://fxbug.dev/443825617): Use the build setting once available.
        target_api_level = "PLATFORM"
        idk_prebuilt_base = _get_prebuilt_libraries_base_path(cpu_arch, target_api_level)

        binaries = {}
        binaries["api_level"] = target_api_level
        binaries["arch"] = cpu_arch

        additional_prebuild_info = dict(ctx.attr.additional_prebuild_info)
        files_map = dict(ctx.attr.files_map)
        format = json.decode(additional_prebuild_info["format"])

        link_lib_dest_dir = "%s/lib" % idk_prebuilt_base
        link_lib_dest = "%s/%s" % (link_lib_dest_dir, lib_name)

        if format == "shared":
            debug_lib_dest = "%s/debug/%s" % (idk_prebuilt_base, lib_name)
            dist_lib_dest = "%s/dist/%s" % (idk_prebuilt_base, lib_name)

            # TODO(https://fxbug.dev/421888626): Once an unstripped binary is
            # exposed, specify the correct label to `files_map`.
            files_map[debug_lib_dest] = first_output_file
            binaries["debug_lib"] = debug_lib_dest

            # For shared libraries, the final binary is the dist_lib.
            files_map[dist_lib_dest] = first_output_file
            binaries["dist_lib"] = dist_lib_dest
            binaries["dist_path"] = "lib/%s" % lib_name

            # TODO(https://fxbug.dev/449812165): Once the link stub is exposed,
            # specify the correct label to `files_map`.
            # a link stub is exposed.
            files_map[link_lib_dest] = first_output_file
            binaries["link_lib"] = link_lib_dest

            # TODO(https://fxbug.dev/449812165): Once the IFS file is exposed,
            # get the `ifs_name` from it and specify the correct label to `files_map`.
            ifs_name = lib_name[:-2] + "ifs"
            ifs_dest = "%s/%s" % (link_lib_dest_dir, ifs_name)
            files_map[ifs_dest] = first_output_file
            binaries["ifs"] = ifs_dest

        elif format == "static":
            # For static libraries, the final binary is the link_lib.
            files_map[link_lib_dest] = first_output_file
            binaries["link_lib"] = link_lib_dest

        else:
            fail("Unrecognized `format` '%s'." % format)

        additional_prebuild_info["binaries"] = json.encode(binaries)

        return additional_prebuild_info, files_map

    else:
        return ctx.attr.additional_prebuild_info, ctx.attr.files_map

def _create_idk_atom_impl(ctx):
    if not ctx.attr.name.endswith("_idk"):
        fail("IDK atom names must end with `_idk`.")

    if (not ctx.attr.api_file_path) != (not ctx.attr.api_contents_map):
        fail("`api_file_path` and `api_contents_map` must be specified together.")

    all_deps_depset = depset(
        direct = ctx.files.idk_deps + ctx.files.underlying_library + ctx.files.atom_build_deps,
    )
    idk_deps = ctx.attr.idk_deps

    additional_prebuild_info, files_map = _get_additional_info(ctx)

    return [
        DefaultInfo(files = all_deps_depset),
        FuchsiaIdkAtomInfo(
            label = ctx.label,
            idk_name = ctx.attr.idk_name,
            id = ctx.attr.id,
            meta_dest = ctx.attr.meta_dest,
            type = ctx.attr.type,
            category = ctx.attr.category,
            is_stable = ctx.attr.stable,
            api_area = ctx.attr.api_area,
            api_file_path = ctx.attr.api_file_path,
            api_contents_map = ctx.attr.api_contents_map,
            atom_files_map = files_map,
            idk_deps = idk_deps,
            atoms_depset = depset(
                direct = idk_deps,
                transitive = [dep[FuchsiaIdkAtomInfo].atoms_depset for dep in idk_deps],
            ),
            atom_build_deps = ctx.attr.atom_build_deps,
            additional_prebuild_info = additional_prebuild_info,
        ),
    ]

_create_idk_atom = rule(
    doc = """Define an IDK atom. Do not instantiate directly - use `idk_atom()` instead.

`name` must end in `_idk`.

If `underlying_library` is specified, information from it will be added to the
provided `files_map` and `additional_prebuild_info`.

Atoms will be checked for category and API area violations when generating the IDK (see `generate_idk`).
""",
    implementation = _create_idk_atom_impl,
    provides = [FuchsiaIdkAtomInfo],
    attrs = {
        "idk_name": attr.string(
            doc = "Name of this atom within the IDK.",
            mandatory = True,
        ),
        "id": attr.string(
            doc = "Identifier of this atom within the IDK. " +
                  "The identifier should represent the canonical base path of the element within " +
                  "the IDK according to the standard layout (https://fuchsia.dev/fuchsia-src/development/idk/layout.md)." +
                  "For an element at $ROOT/pkg/foo, the id should be `sdk://pkg/foo`.",
            mandatory = True,
        ),
        "meta_dest": attr.string(
            doc = "The path of the metadata file (usually `meta.json`) in the final IDK, relative to the IDK root.",
            mandatory = True,
        ),
        "type": attr.string(
            doc = "Type of the atom. Used to determine schema for this file. " +
                  "Metadata files are hosted under `//build/sdk/meta`. " +
                  'If the metadata conforms to `//build/sdk/meta/foo.json`, the present attribute should have a value of "foo".',
            mandatory = True,
        ),
        "category": attr.string(
            doc = """Describes the availability of the element.
Possible values, from most restrictive to least restrictive:
    - compat_test : May be used to configure and run CTF tests but may not be exposed for use
                    in production in the IDK or used by host tools.
    - host_tool   : May be used by host tools (e.g., ffx) provided by the platform organization
                    but may not be used by production code or prebuilt binaries in the IDK.
    - prebuilt    : May be part of the ABI that prebuilt binaries included in the IDK use to
                    interact with the platform.
    - partner     : Included in the IDK for direct use of the API by out-of-tree developers.""",
            mandatory = True,
        ),
        "stable": attr.bool(
            doc = "Whether this atom is stabilized. " +
                  'Must be specified for types "fidl_library" and "cc_source_library" and otherwise unspecified. ' +
                  "This is only informative. The value must match the `stable` value in the atom metadata specified by `source`/`value`. " +
                  "(That metadata is what controls whether the atom is marked as unstable in the final IDK.)",
            mandatory = True,
        ),
        "api_area": attr.string(
            doc = "The API area responsible for maintaining this atom. " +
                  "See docs/contribute/governance/areas/_areas.yaml for the list of areas. " +
                  '"Unknown" is also a valid option.',
            mandatory = True,
        ),
        "api_file_path": attr.string(
            doc = "Path to the file representing the API canonically exposed by this atom. " +
                  "This file is used to ensure modifications to the API are explicitly acknowledged. " +
                  "If this attribute is set, `api_contents_map` must be set as well. If not specified, no such modification checks are performed.",
            mandatory = False,
            default = "",
        ),
        "api_contents_map": attr.string_keyed_label_dict(
            doc = "A dictionary of files making up the atom's API, mapping the destination path " +
                  "of  a file relative to the IDK root to its source file label. " +
                  "The set of files will be used to verify that the API has not changed locally. " +
                  "This is very roughly approximated by checking whether the files themselves have changed at all." +
                  "Required and must not be empty when when `api_file_path` is set.",
            mandatory = False,
            default = {},
            allow_files = True,
        ),
        "files_map": attr.string_keyed_label_dict(
            doc = "A dictionary of files for this atom, mapping the destination " +
                  "path of a file relative to the IDK root to its source file label.",
            mandatory = False,
            default = {},
            allow_files = True,
        ),
        "idk_deps": attr.label_list(
            providers = [FuchsiaIdkAtomInfo],
            doc = "Bazel labels for other SDK elements this element publicly depends on at build time." +
                  "These labels must point to `_create_idk_atom` targets.",
            mandatory = False,
        ),
        "underlying_library": attr.label(
            providers = [DefaultInfo],
            doc = "The underlying library (e.g., C++ prebuilt library) represented by this atom." +
                  "Information will be extracted from it for prebuild info.",
            mandatory = False,
        ),
        "atom_build_deps": attr.label_list(
            providers = [DefaultInfo],
            doc = "List of dependencies related to building the atom that should not be reflected in IDKs. " +
                  "Mostly useful for code generation and validation.",
            mandatory = False,
        ),
        "additional_prebuild_info": attr.string_dict(
            doc = "A dictionary of type-specific prebuild info for the atom, with values encoded as JSON strings.",
            mandatory = False,
            default = {},
        ),
        "_current_platform": attr.label(
            providers = [CurrentPlatformInfo],
            default = "@//build/bazel:current_platform",
        ),
    },
)

def _idk_atom_impl(
        name,
        type,
        category,
        stable,
        testonly,
        atom_build_deps,
        api_file_path,
        api_contents_map,
        prebuilt_library_format,
        **kwargs):
    if type not in _TYPES_SUPPORTING_UNSTABLE_ATOMS and not stable:
        fail("`stable` must be true unless the type ('%s') is one of %s." % (type, _TYPES_SUPPORTING_UNSTABLE_ATOMS))

    if (not api_file_path) != (not api_contents_map):
        fail("`api_file_path` and `api_contents_map` must be specified together.")

    is_type_not_requiring_compatibility = type in _TYPES_NOT_REQUIRING_COMPATIBILITY
    if stable and not api_file_path and not is_type_not_requiring_compatibility:
        fail("All atoms with types ('%s') requiring compatibility must specify an `api_file_path` unless explicitly unstable." % type)

    # Ensure the atom is in the appropriate allowlist.
    # The attribute is immutable, so create a mutable copy.
    atom_build_deps = list(atom_build_deps)
    atom_build_deps.append(get_allowlist_target(type, category, stable, prebuilt_library_format))

    _verify_api = bool(api_file_path)
    if _verify_api:
        if not api_contents_map:
            fail("`api_contents_map` cannot be empty.")

        generate_api_target_name = "%s_generate_api" % name
        verify_api_target_name = "%s_verify_api" % name

        # GN-generated files generally have `_sdk` from the target name.
        # TODO(https://fxbug.dev/425931839): Change this to `_idk` or drop it
        # once GN is no longer generating such files.
        current_api_file = "%s_sdk.api" % name

        _compute_atom_api(
            name = generate_api_target_name,
            api_contents_map = api_contents_map,
            generated_api_file = current_api_file,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )

        verify_golden_files(
            name = verify_api_target_name,
            candidate_files = [current_api_file],
            golden_files = [api_file_path],
            only_warn_on_changes = warn_on_sdk_changes,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )

        atom_build_deps.append(":%s" % verify_api_target_name)

    _create_idk_atom(
        name = name,
        type = type,
        category = category,
        stable = stable,
        api_file_path = api_file_path,
        api_contents_map = api_contents_map,
        atom_build_deps = atom_build_deps,
        testonly = testonly,
        **kwargs
    )

idk_atom = macro(
    doc = """Generate an IDK atom and ensure proper validation of it.

`name` is the name of the IDK atom target and must end in `_idk`.

Atoms will be checked for category and API area violations when generating the IDK (see `generate_idk`).
""",
    implementation = _idk_atom_impl,
    inherit_attrs = _create_idk_atom,
    attrs = {
        "type": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "stable": attr.bool(
            doc = "See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "atom_build_deps": attr.label_list(
            doc = "See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "api_file_path": attr.string(
            doc = "See _create_idk_atom().",
            default = "",
            configurable = False,
        ),
        "api_contents_map": attr.string_keyed_label_dict(
            doc = "See _create_idk_atom().",
            allow_files = True,
            default = {},
            configurable = False,
        ),
        "prebuilt_library_format": attr.string(
            doc = "See get_allowlist_target().",
            default = "",
            configurable = False,
        ),
    },
)
