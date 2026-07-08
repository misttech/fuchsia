# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Non-SDK version of fuchsia_package rule for platform building."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "find_and_process_unstripped_binaries",
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
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "fuchsia_find_all_package_resources",
)

def fx_package(
        *,
        name,
        package_name = None,
        archive_name = None,
        platform = None,
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

    ```
    fuchsia_package(
        name = "pkg",
        components = [":my_component"],
        tools = [":my_tool"]
    )
    ```

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
        platform: Optionally override the platform to build the package for.
        tags: Forward additional tags to all generated targets.
        **kwargs: extra attributes to pass along to the build rule.
    """
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
        name = name,
        components = components,
        resources = resources,
        processed_binaries = processed_binaries,
        collected_resources = collected_resources,
        tools = tools,
        subpackages = subpackages,
        subpackages_to_flatten = subpackages_to_flatten,
        package_name = package_name or name,
        archive_name = archive_name,
        platform = platform,
        tags = tags + ["manual"],
        **kwargs
    )

def _build_fuchsia_package_impl(ctx):
    fuchsia_debug_symbol_info = FuchsiaDebugSymbolInfo(build_id_dirs_mapping = {})

    return common_build_fuchsia_package_impl(
        ctx,
        ffx_package = ctx.executable._package_tool,
        ffx_package_is_ffx = False,
        cmc_tool = ctx.file._cmc_tool,
        meta_content_append_tool = ctx.executable._meta_content_append_tool,
        validate_component_manifests_tool = ctx.executable._validate_component_manifests,
        fuchsia_debug_symbol_info = fuchsia_debug_symbol_info,
        api_level = ctx.attr._current_api_level[BuildSettingInfo].value,
    )

_build_fuchsia_package = rule(
    implementation = _build_fuchsia_package_impl,
    attrs = COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES | {
        "processed_binaries": attr.label(
            doc = "Label to a find_and_process_unstripped_binaries() target for this package.",
            providers = [FuchsiaPackageResourcesInfo, FuchsiaDebugSymbolInfo],
        ),
        "_package_tool": attr.label(
            # TODO(b/519244675): Replace with a Bazel label once `package-tool` is migrated to Bazel.
            default = "@gn_targets//toolchain_host_x64/src/sys/pkg/bin/package-tool",
            executable = True,
            cfg = "exec",
        ),
        "_cmc_tool": attr.label(
            # TODO(b/519243783): Replace with a Bazel label once `cmc` is migrated to Bazel.
            default = "@gn_targets//toolchain_host_x64/tools/cmc",
            allow_single_file = True,
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
        "_current_api_level": attr.label(
            default = "@//build/bazel/versioning:api_level",
        ),
    },
)
