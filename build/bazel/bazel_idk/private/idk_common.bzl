# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common functions for IDK macros."""

def json_encode_dict_values(dict):
    """Returns the dictionary with each top-level value encoded as a JSON string.

This allows the dictionary to be passed to a `string_dict` attribute.
    """
    return {k: json.encode(v) for k, v in dict.items()}

def _get_idk_label(label_str):
    # Ensure the label is relative to the `BUILD` file, not this `.bzl` file
    # in cases where `label_str` omits the package (e.g., ":target_name").
    label = native.package_relative_label(label_str)

    # Build the label to handle cases where `label_str` omits the target name
    # (e.g., "//path/to/package").
    return "//{}:{}_idk".format(label.package, label.name)

def get_idk_deps(underlying_deps):
    return [_get_idk_label(dep) for dep in underlying_deps]

def get_allowlist_target(type, category, stable):
    """Returns the allowlist target for the combination of parameters.

    All atoms must be in an allowlist.
    """
    if type == "cc_source_library":
        if category == "partner":
            if stable:
                return "//build/bazel/bazel_idk:partner_idk_cc_source_library_allowlist"
            else:
                return "//build/bazel/bazel_idk:partner_idk_unstable_cc_source_library_allowlist"
    elif type == "data":
        if category == "partner" and stable:
            return "//build/bazel/bazel_idk:partner_idk_data_allowlist"
    else:
        fail("Unhandled atom type: %s" % type)

    fail("Create a separate allowlist when adding support for other categories or stability.")

def get_atom_visibility(target_visibility):
    """Returns the visibility to use for an atom target.

    The atom's visibility should allow IDK contents/definition rules to depend
    on the atom in addition to the visibility specified to the macro.
    The built-in visibility labels cannot be used in combination with other
    labels so handle them specifically.
    """

    # TODO(https://fxbug.dev/431287514): Support package `default_visibility`.
    if "//visibility:public" in target_visibility:
        return target_visibility

    # All atoms must be visible to the targets defining the IDK.
    atom_visibility = ["//sdk:__pkg__"]

    if "//visibility:private" not in target_visibility:
        atom_visibility += target_visibility

    return atom_visibility
