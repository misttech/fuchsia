# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK molecule."""

load("//build/bazel/bazel_idk:providers.bzl", "FuchsiaIdkAtomInfo", "FuchsiaIdkMoleculeInfo")

visibility(["//build/bazel/bazel_idk/..."])

def _idk_molecule_impl(ctx):
    if not ctx.attr.name.endswith("_idk"):
        fail("IDK molecule `name`s must end with `_idk`.")

    if (len(ctx.attr.target_compatible_with) != 1 or
        str(ctx.attr.target_compatible_with[0].label) != "@@platforms//os:fuchsia"):
        fail('`target_compatible_with` must be `["@platforms//os:fuchsia"]`.')

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
    },
)

idk_host_tool_molecule = rule(
    doc = """Generate an IDK molecule containing atoms for the host.

    * `name` must end with '_idk' (unlike most other IDK macros).
    * `target_compatible_with` must be `["@platforms//os:fuchsia"]`.
      The `deps` will be built for the host platforms via a transition.

    Currently only supports the current host platform.
    """,
    implementation = _idk_molecule_impl,
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
    },
)
