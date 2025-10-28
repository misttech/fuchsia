# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for declaring a FIDL library"""

load(":fidl_ir.bzl", "fidl_ir")

def _fidl_library_impl(
        name,
        srcs,
        library_name,
        category,
        stable,
        api_area,
        deps,
        api_file_path,
        versioned,
        available,
        experimental_flags,
        experimental_checks,
        excluded_checks,
        goldens_dir,
        contains_drivers,
        enable_cpp,
        enable_hlcpp,
        enable_rust,
        enable_rust_next,
        enable_rust_drivers,
        rust_next_emit_compat,
        enable_bindlib,
        enable_banjo,
        enable_zither,
        additional_cpp_configs,
        non_fidl_deps,  # buildifier: disable=unused-variable - For GN conversion only.
        testonly,
        visibility):
    """Implementation of the fidl_library() macro."""

    if enable_rust_drivers and not (enable_rust or enable_rust_next):
        fail("`enable_rust_drivers` requires `enable_rust` or `enable_rust_next`.")
    if rust_next_emit_compat and not enable_rust:
        fail("`rust_next_emit_compat` requires `enable_rust`.")
    if additional_cpp_configs:
        fail("`additional_cpp_configs` is not yet supported. A different mechanism will be needed to support this in Bazel.")

    # TODO(https://fxbug.dev/454397833): Support category markers or similar.

    if not library_name:
        library_name = name

    fidl_ir_json = "%s.fidl.json" % name

    # This should be named `"%s_compile" % name` for consistency with the GN
    # target name. However, Bazel symbolic macro naming limitations for output
    # files used as inputs require that output file names begin with the macro's
    # `name`. Thus, this target's name must be a prefix of `fidl_ir_json`.
    # TODO(https://fxbug.dev/428285014): Consider making `fidl_ir()` a legacy
    # macro, especially if other generated file names become problematic, or
    # renaming the GN target for consistency.
    compilation_target_name = "%s.fidl" % name

    # TODO(https://fxbug.dev/428285014): Validate versioning-related attributes,
    # determine the need for running compatibility tests, and determine the
    # value for the fidlc `versioned` argument.
    fidlc_versioned_arg = versioned

    fidl_ir(
        name = compilation_target_name,
        library_name = library_name,
        fidl_library_target_name = name,
        srcs = srcs,
        deps = deps,
        json_dir = "",
        json_representation = fidl_ir_json,
        available = available,
        versioned = fidlc_versioned_arg,
        experimental_flags = experimental_flags,
        testonly = testonly,
        visibility = ["//visibility:private"],

        # Temporary for testing.
        # Note that the naming restriction does not apply to this file because
        # it is not used as an input to another target within the macro.
        # TODO(https://fxbug.dev/428285014): Remove this once this is being
        # excercised as part of compatibility tests. We may then be able to
        # change compilation_target_name to the desired value.
        out_json_summary = "%s.api_summary.json" % library_name,
    )
    # TODO(https://fxbug.dev/428285014): Validate resulting JSON.

    # TODO(https://fxbug.dev/428285014): Implement linting.

    # TODO(https://fxbug.dev/417306131): Implement PlaSA support.

    # TODO(https://fxbug.dev/428285014): Implement compatibility tests. This
    # may require making `fidl_ir()` a legacy macro due to the naming
    # restrictions described for `compilation_target_name`.

    if enable_cpp:
        # TODO(https://fxbug.dev/454977301): Implement C++ bindings.
        pass

    if enable_hlcpp:
        # TODO(https://fxbug.dev/454977301): Implement HLCPP bindings.
        fail("HLCPP bindings are not yet supported.")

    if enable_rust:
        # TODO(https://fxbug.dev/454452299): Implement Rust bindings.
        pass

    if enable_rust_next:
        # TODO(https://fxbug.dev/454452299): Implement next-generation Rust bindings.
        fail("Next-generation Rust bindings are not yet supported.")

    if enable_bindlib:
        # TODO(https://fxbug.dev/454451664): Implement bindlib bindings.
        pass

    if enable_banjo:
        # TODO(https://fxbug.dev/428285014): Implement Banjo bindings if necessary.
        fail("Banjo bindings are not yet supported.")

    if enable_zither:
        # TODO(https://fxbug.dev/454449781): Implement Zither bindings.
        fail("Zither bindings are not yet supported.")

    # TODO(https://fxbug.dev/442637596): Implement host test data or similar in the proper conditions.

    if category:
        # TODO(https://fxbug.dev/442637596): Create an idk_atom().
        fail("IDK atom creation is not yet supported.")

    native.filegroup(
        name = name,
        srcs = [compilation_target_name],
        testonly = testonly,
        visibility = visibility,
    )

fidl_library = macro(
    doc = """Declares a FIDL library.

Supported backends: Rust, C++, HLCPP, banjo_{c,cpp,rust}, bindlib, and Zither.""",
    implementation = _fidl_library_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = """List of `.fidl` source files.
GN equivalent: `sources`""",
            mandatory = True,
            allow_files = True,
            allow_empty = False,
            configurable = False,
        ),
        "library_name": attr.string(
            doc = """Name of the library. Defaults to `name`.
GN equivalent: `name`""",
            mandatory = False,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Publication level of the library in the IDK. See _create_idk_atom().",
            mandatory = False,
            configurable = False,
        ),
        "stable": attr.bool(
            doc = """Whether this source library is stabilized.
When true, an .api file is generated. When false, the atom is marked as unstable in the final IDK.""",
            mandatory = False,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = """The API area responsible for maintaining this library.
GN equivalent: `sdk_area`""",
            mandatory = False,
        ),
        "deps": attr.label_list(
            doc = """
            List of labels for other FIDL libraries on which this library depends.
As with all deps arguments, must not contain `select()` statements.
GN equivalent: `public_deps`""",
            default = [],
            configurable = False,
        ),
        "api_file_path": attr.string(
            doc = """Override path for the file representing the API of this library.
This file is used to ensure modifications to the library's API are explicitly acknowledged.
If not specified, the path will be "<library_name>.api".
Only specify when the default needs to be overridden.
When the path is not in the current directory, the file will likely need to be
made visibile to this target using `exports_files()` in the BUILD.bazel file
for the directory containing the .api file.
GN equivalent: `api`
Not allowed when `stable` is false.""",
            default = "",
            configurable = False,
        ),
        "versioned": attr.string(
            doc = """A string of the form PLATFORM or PLATFORM:VERSION.
If provided, fidlc will validate that the library is versioned under PLATFORM and added at
VERSION (if provided).
fidlc determines the library's actual platform from FIDL files as follows:
    * If there are no @available attributes, the platform is "unversioned".
    * The platform can be explicit with @available(platform="PLATFORM").
    * Otherwise, the platform is the first component of the library name.
Defaults are:
    * When testonly is true and SDK category is not specified: "unversioned"
    * When the library name starts with "fuchsia.":
    * If `stable` is true: "fuchsia"
    * For unstable libraries in an SDK category: "fuchsia:HEAD"
    * Otherwise: "fuchsia:HEAD" (with a few temporary exceptions)
    * Otherwise: "unversioned"
""",
            configurable = False,
        ),
        "available": attr.string_list(
            doc = """A list of strings of the form PLATFORM:VERSION. This is needed when using
`@available` annotations for platforms other than "fuchsia".
For more information, look for `--available` in `fidlc --help`.
Warning: All dependencies must specify the same value for `available`;
otherwise bindings will be inconsistent. Since this is easy to misuse,
this parameter is only allowed on `testonly` libraries.
""",
            configurable = False,
        ),
        "experimental_flags": attr.string_list(
            doc = "A list of experimental fidlc features to enable.",
            configurable = False,
        ),
        "experimental_checks": attr.string_list(
            doc = "A list of fidl-lint check IDs to include (by passing the command line flag " +
                  "`-x some-check-id` for each value).",
            configurable = False,
        ),
        "excluded_checks": attr.string_list(
            doc = "A list of fidl-lint check IDs to ignore (by passing the command line flag " +
                  "`-e some-check-id` for each value).",
            configurable = False,
        ),
        "goldens_dir": attr.string(
            doc = "The directory containing golden files for this FIDL API, per API level. " +
                  "Should not contain a trailing slash. This is only used if compatibility tests are required.",
            default = "//sdk/history",
            configurable = False,
        ),
        "contains_drivers": attr.bool(
            doc = "Indicates if any of the FIDL files contain the driver transport or " +
                  "references to the driver transport.",
            default = False,
            configurable = False,
        ),
        "enable_cpp": attr.bool(
            doc = "Set to false to disable the new C++ bindings for this library",
            default = True,
            configurable = False,
        ),
        "enable_hlcpp": attr.bool(
            doc = "Set to true to enable legacy HLCPP bindings for this library",
            default = False,
            configurable = False,
        ),
        "enable_rust": attr.bool(
            doc = "Set to false to disable Rust bindings for this library",
            default = True,
            configurable = False,
        ),
        "enable_rust_next": attr.bool(
            doc = "Set to true to enable next-generation Rust bindings for this library",
            default = False,
            configurable = False,
        ),
        "enable_rust_drivers": attr.bool(
            doc = "Set to true to enable experimental rust driver transport support",
            default = False,
            configurable = False,
        ),
        "rust_next_emit_compat": attr.bool(
            doc = "Set to false to disable compatibility with existing Rust bindings for this library",
            default = True,
            configurable = False,
        ),
        "enable_bindlib": attr.bool(
            doc = "Set to false to disable bindlib bindings for this library",
            default = True,
            configurable = False,
        ),
        "enable_banjo": attr.bool(
            doc = "Set to true to enable Banjo bindings for this library.",
            default = False,
            configurable = False,
        ),
        "enable_zither": attr.bool(
            doc = "Set to true to enable Zither bindings for this library.",
            default = False,
            configurable = False,
        ),
        # TODO(https://fxbug.dev/454443348): Add applicable_licenses.
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
        # TODO(https://fxbug.dev/425931839): Remove these when no longer converting to GN.
        "additional_cpp_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
            configurable = False,
        ),
        "non_fidl_deps": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only." +
                  "Bazel should correctly handle dependencies for generated `srcs` files.",
            default = [],
            configurable = False,
        ),
    },
)
