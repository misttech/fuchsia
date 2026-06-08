# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Forwarding definitions for IDK C/C++ source libraries."""

load(
    "//build/bazel/rules/idk/private:idk_cc_source_library.bzl",
    _idk_cc_source_library = "idk_cc_source_library",
    _idk_cc_source_library_zx = "idk_cc_source_library_zx",
)

idk_cc_source_library = _idk_cc_source_library
idk_cc_source_library_zx = _idk_cc_source_library_zx
