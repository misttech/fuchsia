# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common functions for IDK macros."""

load("@bazel_skylib//lib:paths.bzl", "paths")

visibility(["//build/bazel/rules/fidl/..."])

def json_encode_dict_values(dict):
    """Returns the dictionary with each top-level value encoded as a JSON string.

This allows the dictionary to be passed to a `string_dict` attribute.
    """
    return {k: json.encode(v) for k, v in dict.items()}

def select_for(condition, condition_value, default_value = []):
    return select({
        condition: condition_value,
        "//conditions:default": default_value,
    })

def select_for_fuchsia(fuchsia_value, non_fuchsia_value = []):
    return select_for("@platforms//os:fuchsia", fuchsia_value, non_fuchsia_value)

def _get_idk_label(label_str):
    # Ensure the label is relative to the `BUILD` file, not this `.bzl` file
    # in cases where `label_str` omits the package (e.g., ":target_name").
    label = native.package_relative_label(label_str)

    # Build the label to handle cases where `label_str` omits the target name
    # (e.g., "//path/to/package").
    return "//{}:{}_idk".format(label.package, label.name)

def get_idk_deps(underlying_deps):
    return [_get_idk_label(dep) for dep in underlying_deps]

def get_allowlist_target(type, category, stable, prebuilt_library_format = None):
    """Returns the allowlist target for the combination of parameters.

    All atoms must be in an allowlist.
    """
    if prebuilt_library_format and type != "cc_prebuilt_library":
        fail("`prebuilt_library_format` is only valid for the 'cc_prebuilt_library' type.")

    if type == "cc_source_library":
        if category == "partner":
            if stable:
                return "//build/bazel/bazel_idk:partner_idk_cc_source_library_allowlist"
            else:
                return "//build/bazel/bazel_idk:partner_idk_unstable_cc_source_library_allowlist"
    elif type == "cc_prebuilt_library":
        if category == "partner" and stable:
            if prebuilt_library_format == "shared":
                return "//build/bazel/bazel_idk:partner_idk_cc_prebuilt_shared_library_allowlist"
            elif prebuilt_library_format == "static":
                return "//build/bazel/bazel_idk:partner_idk_cc_prebuilt_static_library_allowlist"
            else:
                fail("Unrecognized prebuilt library format: '%s'" % prebuilt_library_format)
    elif type == "data":
        if category == "partner" and stable:
            return "//build/bazel/bazel_idk:partner_idk_data_allowlist"
    elif type == "fidl_library":
        if category == "partner":
            if stable:
                return "//sdk/fidl:partner_idk_fidl_library_allowlist"
            else:
                return "//sdk/fidl:partner_idk_unstable_fidl_library_allowlist"
        elif category == "":
            return "//sdk/fidl:no_category_fidl_library_allowlist"
    elif type == "host_tool":
        if category == "partner" and stable:
            return "//build/bazel/bazel_idk:partner_idk_host_tool_allowlist"
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

def get_api_file_path(idk_name, stable, api_file_path):
    """Returns the string path to the API file for the given IDK atom.

    If `stable` is False, `api_file_path` must be `None` and `None` is returned.
    If `stable` is True and `api_file_path` is not specified, the returned path
    is "<idk_name>.api". Otherwise, `api_file_path` is returned.

    `api_file_path` must not specify the default path. This is verified when
    possible.

    Args:
        idk_name: String name of this atom within the IDK.
        stable: Whether this atom is stabilized.
        api_file_path: String path for the file representing the API exposed by
            this atom. Overrides the default path. Can be `None`.

    Returns:
        The string path to the API file for the given IDK target. May be `None`.
    """
    if stable:
        default_api_path = idk_name + ".api"
        if api_file_path:
            # Check that `api_file_path` does not specify the default path.
            # We must assume that absolute paths are not specifying the default
            # path because `relativize()` fails with absolute paths and we
            # cannot get the package path at this point.
            if not paths.is_absolute(api_file_path):
                if paths.relativize(api_file_path, ".") == paths.relativize(default_api_path, "."):
                    fail("The specified `api_file_path` (`%s`) matches the default. `api_file_path` only needs to be specified when overriding the default." % api_file_path)

            return api_file_path
        else:
            return default_api_path
    else:
        if api_file_path != None:
            fail("`api_file_path` must only be specified for stable IDK atoms.")
        return None
