# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""fuchsia_package() rule."""

load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "FUCHSIA_DEBUG_SYMBOLS_ATTRS",
    "find_and_process_unstripped_binaries",
    "merge_debug_symbol_infos",
)
load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaDebugSymbolInfo",
)
load(
    "@fuchsia_rules_common//packages:package.bzl",
    "COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES",
    "common_build_fuchsia_package_impl",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "fuchsia_find_all_package_resources",
)
load("//fuchsia/constraints:target_compatibility.bzl", "COMPATIBILITY")
load("//fuchsia/private/workflows:fuchsia_package_tasks.bzl", "fuchsia_package_tasks")
load(":fuchsia_api_level.bzl", "FUCHSIA_API_LEVEL_ATTRS", "get_fuchsia_api_level")
load(":fuchsia_toolchains.bzl", "FUCHSIA_TOOLCHAIN_DEFINITION", "get_fuchsia_sdk_toolchain")
load(":fuchsia_transition.bzl", "fuchsia_transition")
load(
    ":providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load(
    ":utils.bzl",
    "append_suffix_to_label",
)

def fuchsia_package(
        *,
        name,
        package_name = None,
        archive_name = None,
        platform = None,
        fuchsia_api_level = None,
        components = [],
        resources = [],
        tools = [],
        subpackages = [],
        subpackages_to_flatten = [],
        tags = [],
        **kwargs):
    """Builds a fuchsia package.

    This rule produces a fuchsia package which can be published to a package
    server and loaded on a device.

    The rule will return both package manifest json file which can be used later
    in the build system and an archive (.far) of the package which can be shared.

    This macro will expand out into several fuchsia tasks that can be run by a
    bazel invocation. Given a package definition, the following targets will be
    created.

    ```
    fuchsia_package(
        name = "pkg",
        components = [":my_component"],
        tools = [":my_tool"]
    )
    ```
    - pkg.help: Calling run on this target will show the valid macro-expanded targets
    - pkg.publish: Calling run on this target will publish the package
    - pkg.my_component: Calling run on this target will call `ffx component run`
        with the  component url if it is fuchsia_component instance and will
        call `ffx driver register` if it is a fuchsia_driver_component.
    - pkg.my_tool: Calling run on this target will call `ffx driver run-tool` if
        the tool is a fuchsia_driver_tool

    Args:
        name: The target name.
        components: A list of components to add to this package. The dependencies
          of these targets will have their debug symbols stripped and added to
          the build-id directory.
        resources: A list of additional resources to add to this package. These
          resources will not have debug symbols stripped.
        tools: Additional tools that should be added to this package.
        subpackages: Additional subpackages that should be added to this package.
        subpackages_to_flatten: The list of subpackages included in this package.
          The packages included in this list will be cracked open and all the
          components included will be include in the parent package.
        package_name: An optional name to use for this package, defaults to name.
        archive_name: An option name for the far file.
        fuchsia_api_level: The API level to build for.
        platform: Optionally override the platform to build the package for.
        tags: Forward additional tags to all generated targets.
        **kwargs: extra attributes to pass along to the build rule.
    """

    # This is only used when we want to disable a pre-existing driver so we can
    # register another driver.
    disable_repository_name = kwargs.pop("disable_repository_name", None)

    package_repository_name = kwargs.pop("package_repository_name", None)

    _deps_to_search = components + resources + tools

    processed_binaries = "%s_fuchsia_package.elf_binaries" % name
    find_and_process_unstripped_binaries(
        name = processed_binaries,
        deps = _deps_to_search,
        tags = tags + ["manual"],
        **kwargs
    )

    collected_resources = "%s_fuchsia_package.resources" % name
    fuchsia_find_all_package_resources(
        name = collected_resources,
        deps = _deps_to_search,
        tags = tags + ["manual"],
        **kwargs
    )

    _build_fuchsia_package(
        name = "%s_fuchsia_package" % name,
        components = components,
        resources = resources,
        processed_binaries = processed_binaries,
        collected_resources = collected_resources,
        tools = tools,
        subpackages = subpackages,
        subpackages_to_flatten = subpackages_to_flatten,
        package_name = package_name or name,
        archive_name = archive_name,
        fuchsia_api_level = fuchsia_api_level,
        platform = platform,
        package_repository_name = package_repository_name,
        tags = tags + ["manual"],
        **kwargs
    )

    fuchsia_package_tasks(
        name = name,
        package = "%s_fuchsia_package" % name,
        component_run_tags = [Label(c).name for c in components],
        tools = {tool: tool for tool in tools},
        package_repository_name = package_repository_name,
        disable_repository_name = disable_repository_name,
        # TODO(b/339099331) fuchsia_packages that are testonly shouldn't have the
        # full set of tasks.
        is_test = kwargs.get("testonly", False),
        tags = tags,
        **kwargs
    )

def _fuchsia_test_package(
        *,
        name,
        package_name = None,
        archive_name = None,
        resources = [],
        fuchsia_api_level = None,
        platform = None,
        _test_component_mapping,
        _components = [],
        subpackages = [],
        subpackages_to_flatten = [],
        test_realm = None,
        tags = [],
        target_compatible_with = [],
        **kwargs):
    """Defines test variants of fuchsia_package.

    See fuchsia_package for argument descriptions."""

    _deps_to_search = _components + resources + _test_component_mapping.values()

    processed_binaries = "%s_fuchsia_package.elf_binaries" % name
    find_and_process_unstripped_binaries(
        name = processed_binaries,
        deps = _deps_to_search,
        testonly = True,
        tags = tags + ["manual"],
        target_compatible_with = target_compatible_with,
    )

    collected_resources = "%s_fuchsia_package.resources" % name
    fuchsia_find_all_package_resources(
        name = collected_resources,
        deps = _deps_to_search,
        testonly = True,
        tags = tags + ["manual"],
        target_compatible_with = target_compatible_with,
    )

    _build_fuchsia_package(
        name = "%s_fuchsia_package" % name,
        test_components = _test_component_mapping.values(),
        components = _components,
        resources = resources,
        processed_binaries = processed_binaries,
        collected_resources = collected_resources,
        subpackages = subpackages,
        subpackages_to_flatten = subpackages_to_flatten,
        package_name = package_name or name,
        archive_name = archive_name,
        fuchsia_api_level = fuchsia_api_level,
        platform = platform,
        target_compatible_with = target_compatible_with,
        tags = tags + ["manual"],
        testonly = True,
        **kwargs
    )

    fuchsia_package_tasks(
        name = name,
        package = "%s_fuchsia_package" % name,
        component_run_tags = _test_component_mapping.keys(),
        is_test = True,
        test_realm = test_realm,
        tags = tags,
        target_compatible_with = target_compatible_with,
        **kwargs
    )

def fuchsia_test_package(
        *,
        name,
        test_components = [],
        components = [],
        subpackages_to_flatten = [],
        fuchsia_api_level = None,
        platform = None,
        **kwargs):
    """A test variant of fuchsia_test_package

    See _fuchsia_test_package for additional arguments.


"""
    _fuchsia_test_package(
        name = name,
        _test_component_mapping = {Label(component).name: component for component in test_components},
        _components = components,
        fuchsia_api_level = fuchsia_api_level,
        platform = platform,
        subpackages_to_flatten = subpackages_to_flatten,
        **kwargs
    )

def fuchsia_unittest_package(
        *,
        name,
        unit_tests,
        **kwargs):
    """A wrapper around fuchsia_test_package which doesn't require components.

    This rule allows users to construct a fuchsia_test_package without having to
    create components for each test. This allows users to take a dependency on a
    rule like fuchsia_cc_test directly.

    It is up to the author of the rule being depended on to craft the rule in a
    way that it exposes the generated test component to this rule. The convention
    is that a unit_test target must also create a fuchsia_test_component which
    has the name <name>.unittest_component.

    See fuchsia_test_package for additional arguments.

    Args:
        name: This target name.
        unit_tests: The unit_test targets. These targets must have a generated
          fuchsia_test_component with the name <name>.unittest_component.
        **kwargs: Arguments to forward to the fuchsia_test_package.
    """

    fuchsia_test_package(
        name = name,
        test_components = [append_suffix_to_label(t, "unittest_component") for t in unit_tests],
        **kwargs
    )

def _build_fuchsia_package_impl(ctx):
    sdk = get_fuchsia_sdk_toolchain(ctx)

    fuchsia_debug_symbol_info = merge_debug_symbol_infos(
        ctx.attr.subpackages,
        ctx.attr.test_components,
        ctx.attr.components,
        ctx.attr.resources,
        ctx.attr.processed_binaries,
        ctx.attr.tools,
        ctx.attr._fuchsia_sdk_debug_symbols,
    )

    return common_build_fuchsia_package_impl(
        ctx = ctx,
        ffx_package = sdk.ffx_package,
        ffx_package_is_ffx = True,
        cmc_tool = sdk.cmc,
        meta_content_append_tool = ctx.executable._meta_content_append_tool,
        validate_component_manifests_tool = ctx.executable._validate_component_manifests,
        fuchsia_debug_symbol_info = fuchsia_debug_symbol_info,
        api_level = get_fuchsia_api_level(ctx),
    )

_build_fuchsia_package = rule(
    doc = "Builds a fuchsia package.",
    implementation = _build_fuchsia_package_impl,
    cfg = fuchsia_transition,
    toolchains = [FUCHSIA_TOOLCHAIN_DEFINITION, "@bazel_tools//tools/cpp:toolchain_type"],
    attrs = COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES | {
        "processed_binaries": attr.label(
            doc = "Label to a find_and_process_unstripped_binaries() target for this package.",
            providers = [FuchsiaPackageResourcesInfo, FuchsiaDebugSymbolInfo],
        ),
        "hack_ignore_cpp": attr.bool(
            doc = "This value is no longer used and will be removed shortly.",
            default = False,
        ),
        "_fuchsia_sdk_debug_symbols": attr.label(
            doc = "Include debug symbols from @fuchsia_sdk.",
            default = "@fuchsia_sdk//:debug_symbols",
        ),
        "_meta_content_append_tool": attr.label(
            default = "@fuchsia_rules_common//packages/tools:meta_content_append",
            executable = True,
            cfg = "exec",
        ),
        "_validate_component_manifests": attr.label(
            default = "@fuchsia_rules_common//packages/tools:validate_component_manifests",
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    } | COMPATIBILITY.FUCHSIA_ATTRS | FUCHSIA_API_LEVEL_ATTRS | FUCHSIA_DEBUG_SYMBOLS_ATTRS,
)
