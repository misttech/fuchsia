# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK molecule."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_build_info//:args.bzl", "target_cpu")
load(
    "//build/bazel/bazel_idk:providers.bzl",
    "FuchsiaIdkAtomInfo",
    "FuchsiaIdkMoleculeInfo",
)
load("//build/bazel/rules:current_platform_info.bzl", "CurrentPlatformInfo")
load(":idk_transitions.bzl", "api_level_and_cpu_combinations_transition")

visibility([
    "//build/bazel/bazel_idk/...",
    "//sdk",
])

def _idk_molecule_common_impl(ctx, allowed_in_configurations):
    if not ctx.attr.name.endswith("_idk"):
        fail("IDK molecule `name`s must end with `_idk`.")

    if (len(ctx.attr.target_compatible_with) != 1 or
        str(ctx.attr.target_compatible_with[0].label) != "@@platforms//os:fuchsia"):
        fail('`target_compatible_with` must be `["@platforms//os:fuchsia"]`.')

    if allowed_in_configurations != "fuchsia":
        api_level = ctx.attr._current_api_level[BuildSettingInfo].value
        if api_level != "PLATFORM":
            fail('This molecule is only to be built at the "PLATFORM" API level, not "%s".' %
                 api_level)
    if allowed_in_configurations == "once":
        current_cpu = ctx.attr._current_platform[CurrentPlatformInfo].cpu
        if current_cpu != target_cpu:
            fail('This molecule is only to be built for the target CPU architecture ("%s"), not "%s".' %
                 (target_cpu, current_cpu))

    all_deps_depset = depset(direct = ctx.files.deps)

    # Build the atoms depset, excluding molecules while including their atoms.
    direct_deps = []
    transitive_depsets = []
    for dep in ctx.attr.deps:
        if FuchsiaIdkAtomInfo in dep:
            direct_deps.append(dep)
            transitive_depsets.append(dep[FuchsiaIdkAtomInfo].atoms_depset)
        elif FuchsiaIdkMoleculeInfo in dep:
            transitive_depsets.append(dep[FuchsiaIdkMoleculeInfo].atoms_depset)
        else:
            fail("Unexpected dependency %s. Must be an atom or a molecule." % dep)

    atoms_depset = depset(direct = direct_deps, transitive = transitive_depsets)

    return [
        DefaultInfo(files = all_deps_depset),
        FuchsiaIdkMoleculeInfo(
            label = ctx.label,
            deps = ctx.attr.deps,
            atoms_depset = atoms_depset,
        ),
    ]

COMMON_MOLECULE_ATTRS = {
    "_current_api_level": attr.label(
        default = "@//build/bazel:fuchsia_api_level",
    ),
    "_current_platform": attr.label(
        providers = [CurrentPlatformInfo],
        default = "@//build/bazel:current_platform",
    ),
}

def _idk_molecule_impl(ctx):
    return _idk_molecule_common_impl(ctx, ctx.attr.allowed_in_configurations)

idk_molecule = rule(
    doc = """Generate an IDK molecule containing atoms for Fuchsia targets.

    * `name` must end with '_idk' (unlike most other IDK macros).
    * `target_compatible_with` must be `["@platforms//os:fuchsia"]`.
    """,
    implementation = _idk_molecule_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "Atoms and other molecules the molecule depends on.",
            providers = [[FuchsiaIdkAtomInfo], [FuchsiaIdkMoleculeInfo]],
            mandatory = True,
        ),
        "allowed_in_configurations": attr.string(
            doc = """The configurations in which this molecule is allowed to be built.

            * "fuchsia": The molecule can be built in any Fuchsia configuration,
              including the main "PLATFORM" build, any API level, and any CPU
              architecture.
            * "once": The molecule can only be built in one configuration, the
              main "PLATFORM" build.
            * "PLATFORM": The molecule can only be built at the "PLATFORM"
              API level, in any CPU architecture.
            """,
            values = ["fuchsia", "once", "PLATFORM"],
            mandatory = True,
        ),
    } | COMMON_MOLECULE_ATTRS,
)

def _idk_all_api_level_and_cpu_combinations_molecule_impl(ctx):
    return _idk_molecule_common_impl(ctx, allowed_in_configurations = "once")

idk_all_api_level_and_cpu_combinations_molecule = rule(
    doc = """Generate an IDK molecule containing molecules for each combination of IDK buildable API level and CPU architecture.

    Each molecule in `deps` will be replicated for each combination of API level
    and CPU architecture supported by the IDK build.

    * `name` must end with '_idk' (unlike most other IDK macros).
    * `target_compatible_with` must be `["@platforms//os:fuchsia"]`.

    Only supported in the main "PLATFORM" Fuhsia build.
    """,
    implementation = _idk_all_api_level_and_cpu_combinations_molecule_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "Molecules to be replicated for each combination of IDK buildable API level and CPU architecture.",
            providers = [FuchsiaIdkMoleculeInfo],
            mandatory = True,
            cfg = api_level_and_cpu_combinations_transition,
        ),
    } | COMMON_MOLECULE_ATTRS,
)

def _idk_host_tool_molecule_impl(ctx):
    return _idk_molecule_common_impl(ctx, allowed_in_configurations = "once")

idk_host_tool_molecule = rule(
    doc = """Generate an IDK molecule containing atoms for the host.

    * `name` must end with '_idk' (unlike most other IDK macros).
    * `target_compatible_with` must be `["@platforms//os:fuchsia"]`.
      The `deps` will be built for the host platforms via a transition.

    Only supported in the main "PLATFORM" Fuhsia build.

    Currently only supports the current host platform.
    """,
    implementation = _idk_host_tool_molecule_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "Atoms and other molecules the molecule depends on.",
            providers = [[FuchsiaIdkAtomInfo], [FuchsiaIdkMoleculeInfo]],
            mandatory = True,
            # TODO(https://fxbug.dev/442025401): Use a custom transition that
            # uses the more permissive sysroot. See https://fxbug.dev/484422864.
            # TODO(https://fxbug.dev/442025401): Support a 1:2 transition that
            # builds the host tools for both x64 and arm64 when appropriate.
            cfg = "exec",
        ),
    } | COMMON_MOLECULE_ATTRS,
)
