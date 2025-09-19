# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules used to define IDK atoms."""

load("@bazel_skylib//lib:paths.bzl", "paths")
load("@rules_cc//cc:defs.bzl", "cc_library")
load(
    ":cpp_verification.bzl",
    "create_verify_no_duplicate_files_target",
    "create_verify_pragma_once_target",
)
load(
    "//build/bazel/bazel_idk/private:idk_common.bzl",
    "get_allowlist_target",
    "get_atom_visibility",
    "get_idk_deps",
    "json_encode_dict_values",
)
load(
    "//build/bazel/bazel_idk/private:idk_atom.bzl",
    "idk_atom",
    _FuchsiaIdkAtomInfo = "FuchsiaIdkAtomInfo",
)
load(
    "//build/bazel/bazel_idk/private:idk_molecule.bzl",
    _FuchsiaIdkMoleculeInfo = "FuchsiaIdkMoleculeInfo",
    _idk_molecule = "idk_molecule",
)

FuchsiaIdkAtomInfo = _FuchsiaIdkAtomInfo
FuchsiaIdkMoleculeInfo = _FuchsiaIdkMoleculeInfo
idk_molecule = _idk_molecule

def create_idk_atom_for_test(name, testonly, **kwargs):
    """Wrapper to allow creating an atom directly for tests only."""
    if not testonly:
        fail("Atom must be `testonly`.")
    idk_atom(name = name, testonly = testonly, **kwargs)

# LINT.IfChange(idk_cc_source_library)
# TODO(https://fxbug.dev/417305295): Make this a symbolic macro after updating
# to Bazel 8. Replace "Required" comments with `mandatory = True`.
# TODO(https://fxbug.dev/428229472): When migrating "zbi-format":
# * add `non_idk_implementation_deps` (GN equivalent: `non_sdk_deps`) argument.
# * assert that it is only used for "//sdk/fidl/zbi:zbi.c.checked-in".
# * Add TODO for https://fxbug.dev/42062786 to remove the argument when fixing the bug.
def idk_cc_source_library(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        hdrs_for_internal_use = [],
        srcs = [],
        deps = [],
        implementation_deps = [],
        fuchsia_deps = [],
        include_base = "include",
        api_file_path = None,
        testonly = False,
        visibility = ["//visibility:private"],
        # TODO(https://fxbug.dev/425931839): Remove these when no longer converting to GN.
        build_as_static = False,  # buildifier: disable=unused-variable - For GN conversion only.
        friend = [],  # buildifier: disable=unused-variable - For GN conversion only.
        public_configs = [],  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Defines a C++ source library that can be exported to an IDK.

    The IDK atom target ("{name}_idk") does NOT build the underlying library
    (`name`). To provide build coverage, ensure some other target in the build
    graph depends on target `name`.

    The values of all deps args must be iterable. That means they cannot contain
    `select()` statements. Instead, use `fuchsia_deps` for public dependencies
    that only apply to Fuchsia.

    Args:
        name: The name of the underlying `cc_library` target. Required.
            GN equivalent: `target_name`
        idk_name: Name of the library in the IDK. Usually matches `name`. Required.
            GN equivalent: `sdk_name`
        category: Publication level of the library in the IDK. See _create_idk_atom(). Required.
        stable: Whether this source library is stabilized.
            When true, an .api file is generated. When false, the atom is marked
            as unstable in the final IDK. Required.
        api_area: The API area responsible for maintaining this library. Required.
            GN equivalent: `sdk_area`
        hdrs: The list of C and C++ header files published by this library to be directly included
            by sources in dependent rules. Does not include internal headers that are included from
            public headers but not meant to be included by dependents - see `hdrs_for_internal_use`.
            Atoms providing headers used by these headers must be included in the (public) `deps`.
            Required and may not be empty.
            GN equivalent: `public`
            GN note: Unlike the GN template, this list does not include `hdrs_for_internal_use`.
        hdrs_for_internal_use: List of C and C++ headers included by headers in `hdrs` that are not
            meant to be included by a client of this library. They usually contain implementation
            details. Their contents are not included in documentation but they are included in the
            `headers` metadata for the IDK library. They may be excluded from some but not all API
            compatibility checks.
            Like `hdrs`, the atoms providing headers used by these headers must be included in the
            (public) `deps`.
            GN equivalent: `sdk_headers_for_internal_use`
            GN note: Unlike the GN template where this argument specifices headers already included
            elsewhere, such headers are only listed here.
        srcs: The list of C and C++ source and header files that are processed to create the
            library target, excluding those in `hdrs` and hdrs_for_internal_use.
            Header files in this list can only be included by this library.
            GN equivalent: `sources`
            GN note: Unlike the GN template, public headers must actually be in `hdrs`.
        deps: List of labels for other IDK elements this element publicly depends on at build time.
            These labels must point to targets with corresponding `_create_idk_atom` targets.
            As with all deps arguments, must not contain `select()` statements.
            GN equivalent: `public_deps`
        implementation_deps: List of labels for other IDK elements this element depends on at build time.
            These labels must point to targets with corresponding `_create_idk_atom` targets.
            GN equivalent: `deps`.
        fuchsia_deps: List of labels for other IDK elements this element
            publicly depends on at build time only when targeting Fuchsia.
            These labels must point to targets with corresponding `_create_idk_atom` targets.
            GN equivalent: `public_deps +=` inside an `if (is_fuchsia) {}` statement.
            GN note: If `bazel2gn` is run on the target, `fuchsia_deps` must come after `deps.
            This may require adding `# buildifier: leave-alone` above the target definition to
            disable reordering by the formatter.
        include_base: Path to the root directory for includes.
            This path will be added to the underlying library's `includes`.
            If the path is "//sdk", the paths to the headers will be made
            relative to `//sdk`. Otherwise, it must be relative to the directory
            containing the invoking BUILD.bazel file and `include_base` will be
            removed from all header paths.
            GN note: This preserves the behavior of the GN template.
        api_file_path: Override path for the file representing the API of this library.
            This file is used to ensure modifications to the library's API are
            explicitly acknowledged.
            If not specified, the path will be "<idk_name>.api".
            Only specify when the default needs to be overridden.
            When the path is not in the current directory, the file will likely
            need to be made visibile to this target using `exports_files()` in
            the BUILD.bazel file for the directory containing the .api file.
            GN equivalent: `api`
            Not allowed when `stable` is false.
        testonly: Standard definition.
        visibility: Standard definition.
        build_as_static: Unused in Bazel, for GN conversion only.
            TODO(https://fxbug.dev/421888626): Use this argument if there is a
             way to tell Bazel to not always compile the source set.
        friend: Unused in Bazel, for GN conversion only.
        public_configs: Unused in Bazel, for GN conversion only.
        **kwargs: Additional arguments for the underlying library.
    """
    atom_type = "cc_source_library"

    if "data" in kwargs:
        fail("Rumtime dependencies are not supported for source libraries.")

    # TODO(https://fxbug.dev/417305295): Replace this with `allow_empty = False`
    # when converting this macro to a symbolic macro.
    if not hdrs:
        fail("`hdrs` cannot be empty.")

    if category not in ["partner"]:
        # Other categories are only to ensure ABI compatibility and thus not
        # applicable.
        fail("Category '%s' is not supported." % category)

    if api_file_path and not stable:
        fail("Unstable targets do not require/support modification acknowledgement.")

    # Group the source files for various uses.
    # Per https://bazel.build/reference/be/c-cpp#cc_library.hdrs,
    # "Headers not meant to be included by a client of this library should be
    # listed in the srcs attribute instead, even if they are included by a
    # published header." Thus, `hdrs_for_internal_use` is added to `srcs` in the
    # underlying library, not `hdrs`. However, other build systems that use the
    # IDK may not work this way, so include `hdrs_for_internal_use` as headers
    # in the IDK. This is also consistent with prebuilt libraries where the IDK
    # only includes headers.
    all_source_files = hdrs + hdrs_for_internal_use + srcs
    hdrs_for_idk = hdrs + hdrs_for_internal_use
    hdrs_for_bazel_library = hdrs
    srcs_for_bazel_library = srcs + hdrs_for_internal_use

    if include_base == "//sdk":
        # Some libraries in //sdk/lib/<library_name>[/...] rely on that in-tree
        # path to allow in-tree code to include `<lib/library_name/header.h>`
        # and for the IDK destination path rather than providing that structure
        # within the `library_name` directory. Handle this case here.

        # Get the relative path from the build file's directory to `//sdk`,
        # which is the real include base for in-tree builds.
        path_to_this_directory = "//" + native.package_name()
        this_directory_relative_to_sdk = paths.relativize(path_to_this_directory, "//sdk")
        include_path = ""
        for _ in this_directory_relative_to_sdk.split("/"):
            include_path += "../"
    else:
        this_directory_relative_to_sdk = None  # Satisfy buildifier.
        include_path = include_base

    # TODO(https://fxbug.dev/421888626): Apply the equivalent of GN's
    # `default_common_binary_configs` using the `copts` argument.
    # TODO(https://fxbug.dev/421888626): Add "//build/config:sdk_extra_warnings"
    # using the `copts` argument.
    cc_library(
        name = name,
        srcs = srcs_for_bazel_library,
        # Add a deps on the allowlist to catch cases where the macro is used but
        # there is no dependency on the atom target.
        data = [get_allowlist_target(atom_type, category, stable)],
        hdrs = hdrs_for_bazel_library,
        deps = deps + select({
            "@platforms//os:fuchsia": fuchsia_deps,
            "//conditions:default": [],
        }),
        # TODO(https://fxbug.dev/428229472): If we must support
        # `non_idk_implementation_deps`, include it below.
        implementation_deps = implementation_deps,
        includes = [include_path],
        testonly = testonly,
        visibility = visibility,
        **kwargs
    )

    idk_root_path = "pkg/" + idk_name
    include_dest = idk_root_path + "/include"

    # Determine destinations in the IDK for headers and sources.
    idk_metadata_headers = []
    idk_metadata_sources = []
    idk_header_files_map = {}

    for header in hdrs_for_idk:
        if include_base == "//sdk":
            # As above, handle the special case.
            relative_destination = paths.join(this_directory_relative_to_sdk, header)
        else:
            relative_destination = paths.relativize(header, include_base)

        destination = include_dest + "/" + relative_destination
        idk_metadata_headers.append(destination)
        idk_header_files_map |= {destination: header}

    idk_files_map = dict(idk_header_files_map)

    for source in srcs:
        source_dest_path = idk_root_path + "/" + source
        idk_metadata_sources.append(source_dest_path)
        idk_files_map |= {source_dest_path: source}

    # Deps strings must be modified before being added to a `select()` statement.
    idk_deps = get_idk_deps(deps) + get_idk_deps(implementation_deps) + select({
        "@platforms//os:fuchsia": get_idk_deps(fuchsia_deps),
        "//conditions:default": [],
    })

    # Dependencies for generating the actual IDK atom (not the underlying library).
    # TODO(https://fxbug.dev/428229472): If we must support
    # `non_idk_implementation_deps`, add it here.
    atom_build_deps = [
        # All this target really does is provide a clearer error message than if
        # we relied on combining the lists in the `verify_no_pragma_once()` rule
        # below.
        create_verify_no_duplicate_files_target(
            name = name,
            hdrs = hdrs,
            hdrs_for_internal_use = hdrs_for_internal_use,
            srcs = srcs,
            testonly = testonly,
        ),

        # For simplicity, check all source files, including non-header files in
        # `srcs`.
        create_verify_pragma_once_target(
            name = name,
            files = all_source_files,
            testonly = testonly,
        ),
    ]

    if stable:
        api_path = idk_name + ".api"
        if api_file_path:
            # Check that `api_file_path` does not specify the default path.
            # We must assume that absolute paths are not specifying the default
            # path because `relativize()` fails with absolute paths and we
            # cannot get the package path at this point.
            if not paths.is_absolute(api_file_path):
                if paths.relativize(api_file_path, ".") == paths.relativize(api_path, "."):
                    fail("The specified `api` file (`%s`) matches the default. `api` only needs to be specified when overriding the default." % api_file_path)
            api_path = api_file_path

        api_contents_map = idk_header_files_map
    else:
        api_path = None
        api_contents_map = None

    # If changing this, also change
    # //build/sdk/idk_prebuild_manifest.gni:cc_source_library.
    additional_prebuild_info_values = {
        "include_dir": include_dest,
        "sources": idk_metadata_sources,
        "headers": idk_metadata_headers,
        "file_base": idk_root_path,
    }

    idk_atom(
        name = name,
        idk_name = idk_name,
        id = "sdk://" + idk_root_path,
        meta_dest = idk_root_path + "/meta.json",
        type = atom_type,
        category = category,
        stable = stable,
        api_area = api_area,
        api_file_path = api_path,
        api_contents_map = api_contents_map,
        files_map = idk_files_map,
        idk_deps = idk_deps,
        atom_build_deps = atom_build_deps,
        additional_prebuild_info = json_encode_dict_values(additional_prebuild_info_values),
        testonly = testonly,
        visibility = get_atom_visibility(visibility),
    )

    # TODO(https://fxbug.dev/417305295):Implement the //sdk:sdk_source_set_list
    # build API module and merge with the GN data.
    # sdk_source_set_sources = all_source_files

# LINT.ThenChange(//build/cpp/sdk_source_set.gni)

def idk_cc_source_library_zx(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        sdk,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Defines a C++ source library that can be exported to an IDK and will be a zx_library() in GN.

    Args:
        name: See idk_cc_source_library().
        idk_name: See idk_cc_source_library().
        category: See idk_cc_source_library().
            Converted to `sdk_publishable` in `zx_library()`.
        stable: See idk_cc_source_library().
        api_area: See idk_cc_source_library().
        hdrs:  See idk_cc_source_library().
            GN equivalent: `sdk_headers`
            GN note: Unlike the GN template, the "include/" part of the path
            must be specified.
        sdk: Must always be "source". Unused in Bazel, for GN conversion only.
        **kwargs: Additional arguments for idk_cc_source_library().
    """

    # LINT.IfChange
    if sdk != "source":
        fail('`sdk` must be "source".')
    if "sdk_headers" in kwargs:
        fail('`sdk_headers` is not supported. Headers for the IDK must be specified in `public`. Note that "include/" must be included in the paths in `public`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    idk_cc_source_library(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        **kwargs
    )
