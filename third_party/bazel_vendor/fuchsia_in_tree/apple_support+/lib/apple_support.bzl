# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Stub of @apple_support//lib:apple_support.bzl.

Only the subset of the upstream API that is loaded by other vendored
repositories (e.g. @rules_rust//cargo/private:cargo_build_script.bzl) is
provided here. Values mirror upstream apple_support 1.24.1.
"""

def _sdkroot_path_placeholder():
    """Returns a placeholder value to be replaced with SDKROOT during action execution."""
    return "__BAZEL_XCODE_SDKROOT__"

def _xcode_path_placeholder():
    """Returns a placeholder value to be replaced with DEVELOPER_DIR during action execution."""
    return "__BAZEL_XCODE_DEVELOPER_DIR__"

apple_support = struct(
    path_placeholders = struct(
        sdkroot = _sdkroot_path_placeholder,
        xcode = _xcode_path_placeholder,
    ),
)
