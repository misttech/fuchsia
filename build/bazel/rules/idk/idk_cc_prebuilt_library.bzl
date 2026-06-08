# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Forwarding definitions for IDK C/C++ prebuilt libraries."""

load(
    "//build/bazel/rules/idk/private:idk_cc_prebuilt_library.bzl",
    _idk_cc_shared_library = "idk_cc_shared_library",
    _idk_cc_shared_library_zx = "idk_cc_shared_library_zx",
    _idk_cc_static_library = "idk_cc_static_library",
    _idk_cc_static_library_zx = "idk_cc_static_library_zx",
)

idk_cc_shared_library = _idk_cc_shared_library
idk_cc_shared_library_zx = _idk_cc_shared_library_zx
idk_cc_static_library = _idk_cc_static_library
idk_cc_static_library_zx = _idk_cc_static_library_zx
