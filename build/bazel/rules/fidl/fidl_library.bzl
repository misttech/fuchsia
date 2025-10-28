# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for declaring a FIDL library"""

load(":fidl_ir.bzl", "fidlc")

def fidl_library(name, srcs, library_name = None, deps = [], testonly = False, visibility = None):
    """
    A FIDL library.

    Args:
        name: Name of the target.
        library_name: Name of the FIDL library; defaults to `name`.
        srcs: List of source files.
        deps: List of labels for FIDL libraries this library depends on.
        testonly: Standard meaning.
        visibility: Standard meaning.
    """

    if not library_name:
        library_name = name

    fidlc(
        name = name,
        library_name = library_name,
        srcs = srcs,
        deps = deps,
        testonly = testonly,
        visibility = visibility,
    )
