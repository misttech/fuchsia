# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Public macros for defining C/C++ prebuilt libraries in the IDK."""

load(
    "//build/bazel/rules/idk/private:idk_cc_prebuilt_library.bzl",
    _get_ifs_golden_file_name = "get_ifs_golden_file_name",
    _get_shared_library_output_name = "get_shared_library_output_name",
    _idk_cc_shared_library = "idk_cc_shared_library",
    _idk_cc_shared_library_zx = "idk_cc_shared_library_zx",
    _idk_cc_static_library = "idk_cc_static_library",
    _idk_cc_static_library_zx = "idk_cc_static_library_zx",
)
load(
    "//build/bazel/rules/idk/private:idk_common.bzl",
    "get_api_file_path",
    "get_golden_file",
)

def idk_cc_shared_library(name, idk_name, category, api_file_path = None, output_name = "", **kwargs):
    """Defines a C++ prebuilt shared library that can be exported to an IDK.

    This is a wrapper around `_idk_cc_shared_library()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_shared_library()` for documentation.
    """
    stable = True
    output_name = _get_shared_library_output_name(name, output_name)

    _idk_cc_shared_library(
        name = name,
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        output_name = output_name,
        ifs_golden_file = get_golden_file(_get_ifs_golden_file_name(output_name), support_platform = True),
        **kwargs
    )

def idk_cc_static_library(idk_name, category, api_file_path = None, **kwargs):
    """Defines a C++ prebuilt static library that can be exported to an IDK.

    This is a wrapper around `_idk_cc_static_library()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_static_library()` for documentation.
    """
    stable = True

    _idk_cc_static_library(
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        **kwargs
    )

def idk_cc_shared_library_zx(name, idk_name, category, api_file_path = None, output_name = "", **kwargs):
    """Defines a C++ shared library that can be exported to an IDK and will be a `zx_library()` in GN.

    This is a wrapper around `_idk_cc_shared_library_zx()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_shared_library_zx()` for documentation.
    """
    stable = True
    output_name = _get_shared_library_output_name(name, output_name)

    _idk_cc_shared_library_zx(
        name = name,
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        output_name = output_name,
        ifs_golden_file = get_golden_file(_get_ifs_golden_file_name(output_name), support_platform = True),
        **kwargs
    )

def idk_cc_static_library_zx(idk_name, category, api_file_path = None, **kwargs):
    """Defines a C++ static library that can be exported to an IDK and will be a `zx_library()` in GN.

    This is a wrapper around `_idk_cc_static_library_zx()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_static_library_zx()` for documentation.
    """
    stable = True

    _idk_cc_static_library_zx(
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        **kwargs
    )
