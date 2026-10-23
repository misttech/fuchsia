# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Forwarding definitions for IDK ffx tools."""

load(
    "//build/bazel/rules/idk/private:idk_ffx_tool.bzl",
    _idk_ffx_tool = "idk_ffx_tool",
)

visibility([
    "//build/bazel/bazel_idk/tests/...",
    "//src/developer/ffx/...",
])

idk_ffx_tool = _idk_ffx_tool
