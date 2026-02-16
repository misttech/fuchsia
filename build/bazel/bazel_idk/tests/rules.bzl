# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("//build/bazel/bazel_idk:providers.bzl", "FuchsiaIdkAtomInfo")

# LINT.IfChange(idk_atom_info)
def _create_test_atom_info_impl(ctx):
    # In the actual atom info:
    # * Labels for source files are identified as `<input file target>`
    # * Labels for binaries are identified as `<generated file>`
    # If we just use the label that generates the file, we will get a `<target>`
    # with that label. If we get the file from `DefaultInfo`, it is a
    # `<generated file>`. However, if we use `DefaultInfo` for a source file, it
    # is a `<source file>`. Thus, we must use two different methods for these
    # two cases, matching the implementation.
    # The following code merges the two dictionaries using the appropriate
    # mechanism for each type of file.
    atom_files_map = dict(ctx.attr.atom_files_map_source_files)
    for dest_path, source_file in ctx.attr.atom_files_map_generated_files.items():
        atom_files_map[dest_path] = source_file[DefaultInfo].files.to_list()[0]

    return [
        FuchsiaIdkAtomInfo(
            label = ctx.attr.label.label,
            idk_name = ctx.attr.idk_name,
            id = ctx.attr.id,
            meta_dest = ctx.attr.meta_dest,
            type = ctx.attr.type,
            category = ctx.attr.category,
            is_stable = ctx.attr.is_stable,
            api_area = ctx.attr.api_area,
            api_file_path = ctx.attr.api_file_path,
            api_contents_map = ctx.attr.api_contents_map,
            atom_files_map = atom_files_map,
            idk_deps = ctx.attr.idk_deps,
            atoms_depset = depset(
                direct = ctx.attr.idk_deps,
                transitive = [dep[FuchsiaIdkAtomInfo].atoms_depset for dep in ctx.attr.idk_deps],
            ),
            atom_build_deps = ctx.attr.atom_build_deps,
            additional_prebuild_info = ctx.attr.additional_prebuild_info,
        ),
    ]

create_test_atom_info = rule(
    doc = "Creates a FuchsiaIdkAtomInfo provider for testing.",
    implementation = _create_test_atom_info_impl,
    attrs = {
        "label": attr.label(mandatory = True),
        "idk_name": attr.string(mandatory = True),
        "id": attr.string(mandatory = True),
        "meta_dest": attr.string(mandatory = True),
        "type": attr.string(mandatory = True),
        "category": attr.string(mandatory = True),
        "is_stable": attr.bool(mandatory = True),
        "api_area": attr.string(mandatory = True),
        "api_file_path": attr.label(mandatory = False, allow_single_file = True),
        "api_contents_map": attr.string_keyed_label_dict(mandatory = True, allow_files = True),
        "atom_files_map_source_files": attr.string_keyed_label_dict(
            doc = "Mappings in `atom_files_map` for static source files.",
            mandatory = True,
            allow_files = True,
        ),
        "atom_files_map_generated_files": attr.string_keyed_label_dict(
            doc = "Mappings in `atom_files_map` for generated files. Use the " +
                  "label that generates the file and returns it as the first " +
                  "element of `DefaultInfo.files`.",
            mandatory = True,
            allow_files = True,
        ),
        "idk_deps": attr.label_list(mandatory = True),
        "atom_build_deps": attr.label_list(mandatory = True),
        "additional_prebuild_info": attr.string_dict(mandatory = True),
        "atoms_depset": attr.label_list(mandatory = False),
    },
)
# LINT.ThenChange(//build/bazel/bazel_idk/providers.bzl:idk_atom_info)

def _verify_atom_info_impl(ctx):
    atom_info = ctx.attr.atom[FuchsiaIdkAtomInfo]
    expected_atom_info = ctx.attr.expected_atom_info[FuchsiaIdkAtomInfo]

    # Compare string representations because the objects are not equal.
    if str(atom_info) != str(expected_atom_info):
        # Break the strings on commas to make the output more readable and easy
        # to copy to a diff viewer.
        fail("The actual atom info does not match the expected atom info.\nActual:\n%s\nExpected:\n%s\n" % (
            str(atom_info).replace(",", ",\n"),
            str(expected_atom_info).replace(",", ",\n"),
        ))
    return []

verify_atom_info = rule(
    doc = "Verifies that the actual FuchsiaIdkAtomInfo provider of an atom " +
          "target matches the expected provider representations for testing.",
    implementation = _verify_atom_info_impl,
    attrs = {
        "atom": attr.label(mandatory = True, providers = [FuchsiaIdkAtomInfo]),
        "expected_atom_info": attr.label(mandatory = True, providers = [FuchsiaIdkAtomInfo]),
    },
)
