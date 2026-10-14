# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia unit test package."""

load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")
load(
    "//build/bazel/rules/testing:fx_test.bzl",
    "fx_test",
)
load("//build/bazel/rules/testing:fx_unittest_component.bzl", "fx_unittest_component")

def _fx_unittest_package_impl(
        name,
        package_name,
        unit_tests,
        resources,
        testonly,
        visibility,
        tags,
        **kwargs):
    if testonly != None and not testonly:
        fail("`fx_unittest_package()` targets are always testonly.")
    tags = tags or []
    package_name = package_name or name

    test_components = []
    for unit_test in unit_tests:
        component_target_name = "{}.{}.component".format(name, unit_test.name)
        component_name = package_name if len(unit_tests) == 1 else unit_test.name
        fx_unittest_component(
            name = component_target_name,
            binary = unit_test,
            component_name = component_name,
            tags = tags + ["manual"],
        )
        test_components.append(":" + component_target_name)

    package_target_name = name + ".package"
    fx_package(
        name = package_target_name,
        package_name = package_name,
        test_components = test_components,
        resources = resources,
        tags = tags + ["manual"],
    )

    fx_test(
        name = name,
        package = ":" + package_target_name,
        testonly = True,
        visibility = visibility,
        tags = tags,
        **kwargs
    )

fx_unittest_package = macro(
    doc = """Defines a Fuchsia package and test target for one or more unit test binaries.

    This macro creates a unit test component with `fx_unittest_component()` for
    each binary in `unit_tests`, packages them with `fx_package()`, and exposes
    the device test with `fx_test()`.

    Example usage:

    ```bazel
    fx_unittest_package(
        name = "foo-test",
        package_name = "foo-test",
        unit_tests = [":foo_lib_test"],
    )
    ```
    """,
    implementation = _fx_unittest_package_impl,
    inherit_attrs = fx_test,
    attrs = {
        "package_name": attr.string(
            doc = """ An optional name of the Fuchsia package.

            Defaults to `name` to match the behavior of `fx_package`.
            """,
            # Otherwise an unset value reaches the impl as an always-truthy
            # select(), and the `name` fallback never applies.
            configurable = False,
        ),
        "resources": attr.label_list(
            doc = "Additional package resource targets to include in the package.",
            configurable = False,
        ),
        "unit_tests": attr.label_list(
            doc = "The unit test binary targets to package and run.",
            mandatory = True,
            allow_empty = False,
            configurable = False,
        ),
        # Set internally by the macro.
        "package": None,
    },
)
