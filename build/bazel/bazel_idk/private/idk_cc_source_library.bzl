# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK C/C++ source library."""

load("@bazel_skylib//lib:paths.bzl", "paths")
load("@rules_cc//cc:defs.bzl", "cc_library")
load(
    ":cc_verification.bzl",
    "create_verify_no_duplicate_files_target",
    "create_verify_pragma_once_target",
)
load(":idk_atom.bzl", "idk_atom")
load(
    ":idk_common.bzl",
    "get_allowlist_target",
    "get_atom_visibility",
    "get_idk_deps",
    "json_encode_dict_values",
)

# LINT.IfChange(idk_cc_source_library)

# TODO(https://fxbug.dev/428229472): When migrating "zbi-format":
# * add `non_idk_implementation_deps` (GN equivalent: `non_sdk_deps`) argument.
# * assert that it is only used for "//sdk/fidl/zbi:zbi.c.checked-in".
# * Add TODO for https://fxbug.dev/42062786 to remove the argument when fixing the bug.
def _idk_cc_source_library_impl(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        hdrs_for_internal_use,
        srcs,
        deps,
        implementation_deps,
        fuchsia_deps,
        include_base,
        api_file_path,
        testonly,
        visibility,
        build_as_static,  # buildifier: disable=unused-variable - For GN conversion only.
        friend,  # buildifier: disable=unused-variable - For GN conversion only.
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Implementation for the idk_cc_source_library() macro."""

    atom_type = "cc_source_library"

    if "data" in kwargs:
        fail("Rumtime dependencies are not supported for source libraries.")

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
            relative_destination = paths.join(this_directory_relative_to_sdk, header.name)
        else:
            relative_destination = paths.relativize(header.name, include_base)

        destination = include_dest + "/" + relative_destination
        idk_metadata_headers.append(destination)
        idk_header_files_map |= {destination: header}

    idk_files_map = dict(idk_header_files_map)

    for source in srcs:
        source_dest_path = idk_root_path + "/" + source.name
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
        name = name + "_idk",
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

    # TODO(https://fxbug.dev/446996512): Implement the //sdk:sdk_source_set_list
    # build API module and merge with the GN data.
    # sdk_source_set_sources = all_source_files

idk_cc_source_library = macro(
    doc = """Defines a C++ source library that can be exported to an IDK.

Defines a `cc_library` named `name` and an IDK atom named "{name}_idk".

The IDK atom target ("{name}_idk") does NOT build the underlying library
(`name`). To provide build coverage, ensure some other target in the build
graph depends on target `name`.

The values of all deps args must be iterable. That means they cannot contain
`select()` statements. Instead, use `fuchsia_deps` for public dependencies
that only apply to Fuchsia.""",
    implementation = _idk_cc_source_library_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_library()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_library,
    attrs = {
        "idk_name": attr.string(
            doc = """Name of the library in the IDK. Usually matches `name`.
GN equivalent: `sdk_name`""",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Publication level of the library in the IDK. See _create_idk_atom().",
            mandatory = True,
            configurable = False,
        ),
        "stable": attr.bool(
            doc = """Whether this source library is stabilized.
When true, an .api file is generated. When false, the atom is marked as unstable in the final IDK.""",
            mandatory = True,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = """The API area responsible for maintaining this library.
GN equivalent: `sdk_area`""",
            mandatory = True,
        ),
        "hdrs": attr.label_list(
            doc = """The list of C and C++ header files published by this library to be directly
included by sources in dependent rules. Does not include internal headers that are included from
public headers but not meant to be included by dependents - see `hdrs_for_internal_use`.
Atoms providing headers used by these headers must be included in the (public) `deps`.
GN equivalent: `public`
GN note: Unlike the GN template, this list does not include `hdrs_for_internal_use`.""",
            allow_files = True,
            allow_empty = False,
            mandatory = True,
            configurable = False,
        ),
        "hdrs_for_internal_use": attr.label_list(
            doc = """List of C and C++ headers included by headers in `hdrs` that are not
meant to be included by a client of this library. They usually contain implementation details.
Their contents are not included in documentation but they are included in the `headers` metadata
for the IDK library. They may be excluded from some but not all API compatibility checks.
Like `hdrs`, the atoms providing headers used by these headers must be included in the
(public) `deps`.
GN equivalent: `sdk_headers_for_internal_use`
GN note: Unlike the GN template where this argument specifices headers already included
elsewhere, such headers are only listed here.""",
            allow_files = True,
            default = [],
            configurable = False,
        ),
        "srcs": attr.label_list(
            doc = """The list of C and C++ source and header files that are processed to create the
library target, excluding those in `hdrs` and `hdrs_for_internal_use`.
Header files in this list can only be included by this library.
GN equivalent: `sources`
GN note: Unlike the GN template, public headers must actually be in `hdrs`.""",
            allow_files = True,
            default = [],
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = """List of labels for other IDK elements this element publicly depends on at build time.
These labels must point to targets with corresponding `_create_idk_atom()` targets.
As with all deps arguments, must not contain `select()` statements.
GN equivalent: `public_deps`""",
            default = [],
            configurable = False,
        ),
        "implementation_deps": attr.label_list(
            doc = """List of labels for other IDK elements this element depends on at build time.
These labels must point to targets with corresponding `_create_idk_atom()` targets.
GN equivalent: `deps`.""",
            default = [],
            configurable = False,
        ),
        "fuchsia_deps": attr.label_list(
            doc = """List of labels for other IDK elements this element publicly depends on at
build time only when targeting Fuchsia.
These labels must point to targets with corresponding `_create_idk_atom()` targets.
GN equivalent: `public_deps +=` inside an `if (is_fuchsia) {}` statement.
GN note: If `bazel2gn` is run on the target, `fuchsia_deps` must come after `deps.
This may require adding `# buildifier: leave-alone` above the target definition to
disable reordering by the formatter.""",
            default = [],
            configurable = False,
        ),
        "include_base": attr.string(
            doc = """Path to the root directory for includes.
This path will be added to the underlying library's `includes`.
If the path is "//sdk", the paths to the headers will be made relative to `//sdk`.
therwise, it must be relative to the directory containing the invoking
BUILD.bazel file and `include_base` will be removed from all header paths.
GN note: This preserves the behavior of the GN template.""",
            default = "include",
            configurable = False,
        ),
        "api_file_path": attr.string(
            doc = """Override path for the file representing the API of this library.
This file is used to ensure modifications to the library's API are explicitly acknowledged.
If not specified, the path will be "<idk_name>.api".
Only specify when the default needs to be overridden.
When the path is not in the current directory, the file will likely need to be
made visibile to this target using `exports_files()` in the BUILD.bazel file
for the directory containing the .api file.
GN equivalent: `api`
Not allowed when `stable` is false.""",
            default = "",
            configurable = False,
        ),
        # TODO(https://fxbug.dev/425931839): Remove these when no longer converting to GN.
        # TODO(https://fxbug.dev/421888626): Use this argument if there is a
        # way to tell Bazel to not always compile the source set.
        "build_as_static": attr.bool(
            doc = "Unused in Bazel, for GN conversion only.",
            default = False,
        ),
        "friend": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        # Rumtime dependencies are not supported for source libraries. Do not inherit.
        "data": None,
        # Do not inherit as we will specify this attributed to `cc_library()` in the implementation.
        "includes": None,
    },
)

# LINT.ThenChange(//build/cpp/sdk_source_set.gni)

def _idk_cc_source_library_zx_impl(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        sdk,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Implementation for the idk_cc_source_library_zx() macro."""

    # LINT.IfChange
    if sdk != "source":
        fail('`sdk` must be "source".')
    if "sdk_headers" in kwargs:
        fail('`sdk_headers` is not supported. Headers for the IDK must be specified in `public`. Note that "include/" must be included in the paths in `public`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    idk_cc_source_library(
        name = name,
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_area = api_area,
        hdrs = hdrs,
        **kwargs
    )

idk_cc_source_library_zx = macro(
    doc = "Defines a C++ source library that can be exported to an IDK and will be a zx_library() in GN.",
    inherit_attrs = idk_cc_source_library,
    implementation = _idk_cc_source_library_zx_impl,
    attrs = {
        "idk_name": attr.string(
            doc = "See idk_cc_source_library().",
            mandatory = True,
            configurable = False,
        ),
        "category": attr.string(
            doc = """See idk_cc_source_library().
Converted to `sdk_publishable` in `zx_library()`.""",
            mandatory = True,
            configurable = False,
        ),
        "stable": attr.bool(
            doc = "See idk_cc_source_library().",
            mandatory = True,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = "See idk_cc_source_library().",
            mandatory = True,
        ),
        "hdrs": attr.label_list(
            doc = """ See idk_cc_source_library().
GN equivalent: `sdk_headers`
GN note: Unlike the GN template, the "include/" part of the path must be specified.""",
            allow_files = True,
            mandatory = True,
            configurable = False,
        ),
        "sdk": attr.string(
            doc = 'Must always be "source". Unused in Bazel, for GN conversion only.',
            mandatory = True,
            configurable = False,
        ),
    },
)
