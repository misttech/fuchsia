# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines an IDK molecule."""

load(":idk_atom.bzl", "FuchsiaIdkAtomInfo")

FuchsiaIdkMoleculeInfo = provider(
    doc = "Defines an IDK molecule, or group of atoms",
    fields = {
        "label": "The molecule's label",
        "idk_deps": "Atoms and other molecules the molecule depends on.",
        "atoms_depset": "depset[FuchsiaIdkAtomInfo] The full set atoms that make up the molecule",
    },
)

def _idk_molecule_impl(ctx):
    all_deps_depset = depset(direct = ctx.files.deps)
    idk_deps = ctx.attr.deps

    # Build the atoms depset, excluding molecules while including their atoms.
    direct_deps = []
    transitive_depsets = []
    for dep in idk_deps:
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
            idk_deps = ctx.attr.deps,
            atoms_depset = atoms_depset,
        ),
    ]

idk_molecule = rule(
    doc = "Generate an IDK molecule.",
    implementation = _idk_molecule_impl,
    attrs = {
        "deps": attr.label_list(
            providers = [[FuchsiaIdkAtomInfo], [FuchsiaIdkMoleculeInfo]],
            mandatory = True,
            doc = "Atoms and other molecules the molecule depends on.",
        ),
    },
)
