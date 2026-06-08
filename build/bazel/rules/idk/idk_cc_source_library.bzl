# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Public macros for defining C/C++ source libraries in the IDK."""

load(
    "//build/bazel/rules/idk/private:idk_cc_source_library.bzl",
    _idk_cc_source_library = "idk_cc_source_library",
    _idk_cc_source_library_zx = "idk_cc_source_library_zx",
)
load(
    "//build/bazel/rules/idk/private:idk_common.bzl",
    "get_api_file_path",
)

def idk_cc_source_library(idk_name, category, stable, api_file_path = None, **kwargs):
    """Defines a C++ source library that can be exported to an IDK.

    This is a wrapper around `_idk_cc_source_library()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_source_library()` for documentation.
    """
    _idk_cc_source_library(
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        **kwargs
    )

def idk_cc_source_library_zx(idk_name, category, stable, api_file_path = None, **kwargs):
    """Defines a C++ source library that can be exported to an IDK and will be a `zx_library()` in GN.

    This is a wrapper around `_idk_cc_source_library_zx()` that supports a
    default value for `api_file_path` and sets the allowlist.

    See `_idk_cc_source_library_zx()` for documentation.
    """
    _idk_cc_source_library_zx(
        idk_name = idk_name,
        category = category,
        stable = stable,
        api_file_path = get_api_file_path(idk_name, stable, api_file_path),
        **kwargs
    )
