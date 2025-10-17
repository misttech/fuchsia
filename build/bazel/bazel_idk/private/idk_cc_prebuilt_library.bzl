# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK C/C++ prebuilt library."""

load("@bazel_skylib//lib:paths.bzl", "paths")
load("@rules_cc//cc:defs.bzl", "cc_library", "cc_shared_library")
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

visibility(["//build/bazel/bazel_idk/..."])

# LINT.IfChange(idk_cc_prebuilt_library)
def _idk_cc_prebuilt_library_impl(
        name,
        prebuilt_library_type,
        idk_name,
        category,
        api_area,
        hdrs,
        hdrs_for_internal_use,
        srcs,
        deps,
        implementation_deps,
        fuchsia_deps,
        runtime_deps,
        include_base,
        api_file_path,
        output_name,
        no_headers,
        libcxx_linkage,
        testonly,
        visibility,
        friend,  # buildifier: disable=unused-variable - For GN conversion only.
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Implementation for the idk_cc_prebuilt_library() macro."""

    atom_type = "cc_prebuilt_library"

    if "data" in kwargs:
        fail("Use `runtime_deps` instead of `data` for atoms that are runtime dependencies.")

    # TODO(https://fxbug.dev/428229472): When migrating "vfs_internal", it may
    # be necessary to add exceptions to these two checks for empty `hdrs`.
    if not hdrs and not no_headers:
        fail("`hdrs` cannot be empty unless `no_headers` is True.")

    if hdrs_for_internal_use and not hdrs:
        fail("`hdrs_for_internal_use` must be empty when `hdrs` is empty.")

    if no_headers and prebuilt_library_type != "shared":
        fail("`no_headers` is only supported for 'shared' libraries.")

    if no_headers and (hdrs != [] or deps != []):
        fail("There must be no public headers or dependencies when `no_headers` is True.")

    if runtime_deps:
        if implementation_deps == []:
            fail("Runtime dependencies are only applicable if there are private dependencies.")

        # TODO(https://fxbug.dev/447151364): Fail if any are not shared libraries.
        # That may need to be done elsewhere.

        # TODO(https://fxbug.dev/447151364): Implement support for runtime dependencies.
        # This includes the "subtle" logic mentioned below and using an aspect
        # to collect runtime dependencies for prebuild info.
        fail("`runtime_deps` is not yet supported.")

    if category not in ["partner"]:
        # Other categories are only to ensure ABI compatibility and thus not
        # applicable.
        fail("Category '%s' is not supported." % category)

    if api_file_path and no_headers:
        fail("Targets without public headers do not require/support modification acknowledgement.")

    if prebuilt_library_type == "static":
        # The output name is always the name of the library target, `name`.

        if output_name != "":
            fail("`output_name` is not supported for static libraries.")

        # `name`, which will be the output name, should not start with `lib` for
        # consistency and simplicity.
        if name.startswith("lib"):
            fail("'lib' will automatically be added to the library file name.")
    else:
        if output_name == "":
            output_name = name
        elif output_name == name:
            fail("The specified `output_name` (`%s`) matches the default. `output_name` only needs to be specified when overriding the default." % output_name)

        # `output_name` should not start with `lib` for consistency and simplicity.
        if output_name.startswith("lib"):
            fail("'lib' will automatically be added to the library file name.")

    if prebuilt_library_type == "static":
        # TODO(https://fxbug.dev/447151364): Implement the "subtle" logic for
        # runtime dependencies from the GN template.
        pass

    # Group the source files for various uses.
    # Per https://bazel.build/reference/be/c-cpp#cc_library.hdrs,
    # "Headers not meant to be included by a client of this library should be
    # listed in the srcs attribute instead, even if they are included by a
    # published header." Thus, `hdrs_for_internal_use` is added to `srcs` in the
    # underlying library, not `hdrs`. However, all headers must be in the IDK.
    all_source_files = hdrs + hdrs_for_internal_use + srcs
    hdrs_for_idk = hdrs + hdrs_for_internal_use
    hdrs_for_bazel_library = hdrs
    srcs_for_bazel_library = srcs + hdrs_for_internal_use

    # Prebuilt shared libraries are eligible for inclusion in the SDK. We do not
    # want to dynamically link against libc++.so because we let clients bring
    # their own toolchain, which might have a different C++ Standard Library or
    # a different C++ ABI entirely.
    if libcxx_linkage == "none":
        # Adding this linker flag keeps us honest about not committing to a
        # specific C++ ABI. If this flag is causing your library to not
        # compile, consider whether your library really ought to be in the SDK.
        # If so, consider including your library in the SDK as source rather than
        # precompiled. If you do require precompilation, you probably need to
        # find a way not to depend on dynamically linking C++ symbols because C++
        # does not have a sufficiently stable ABI for the purposes of our SDK.
        # TODO(https://fxbug.dev/421888626): Apply the equivalent of GN's
        # `//build/config/fuchsia:no_cpp_standard_library`.
        pass
    elif libcxx_linkage == "static":
        # TODO(https://fxbug.dev/421888626): Apply the equivalent of GN's
        # `//build/config/fuchsia:static_cpp_standard_library`.
        pass
    else:
        fail("`libcxx_linkage` ('%s') must be 'none' or 'static'." % libcxx_linkage)

    cc_library_name = "%s_impl" % name

    # TODO(https://fxbug.dev/450004374): Remove  once `cc_static_library()`
    # is no longer an experimental rule and is used below.
    if prebuilt_library_type == "static":
        cc_library_name = name

    # TODO(https://fxbug.dev/421888626): Apply the equivalent of GN's
    # `default_common_binary_configs` for "static" and
    # `default_shared_library_configs` for "shared" using the `copts` attribute.
    # TODO(https://fxbug.dev/421888626): Add "//build/config:sdk_extra_warnings"
    # using the `copts` attribute here too? (It isn't in GN.)
    # TODO(https://fxbug.dev/421888626): Ensure the library is built with the
    # shared library toolchain (without variants for the "shared" case). For
    # the "static" case, this will allow the static library shipped in the IDK
    # to be linked into shared libraries See https://fxbug.dev/404169865.
    cc_library(
        name = cc_library_name,
        srcs = srcs_for_bazel_library,
        # Add a deps on the allowlist to catch cases where the macro is used but
        # there is no dependency on the atom target.
        data = [get_allowlist_target(atom_type, category, stable = True, prebuilt_library_format = prebuilt_library_type)],
        hdrs = hdrs_for_bazel_library,
        deps = deps + select({
            "@platforms//os:fuchsia": fuchsia_deps,
            "//conditions:default": [],
        }),
        implementation_deps = implementation_deps,
        includes = [include_base],
        testonly = testonly,
        visibility = visibility,
        **kwargs
    )

    if prebuilt_library_type == "shared":
        cc_shared_library(
            name = name,
            shared_lib_name = "lib%s.so" % output_name,
            deps = [":%s" % cc_library_name],
            testonly = testonly,
            visibility = visibility,
        )
    elif prebuilt_library_type == "static":
        # TODO(https://fxbug.dev/450004374): Uncomment once
        # `cc_static_library()` is no longer an experimental rule.
        # native.cc_static_library(
        #     name = name,
        #     deps = [":%s" % cc_library_name],
        #     testonly = testonly,
        #     visibility = visibility,
        # )
        pass
    else:
        fail("Unrecognized `prebuilt_library_type` '%s'." % prebuilt_library_type)

    idk_root_path = "pkg/" + idk_name
    include_dest = idk_root_path + "/include"

    # Determine destinations in the IDK for headers and sources.
    idk_metadata_headers = []
    idk_header_files_map = {}

    # Note: Unlike `idk_cc_source_library()`, only relative `include_base`
    # values are allowed.
    for header in hdrs_for_idk:
        relative_destination = paths.relativize(header.name, include_base)
        destination = include_dest + "/" + relative_destination
        idk_metadata_headers.append(destination)
        idk_header_files_map |= {destination: header}

    # The binary files are added to `idk_files_map` by `_create_idk_atom()`.
    idk_files_map = dict(idk_header_files_map)

    # Deps strings must be modified before being added to a `select()` statement.
    idk_deps = get_idk_deps(deps) + get_idk_deps(runtime_deps) + select({
        "@platforms//os:fuchsia": get_idk_deps(fuchsia_deps),
        "//conditions:default": [],
    })

    # Dependencies for generating the actual IDK atom (not the underlying library).
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

    # IFS files do not apply to static libraries.
    verify_public_symbols = prebuilt_library_type != "static"

    if verify_public_symbols:
        atom_build_deps += [
            # TODO(https://fxbug.dev/449812165): Implement this once IFS files are generated.
            # The rule will need to do nothing when
            # `ctx.attr._current_api_level[BuildSettingInfo].value == HEAD`
            # because we do not maintain golden IFS files for "HEAD".
            #
            # create_verify_public_symbols_target()
        ]

    if not no_headers:
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
    # //build/sdk/idk_prebuild_manifest.gni:cc_prebuilt_library.
    additional_prebuild_info_values = {
        "format": prebuilt_library_type,
        "include_dir": include_dest,
        "headers": idk_metadata_headers,
        "file_base": idk_root_path,
        # "binaries" is added by `_create_idk_atom()`.
    }

    if no_headers and (api_path or api_contents_map or idk_header_files_map):
        fail("Internal error: Unexpected populated variables when `no_headers` is True.")

    idk_atom(
        name = name + "_idk",
        idk_name = idk_name,
        id = "sdk://" + idk_root_path,
        meta_dest = idk_root_path + "/meta.json",
        type = atom_type,
        category = category,
        stable = True,
        api_area = api_area,
        api_file_path = api_path,
        api_contents_map = api_contents_map,
        files_map = idk_files_map,
        idk_deps = idk_deps,
        underlying_library = ":%s" % name,
        atom_build_deps = atom_build_deps,
        additional_prebuild_info = json_encode_dict_values(additional_prebuild_info_values),
        prebuilt_library_format = prebuilt_library_type,
        testonly = testonly,
        visibility = get_atom_visibility(visibility),
    )

idk_cc_prebuilt_library = macro(
    doc = """Defines a C++ prebuilt library that can be exported to an IDK.

Defines a prebuilt library of `prebuilt_library_type` named `name` and an IDK
atom named "{name}_idk".

The values of all deps args must be iterable. That means they cannot contain
`select()` statements. Instead, use `fuchsia_deps` for public dependencies
that only apply to Fuchsia.

for static libraries, `name` must not begin with "lib".""",
    implementation = _idk_cc_prebuilt_library_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_library()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_library,
    attrs = {
        "prebuilt_library_type": attr.string(
            doc = """The type of the library - either "shared" or "static".""",
            mandatory = True,
            configurable = False,
        ),
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
May only be empty if `no_headers` is True.
GN equivalent: `public`
GN note: Unlike the GN template, this list does not include `hdrs_for_internal_use`.""",
            allow_files = True,
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
Must be an empty list if `hdrs` is empty.
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
            mandatory = True,
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
        "runtime_deps": attr.label_list(
            doc = """List of labels representing the library's runtime dependencies.
This is only only needed for runtime dependencies inherited from private
dependencies (`implementation_deps` or indirect dependencies not through an IDK
atom's public `deps`). Only labels for shared objects with a corresponding IDK
atom should be specified. That is, labels of instances of
`idk_cc_shared_library()` without the `_idk` suffix.
When not empty, `implementation_deps` must not be empty.
TODO(https://fxbug.dev/447151364): Migrate the note on runtime dependencies from GN template and reference it here.
GN equivalent: `runtime_deps`
GN note: Unlike the GN template, dependencies are not the atom targets and thus
do not end in `_idk`.""",
            default = [],
            configurable = False,
        ),
        "include_base": attr.string(
            doc = """Path to the root directory for includes.
This path will be added to the underlying library's `includes`.
Must be relative to the directory containing the invoking BUILD.bazel file.
`include_base` will be removed from all header paths.""",
            default = "include",
            configurable = False,
        ),
        "api_file_path": attr.string(
            doc = """Override path for the file representing the API of this library.
This file is used to ensure modifications to the library's API are explicitly acknowledged.
Not allowed when `no_headers` is True.
If not specified, the path will be "<idk_name>.api".
Only specify when the default needs to be overridden.
When the path is not in the current directory, the file will likely need to be
made visibile to this target using `exports_files()` in the BUILD.bazel file
for the directory containing the .api file.
GN equivalent: `api`""",
            configurable = False,
        ),
        "output_name": attr.string(
            doc = """Name of the library to generate. Defaults to `name`.
Will be appended to "lib" to generate the library file name.
Must not begin with "lib". Not supported for static libraries.""",
            configurable = False,
        ),
        "no_headers": attr.bool(
            doc = """Specifies that the library's headers are NOT included in the IDK.
Must be False unless `prebuilt_library_type` is "shared".
When True, the API modification acknowledgement mechanism is disabled. (Only the
IFS file mechanism will be used.)
When true, `hdrs` and `deps` must be an empty lists.""",
            default = False,
            configurable = False,
        ),
        "libcxx_linkage": attr.string(
            doc = """Whether or how to link libc++.
SDK shared libraries cannot link libc++.so dynamically because libc++.so does
not have a stable ABI. Can be either "none" or "static".""",
            default = "none",
            configurable = False,
        ),
        # TODO(https://fxbug.dev/425931839): Remove these when no longer converting to GN.
        "friend": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        # Require use of `runtime_deps` for atoms that are runtime dependencies. Do not inherit.
        "data": None,
        # Do not inherit as this attribute is specified to `cc_library()` in the implementation.
        "includes": None,
    },
)

# LINT.ThenChange(//build/cpp/sdk_prebuilt_library_impl.gni:sdk_prebuilt_library_impl)

def _idk_cc_shared_library_impl(name, **kwargs):
    idk_cc_prebuilt_library(name = name, prebuilt_library_type = "shared", **kwargs)

idk_cc_shared_library = macro(
    doc = """Defines a C++ prebuilt shared library that can be exported to an IDK.""",
    inherit_attrs = idk_cc_prebuilt_library,
    attrs = {
        # Do not inherit as this attribute is specified in the implementation.
        "prebuilt_library_type": None,
    },
    implementation = _idk_cc_shared_library_impl,
)

def _idk_cc_static_library_impl(name, **kwargs):
    idk_cc_prebuilt_library(name = name, prebuilt_library_type = "static", **kwargs)

idk_cc_static_library = macro(
    doc = """Defines a C++ prebuilt static library that can be exported to an IDK.""",
    inherit_attrs = idk_cc_prebuilt_library,
    attrs = {
        # Do not inherit as this attribute is specified in the implementation.
        "prebuilt_library_type": None,
        # Disallow an empty list, unlike the underlying macro.
        "hdrs": attr.label_list(
            doc = """The list of C and C++ header files published by this library to be directly
included by sources in dependent rules. Does not include internal headers that are included from
public headers but not meant to be included by dependents - see `hdrs_for_internal_use`.
Atoms providing headers used by these headers must be included in the (public) `deps`.
May only be empty if `no_headers` is True.
GN equivalent: `public`
GN note: Unlike the GN template, this list does not include `hdrs_for_internal_use`.""",
            allow_files = True,
            allow_empty = False,
            mandatory = True,
            configurable = False,
        ),
        # Do not inherit as this attribute is not supported.
        "output_name": None,
    },
    implementation = _idk_cc_static_library_impl,
)

def _idk_cc_shared_library_zx_impl(
        name,
        category,
        hdrs,
        **kwargs):
    """Implementation for the idk_cc_shared_library_zx() macro."""

    # LINT.IfChange
    if "sdk_headers" in kwargs:
        fail('`sdk_headers` is not supported. Headers for the IDK must be specified in `public`. Note that "include/" must be included in the paths in `public`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    idk_cc_shared_library(
        name = name,
        category = category,
        hdrs = hdrs,
        **kwargs
    )

idk_cc_shared_library_zx = macro(
    doc = "Defines a C++ shared library that can be exported to an IDK and will be a zx_library() in GN.",
    inherit_attrs = idk_cc_shared_library,
    implementation = _idk_cc_shared_library_zx_impl,
    attrs = {
        "category": attr.string(
            doc = """See idk_cc_shared_library().
GN equivalent: `sdk_publishable`""",
            mandatory = True,
            configurable = False,
        ),
        "hdrs": attr.label_list(
            doc = """See idk_cc_shared_library().
GN equivalent: `sdk_headers`
GN note: Unlike the GN template, the "include/" part of the path must be specified.""",
            allow_files = True,
            mandatory = True,
            configurable = False,
        ),
    },
)

def _idk_cc_static_library_zx_impl(
        name,
        idk_name,
        category,
        stable,
        api_area,
        hdrs,
        **kwargs):
    """Implementation for the idk_cc_static_library_zx() macro."""

    # LINT.IfChange
    if "sdk_headers" in kwargs:
        fail('`sdk_headers` is not supported. Headers for the IDK must be specified in `public`. Note that "include/" must be included in the paths in `public`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    idk_cc_static_library(
        name = name,
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_area = api_area,
        hdrs = hdrs,
        **kwargs
    )

idk_cc_static_library_zx = macro(
    doc = "Defines a C++ static library that can be exported to an IDK and will be a zx_library() in GN.",
    inherit_attrs = idk_cc_static_library,
    implementation = _idk_cc_static_library_zx_impl,
    attrs = {
        "category": attr.string(
            doc = """See idk_cc_static_library().
GN equivalent: `sdk_publishable`""",
            mandatory = True,
            configurable = False,
        ),
        "hdrs": attr.label_list(
            doc = """See idk_cc_static_library().
GN equivalent: `sdk_headers`
GN note: Unlike the GN template, the "include/" part of the path must be specified.""",
            allow_files = True,
            mandatory = True,
            configurable = False,
        ),
    },
)
