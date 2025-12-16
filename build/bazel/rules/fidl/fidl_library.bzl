# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for declaring a FIDL library"""

load(":fidl_ir.bzl", "fidl_ir")
load("//zircon/tools/zither:zither_library.bzl", "zither_library")

# LINT.IfChange(determine_fidlc_versioned_arg)
def _get_fidlc_versioned_arg(
        library_name,
        versioned,
        category,
        stable,
        testonly):
    """Determines the value of the `versioned` argument to pass to fidlc.

    Also determines whether compatibility tests are required.

    Args:
        library_name: The name of the library.
        versioned: The value of the `versioned` attribute.
        category: The value of the `category` attribute.
        stable: The value of the `stable` attribute.
        testonly: The value of the `testonly` attribute.

    Returns:
        fidlc_versioned_arg: The value of the `versioned` argument to pass to fidlc.
        requires_compatibility_tests: Whether compatibility tests are required.
    """
    # Assume `category` validation is done elsewhere.

    # All libraries in an SDK category require compatibility tests.
    requires_compatibility_tests = category != ""

    # All publishable libraries must have compatibility tests.
    # Only "partner" libraries are publishable.
    is_idk_included_publishable = \
        requires_compatibility_tests and category == "partner"

    is_vendor_library = native.package_name().startswith("//vendor/")
    if is_vendor_library:
        if category and category != "partner":
            fail(library_name + ": In vendor repos, only libraries in the vendor IDK should have `sdk_category` set.")
        if versioned != "unversioned":
            fail(library_name + ": `versioned` must be 'unversioned' for vendor IDK libraries.")

        # Vendor IDK libraries are `is_idk_included_publishable` but not
        # in the "fuchsia" namespace, not intended to be compaitibility
        # tested, and do not appear in allowlists. They specify
        # "unversioned" for clarity.
        if not is_idk_included_publishable:
            fail("Internal logic error")
        requires_compatibility_tests = False

        is_unversioned_vendor_idk = True
    else:
        is_unversioned_vendor_idk = False

    # All stable libraries must be included in an SDK [category] and require
    # compatibility tests, but the inverse is not always true.
    # For unstable libraries with `requires_compatibility_tests=True`, although
    # the build targets are created, the resulting API summery file will be empty.
    if stable and not requires_compatibility_tests:
        fail(library_name + ": Stable libraries must require compatibility tests.")

    if not stable and category and category != "partner":
        fail(
            library_name + ": Libraries in category '%s' must specify `stable=True`." % category,
        )

    # Some IDK prebuilts depend on FIDL libraries that are currently internal
    # and unstable. Treat such libraries as unversioned until each is resolved.
    _libraries_in_unsupported_scenarios = [
        # Do not add to this list without discussing with the FIDL team.
        # It is likely that only instances of the scenarios described in
        # https://fxbug.dev/369892217 should be added.

        # TODO(https://fxbug.dev/364294648): Resolve heapdump instrumentation dependency on library.
        "fuchsia.memory.heapdump.process",
    ]
    _is_library_in_unsupported_scenarios = \
        library_name in _libraries_in_unsupported_scenarios

    # TODO(https://fxbug.dev/364422340): Remove when the internal "zx" library is properly versioned.
    _is_internal_zx_library = native.package_name() == "zircon/vdso" and library_name == "zx"

    # //sdk/banjo/fuchsia.sysmem is the only Banjo library with versioning.
    # Banjo libraries do not have an SDK category and are not marked stable,
    # so it is not caught in an earlier condition.
    # TODO(https://fxbug.dev/306258166): Determine an appropriate state for this
    # library and remove this variable and related exceptions.
    _is_banjo_sysmem = library_name == "fuchsia.sysmem" and not category

    # If `versioned` is not specified, set the default as defined in the
    # `fidl_library()` `versioned` attribute.
    if versioned:
        _platform_override_name = versioned.split(":")[0]
        if not (is_idk_included_publishable or testonly or _is_internal_zx_library):
            fail(
                library_name + ": Non-test library is explicitly versioned but not included in an IDK.",
            )
        if requires_compatibility_tests and _platform_override_name != "fuchsia":
            fail(
                library_name + ": Overriding `versioned` is not allowed for IDK FIDL library, which is a Fuchsia platform API requiring compatibility tests.",
            )

        fidlc_versioned_arg = versioned
    elif testonly and not category:
        fidlc_versioned_arg = "unversioned"
    elif library_name.startswith("fuchsia."):
        # The library is in the "fuchsia" namespace and either not test-only or
        # in an SDK category. Set `versioned` to appropriate default.
        if stable:
            if not category:
                fail(library_name + ": Libraries cannot be stable but not in an SDK category.")

            # Stable "fuchsia.*" library in an SDK category - must compile for all Supported API levels.
            fidlc_versioned_arg = "fuchsia"
        elif requires_compatibility_tests:
            if not category:
                fail(
                    library_name + ": Libraries cannot require compatibility tests unless they are in an SDK category.",
                )

            # Unstable "fuchsia.*" library in an SDK category - can only be used at HEAD.
            fidlc_versioned_arg = "fuchsia:HEAD"
        else:
            if category:
                fail(
                    library_name + ": Libraries with an SDK category should be stable or at least require compatibility tests.",
                )

            # All libraries in the "fuchsia" namespace must be versioned. For unstable
            # and/or internal libraries, that means specifying `@available(added=HEAD)`.
            fidlc_versioned_arg = "fuchsia:HEAD"

            # Temporary exceptions to the above rule. See the TODOs where each
            # variable is declared. Update the comment about "temporary exceptions" in
            # the `fidl_library()` `versioned` attribute when removing the last one.
            if _is_library_in_unsupported_scenarios:
                fidlc_versioned_arg = "unversioned"
            elif _is_banjo_sysmem:
                fidlc_versioned_arg = "fuchsia"
    else:
        if stable or category:
            fail(
                library_name + ": Libraries that are stable and/or have an SDK category must be versioned. This is handled automatically for fuchsia.* libraries but must be displayed for other libraries.",
            )
        fidlc_versioned_arg = "unversioned"

    # The examples in the documentation may not conform to the expectations
    # for illustrative purposes, and it does not make sense to change them.
    _is_documentation_example = library_name == "fuchsia.examples.docs"

    # Verify the results are in one of the expected combinations.
    if (fidlc_versioned_arg == "fuchsia" and stable and
        requires_compatibility_tests and
        (is_idk_included_publishable or
         category == "compat_test" or
         category == "host_tool" or
         category == "prebuilt")):
        # Stable libraries versioned in "fuchsia".
        pass
    elif (fidlc_versioned_arg == "fuchsia" and _is_banjo_sysmem and not stable and
          not requires_compatibility_tests and not is_idk_included_publishable):
        # The Banjo sysmem library is an exception.
        pass
    elif (fidlc_versioned_arg == "fuchsia:HEAD" and not stable and
          requires_compatibility_tests == (category != "")):
        # Unstable libraries versioned in "fuchsia".
        pass
    elif (fidlc_versioned_arg == "unversioned" and
          _is_library_in_unsupported_scenarios):
        # Unversioned libraries in unsupported scenarios.
        pass
    elif (fidlc_versioned_arg == "unversioned" and not stable and
          not requires_compatibility_tests and
          (not category or is_unversioned_vendor_idk)):
        # Unversioned libraries.
        pass
    elif (testonly and not stable and not category and
          not requires_compatibility_tests and
          (fidlc_versioned_arg == "unversioned" or
           _is_documentation_example or
           fidlc_versioned_arg == "test:1")):
        # Test-only libraries are either unversioned or versioned in "test" or
        # documentation examples.
        pass
    elif (_is_internal_zx_library and fidlc_versioned_arg == "fuchsia"):
        # The internal ZX library is an exception.
        pass
    else:
        fail(
            "Library '%s' has an unexpected combination of stability ('%s'), versioned ('%s'), SDK category ('%s'), publishable ('%s'), compatibility testing requirements ('%s'), and `testonly` ('%s')." % (
                library_name,
                stable,
                fidlc_versioned_arg,
                category,
                is_idk_included_publishable,
                requires_compatibility_tests,
                testonly,
            ),
        )

    return fidlc_versioned_arg, requires_compatibility_tests

# LINT.ThenChange(//build/fidl/fidl_library.gni:determine_fidlc_versioned_arg)

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

    if available and not testonly:
        fail("`available` is only allowed for `testonly` libraries.")
    if enable_rust_drivers and not enable_rust:
        fail("`enable_rust_drivers` requires `enable_rust`.")
    if additional_cpp_configs:
        fail("`additional_cpp_configs` is not yet supported. A different mechanism will be needed to support this in Bazel.")

    # TODO(https://fxbug.dev/454397833): Support category markers or similar.

    if not library_name:
        library_name = name

    fidl_gen_dir = "gen/%s" % name
    fidl_ir_json = "%s.fidl.json" % name

    compilation_target_name = "%s_compile" % name

    # TODO(https://fxbug.dev/428285014): Validate versioning-related attributes,
    # determine the need for running compatibility tests, and determine the
    # value for the fidlc `versioned` argument.
    fidlc_versioned_arg, requires_compatibility_tests = _get_fidlc_versioned_arg(
        library_name = library_name,
        versioned = versioned,
        category = category,
        stable = stable,
        testonly = testonly,
    )

    fidl_ir(
        name = compilation_target_name,
        library_name = library_name,
        fidl_library_target_name = name,
        srcs = srcs,
        deps = deps,
        gen_dir = fidl_gen_dir,
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
        out_json_summary = "%s/%s.api_summary.json" % (fidl_gen_dir, library_name),
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
        zither_library(
            name = name + "_zither",
            library_name = library_name,
            srcs = srcs,
            fidl_gen_dir = fidl_gen_dir + "/zither",
            fidl_ir_target = compilation_target_name,
            fidl_ir_json = fidl_ir_json,
            testonly = testonly,
            visibility = visibility,

            # TODO(https://fxbug.dev/454449781): Support overrides for Zither backends.
            # if (defined(invoker.zither)) {
            #   forward_variables_from(invoker.zither, "*")
            # }
        )

    # TODO(https://fxbug.dev/442637596): Implement host test data or similar in the proper conditions.

    atom_type = "fidl_library"

    if category:
        # TODO(https://fxbug.dev/428285014): Create an idk_atom().
        fail("IDK atom creation is not yet supported.")

    native.filegroup(
        name = name,
        srcs = [compilation_target_name],
        # For libraries in a category, add a deps on the allowlist to catch
        # cases where the macro is used but there is no dependency on the atom
        # target.
        # TODO(https://fxbug.dev/428285014): Uncomment when adding IDK atom support.
        # data = [get_allowlist_target(atom_type, category, stable)] if category else [],
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
            values = ["compat_test", "host_tool", "prebuilt", "partner", ""],
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
If not specified, appropriate values will be determined based on the target API level.
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
