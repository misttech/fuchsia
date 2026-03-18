# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK atom."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_build_info//:args.bzl", "warn_on_sdk_changes")
load("//build/bazel/bazel_idk:providers.bzl", "FuchsiaIdkAtomInfo")
load("//build/bazel/rules:current_platform_info.bzl", "CurrentPlatformInfo")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load("//build/bazel/rules/cc:providers.bzl", "PrebuiltLibraryInfo")
load(":idk_common.bzl", "get_allowlist_target", "get_atom_visibility")

ConfigurableInfo = provider(
    doc = "Maps of IDK destination paths to source files.",
    fields = {
        "api_contents_map": "A dictionary of files making up the atom's API, mapping the destination path " +
                            "of  a file relative to the IDK root to its source file label. " +
                            "Used instead of the attribute of the same name. " +
                            "May be empty.",
        "api_contents_map_files": "The `Files` corresponding to the labels in `api_contents_map`. " +
                                  "Must be the same length and in the same order as `api_contents_map`.",
        "files_map": "A dictionary of files for this atom, mapping the destination " +
                     "path of a file relative to the IDK root to its source file label . " +
                     "Used instead of the attribute of the same name. " +
                     "May be empty.",
        "additional_prebuild_info_values": "A dictionary of type-specific prebuild info for the atom, with values encoded as JSON strings. " +
                                           "Merged with values from the `additional_prebuild_info` attribute.",
    },
)

visibility(["//build/bazel/bazel_idk/...", "//build/bazel/rules/fidl/..."])

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

    # Locate the map to use.
    if ctx.attr.configurable_info:
        if ctx.attr.api_contents_map:
            fail("`api_contents_map` and `configurable_info` must not be both set at the same time.")
        configurable_info = ctx.attr.configurable_info[ConfigurableInfo]
        if not configurable_info.api_contents_map:
            fail("The `api_contents_map` field in `configurable_info` must not be empty.")
        map = configurable_info.api_contents_map
        files = configurable_info.api_contents_map_files
    else:
        if not ctx.attr.api_contents_map:
            fail("The `api_contents_map` attribute must not be empty.")
        map = ctx.attr.api_contents_map
        files = ctx.files.api_contents_map

    # We must use `File` objects to ensure we can get the full path to the source
    # files, especially for generated files as is the case for FIDL atoms.
    for dest_path, source_file in zip(map.keys(), files):
        # `add()` supports at most two parameters, so add the third separately.
        args.add("--file", dest_path)
        args.add(source_file.path)

    inputs_depset = depset(files)

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
                  "IDK root to its source file label." +
                  "Either this or `configurable_info` must be specified. " +
                  "Must be non-empty when specified.",
            mandatory = False,
            allow_empty = True,
            default = {},
            allow_files = True,
        ),
        "configurable_info": attr.label(
            doc = "Information about the atom that is configurable and thus may contain `select()` statements. " +
                  "Either this or `api_contents_map` must be specified. " +
                  "The `api_contents_map` field must be non-empty when specified.",
            providers = [ConfigurableInfo],
            mandatory = False,
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

def _replace_placeholders(input_str, ctx):
    """Replaces placeholders in `input_str` with values from `ctx`.

    Supported placeholders:
        $<current_cpu>: The current CPU architecture.
    """
    return input_str.replace("$<current_cpu>", _get_current_cpu_arch(ctx))

def _replace_placeholders_in_map(input_map, ctx):
    """Replaces placeholders in the values of `input_map` with values from `ctx`.

    See `_replace_placeholders()` for details.
    """
    output_map = {}
    for key, value in input_map.items():
        output_map[_replace_placeholders(key, ctx)] = value
    return output_map

def _get_additional_info(ctx, files_map, additional_prebuild_info):
    """Adds additional fields to `files_map` and `additional_prebuild_info` if appropriate.

    Some prebuild info can only be obtained inside the rule implementation. This
    function adds such information to the `files_map` and
    `additional_prebuild_info` attributes and returns the result.

    Returns:
        A tuple containing an updated version of `files_map` and `additional_prebuild_info`.
    """

    if ctx.attr.underlying_library:
        # If changing this, also change
        # //build/sdk/idk_prebuild_manifest.gni:cc_prebuilt_library.

        api_level = ctx.attr._current_api_level[BuildSettingInfo].value
        cpu_arch = _get_current_cpu_arch(ctx)
        idk_prebuilt_base = _get_prebuilt_libraries_base_path(cpu_arch, api_level)

        binaries = {}
        binaries["api_level"] = api_level
        binaries["arch"] = cpu_arch

        files_map = dict(files_map)

        lib_info = ctx.attr.underlying_library[PrebuiltLibraryInfo]
        library_type = lib_info.type

        if library_type != json.decode(additional_prebuild_info["format"]):
            fail("Library type `%s` does not match format `%s`." %
                 (library_type, additional_prebuild_info["format"]))

        # All types have a link library.
        link_lib_dest_dir = "%s/lib" % idk_prebuilt_base
        link_lib_dest = "%s/%s" % (link_lib_dest_dir, lib_info.link_lib.basename)
        files_map[link_lib_dest] = lib_info.link_lib
        binaries["link_lib"] = link_lib_dest

        if library_type == "shared":
            # The stripped IFS file removes text that should not be exposed
            # (e.g., undefined symbols) or that can vary by architecture.
            # TODO(https://fxbug.dev/417307356): Use the correct destination
            # path for the unversioned IFS file built at "PLATFORM".
            ifs_dest = "%s/%s" % (link_lib_dest_dir, lib_info.stripped_ifs_file.basename)
            files_map[ifs_dest] = lib_info.stripped_ifs_file
            binaries["ifs"] = ifs_dest

            debug_lib_dest = "%s/debug/%s" % (idk_prebuilt_base, lib_info.debug.basename)
            files_map[debug_lib_dest] = lib_info.debug
            binaries["debug_lib"] = debug_lib_dest

            dist_lib_dest = "%s/dist/%s" % (idk_prebuilt_base, lib_info.stripped.basename)
            files_map[dist_lib_dest] = lib_info.stripped
            binaries["dist_lib"] = dist_lib_dest
            binaries["dist_path"] = "lib/%s" % lib_info.stripped.basename

        elif library_type == "static":
            if (hasattr(lib_info, "ifs_file") or
                hasattr(lib_info, "debug") or
                hasattr(lib_info, "stripped")):
                fail("Files unexpected for a static library were provided.")

        else:
            fail("Unrecognized library_type '%s'." % library_type)

        if "binaries" in additional_prebuild_info:
            fail("`binaries` should not already be populated in `additional_prebuild_info`.")
        additional_prebuild_info = dict(additional_prebuild_info)
        additional_prebuild_info["binaries"] = json.encode(binaries)

        return additional_prebuild_info, files_map

    else:
        return additional_prebuild_info, files_map

def _get_file_maps(ctx):
    """Returns the `api_contents_map` and `files_map` to use for the rule.

    If `ctx.attr.configurable_info` is set, returns the maps from there.
    Otherwise, returns the maps from `ctx.attr.api_contents_map` and
    `ctx.attr.files_map`. A list of `Files` corresponding to the labesl in
    `files_map` is also returned.
    """
    if ctx.attr.configurable_info:
        if ctx.attr.api_contents_map:
            fail("`api_contents_map` and `configurable_info` must not be both set at the same time.")
        if ctx.attr.files_map:
            fail("`files_map` and `configurable_info` must not be both set at the same time.")

        configurable_info = ctx.attr.configurable_info[ConfigurableInfo]

        if not configurable_info.files_map:
            fail("`files_map` in `configurable_info` must not be empty.")

        api_contents_map = configurable_info.api_contents_map
        files_map = configurable_info.files_map
        atom_files_for_depset = ctx.attr.configurable_info[DefaultInfo].files.to_list()
    else:
        api_contents_map = ctx.attr.api_contents_map
        files_map = ctx.attr.files_map
        atom_files_for_depset = ctx.files.files_map

    return api_contents_map, files_map, atom_files_for_depset

def _create_idk_atom_impl(ctx):
    if not ctx.attr.name.endswith("_idk"):
        fail("IDK atom names must end with `_idk`.")

    # Merge additional prebuild info dictionaries if necessary.
    additional_prebuild_info = ctx.attr.additional_prebuild_info
    if ctx.attr.configurable_info:
        additional_prebuild_info = dict(additional_prebuild_info)
        additional_prebuild_info.update(ctx.attr.configurable_info[ConfigurableInfo].additional_prebuild_info_values)

    # Locate the maps to use.
    api_contents_map, files_map, atom_files_for_depset = _get_file_maps(ctx)

    if bool(ctx.attr.api_file_path) != bool(api_contents_map):
        fail("`api_file_path` and `api_contents_map` must be specified together.")

    all_deps_depset = depset(
        direct = atom_files_for_depset +
                 ctx.files.idk_deps +
                 ctx.files.underlying_library +
                 ctx.files.atom_build_deps,
    )
    idk_deps = ctx.attr.idk_deps

    # Though the `files_map` has been modified, there can be no new dependencies
    # so `all_deps_depset` is still correct.
    additional_prebuild_info, files_map = _get_additional_info(ctx, files_map, additional_prebuild_info)

    return [
        DefaultInfo(files = all_deps_depset),
        # LINT.IfChange(idk_atom_info)
        FuchsiaIdkAtomInfo(
            label = ctx.label,
            idk_name = ctx.attr.idk_name,
            id = _replace_placeholders(ctx.attr.id, ctx),
            meta_dest = _replace_placeholders(ctx.attr.meta_dest, ctx),
            type = ctx.attr.type,
            category = ctx.attr.category,
            is_stable = ctx.attr.stable,
            api_area = ctx.attr.api_area,
            api_file_path = ctx.attr.api_file_path,
            api_contents_map = api_contents_map,
            atom_files_map = _replace_placeholders_in_map(files_map, ctx),
            idk_deps = idk_deps,
            atoms_depset = depset(
                direct = idk_deps,
                transitive = [dep[FuchsiaIdkAtomInfo].atoms_depset for dep in idk_deps],
            ),
            atom_build_deps = ctx.attr.atom_build_deps,
            additional_prebuild_info = additional_prebuild_info,
        ),
        # LINT.ThenChange(//build/bazel/bazel_idk/providers.bzl:idk_atom_info)
    ]

_create_idk_atom = rule(
    doc = """Define an IDK atom. Do not instantiate directly - use `idk_atom()` instead.

`name` must end in `_idk`.

If `underlying_library` is specified, information from it will be added to the
provided `files_map` and `additional_prebuild_info`.

Atoms will be checked for category and API area violations when generating the IDK (see `generate_idk`).

The `id`, `meta_dest`, and `atom_files_map` attributes support placeholders.
See the `_replace_placeholders()` function for supported placeholders.
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
            values = ["compat_test", "host_tool", "prebuilt", "partner"],
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
        "api_file_path": attr.label(
            doc = "Path to the file representing the API canonically exposed by this atom. " +
                  "This file is used to ensure modifications to the API are explicitly acknowledged. " +
                  "If this attribute is set, `api_contents_map` must be set as well. If not specified, no such modification checks are performed.",
            mandatory = False,
            allow_single_file = True,
        ),
        "api_contents_map": attr.string_keyed_label_dict(
            doc = "A dictionary of files making up the atom's API, mapping the destination path " +
                  "of  a file relative to the IDK root to its source file label. " +
                  "The set of files will be used to verify that the API has not changed locally. " +
                  "This is very roughly approximated by checking whether the files themselves have changed at all." +
                  "Required and must not be empty when when `api_file_path` is set. " +
                  "Must be specified if `api_file_path` is specified and `configurable_info` is not. " +
                  "Mutually exclusive with `configurable_info`.",
            mandatory = False,
            default = {},
            allow_files = True,
        ),
        "files_map": attr.string_keyed_label_dict(
            doc = "A dictionary of files for this atom, mapping the destination " +
                  "path of a file relative to the IDK root to its source file label . " +
                  "Mutually exclusive with `configurable_info`.",
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
            providers = [PrebuiltLibraryInfo],
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
        "configurable_info": attr.label(
            doc = "Information about the atom that is configurable and thus may contain `select()` statements. " +
                  "Populated fields are used instead of the corresponding attributes except for " +
                  "`additional_prebuild_info`, which is merged.",
            providers = [ConfigurableInfo],
            mandatory = False,
        ),
        "_current_api_level": attr.label(
            default = "@//build/bazel:fuchsia_api_level",
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
        allowlist,
        prebuilt_library_format,
        additional_prebuild_info,
        configurable_info,
        **kwargs):
    if prebuilt_library_format:
        prebuild_info_format = json.decode(additional_prebuild_info["format"])
        if prebuilt_library_format != prebuild_info_format:
            fail("`prebuilt_library_format` `%s` does not match `%s` in `additional_prebuild_info`." %
                 (prebuilt_library_format, prebuild_info_format))
    elif "format" in additional_prebuild_info:
        fail("`additional_prebuild_info` must not contain `format` when `prebuilt_library_format` is not specified.")

    # The allowlist must be passed as a label attribute due to
    # https://fxbug.dev/446911800. Verify that the correct allowlist is passed.
    allowlist_string = "//%s:%s" % (allowlist.package, allowlist.name)
    expected_allowlist = get_allowlist_target(type, category, stable, prebuilt_library_format)
    if allowlist_string != expected_allowlist:
        fail("`allowlist` must be `%s`, but was `%s`." % (expected_allowlist, allowlist_string))

    if type not in _TYPES_SUPPORTING_UNSTABLE_ATOMS and not stable:
        fail("`stable` must be true unless the type ('%s') is one of %s." % (type, _TYPES_SUPPORTING_UNSTABLE_ATOMS))

    if bool(api_contents_map) and bool(configurable_info):
        fail("`api_contents_map` and `configurable_info` must not be both set at the same time.")

    is_type_not_requiring_compatibility = type in _TYPES_NOT_REQUIRING_COMPATIBILITY
    if stable and not api_file_path and not is_type_not_requiring_compatibility:
        fail("All atoms with types ('%s') requiring compatibility must specify an `api_file_path` unless explicitly unstable." % type)

    # Ensure the atom is in the appropriate allowlist.
    # The attribute is immutable, so create a mutable copy.
    atom_build_deps = list(atom_build_deps)
    atom_build_deps.append(allowlist)

    _verify_api = bool(api_file_path)
    if _verify_api:
        if not api_contents_map and not configurable_info:
            fail("`api_contents_map` cannot be empty when `api_file_path` is specified.")

        generate_api_target_name = "%s_generate_api" % name
        verify_api_target_name = "%s_verify_api" % name
        current_api_file = "%s.api" % name

        _compute_atom_api(
            name = generate_api_target_name,
            api_contents_map = api_contents_map,
            configurable_info = configurable_info,
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
            # Required for tests using `create_test_atom_info()`.
            visibility = ["//build/bazel/bazel_idk/tests:__subpackages__"],
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
        additional_prebuild_info = additional_prebuild_info,
        configurable_info = configurable_info,
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
        # Attributes that are also defined for `_create_idk_atom()` must be
        # repeated here to set `configurable = False`, which is not allowed in
        # rule definitions and thus cannot be inherited.
        "type": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "See _create_idk_atom().",
            values = ["compat_test", "host_tool", "prebuilt", "partner"],
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
        "api_file_path": attr.label(
            doc = "See _create_idk_atom().",
            allow_single_file = True,
            configurable = False,
        ),
        "api_contents_map": attr.string_keyed_label_dict(
            doc = "See _create_idk_atom().",
            allow_files = True,
            default = {},
            configurable = False,
        ),
        "additional_prebuild_info": attr.string_dict(
            doc = "A dictionary of type-specific prebuild info for the atom, with values encoded as JSON strings.",
            mandatory = False,
            default = {},
            configurable = False,
        ),
        # Non-inherited attributes.
        "allowlist": attr.label(
            doc = "The allowlist to check for this target configuration. Set by a wrapper macro.",
            mandatory = True,
            configurable = False,
        ),
        "prebuilt_library_format": attr.string(
            doc = "See get_allowlist_target().",
            default = "",
            configurable = False,
        ),
    },
)

def _idk_noop_atom_impl(name, visibility, **kwargs):
    # Unlike other IDK macros, which append "_idk" `name`, `name` must end with
    # "_idk". This is to avoid buildifier `duplicated-name` errors in
    # `BUILD.bazel` files, which would occur because this macro does not wrap
    # the underlying target like the other macros do. This symbolic macro would
    # also need to be wrapped in a legacy macro to avoid "conflicts with an
    # existing macro" build errors.
    if not name.endswith("_idk"):
        fail("`name` must end with '_idk'.")

    _create_idk_atom(
        name = name,
        meta_dest = "",
        type = "none",
        visibility = get_atom_visibility(visibility),
        **kwargs
    )

idk_noop_atom = macro(
    doc = """An empty IDK atom.

Should be used in very specific situations where IDK elements are injected in
IDKs in a way that is not reflected in the build graph. This allows IDK-related
macros to handle such a target as any other IDK target.

`name` must end with '_idk' (unlike other IDK macros).
""",
    implementation = _idk_noop_atom_impl,
    attrs = {
        "idk_name": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
        ),
        "id": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
        ),
        "category": attr.string(
            doc = "See _create_idk_atom().",
            values = ["compat_test", "host_tool", "prebuilt", "partner"],
            mandatory = True,
        ),
        "stable": attr.bool(
            doc = "See _create_idk_atom().",
            mandatory = True,
        ),
        "api_area": attr.string(
            doc = "See _create_idk_atom().",
            mandatory = True,
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)
