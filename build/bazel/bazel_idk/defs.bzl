# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules used to define IDK atoms."""

load(
    "//build/bazel/rules/idk:idk_atom.bzl",
    _idk_noop_atom = "idk_noop_atom",
)
load(
    "//build/bazel/rules/idk:idk_cc_prebuilt_library.bzl",
    _idk_cc_shared_library = "idk_cc_shared_library",
    _idk_cc_shared_library_zx = "idk_cc_shared_library_zx",
    _idk_cc_static_library = "idk_cc_static_library",
    _idk_cc_static_library_zx = "idk_cc_static_library_zx",
)
load(
    "//build/bazel/rules/idk:idk_cc_source_library.bzl",
    _idk_cc_source_library = "idk_cc_source_library",
    _idk_cc_source_library_zx = "idk_cc_source_library_zx",
)
load(
    "//build/bazel/rules/idk:idk_data.bzl",
    _idk_data = "idk_data",
)
load(
    "//build/bazel/rules/idk:idk_host_tool.bzl",
    _idk_cc_binary_host_tool = "idk_cc_binary_host_tool",
    _idk_go_binary_host_tool = "idk_go_binary_host_tool",
    _idk_rustc_binary_host_tool = "idk_rustc_binary_host_tool",
)
load(
    "//build/bazel/rules/idk:idk_molecule.bzl",
    _idk_molecule = "idk_molecule",
)

idk_molecule = _idk_molecule

idk_noop_atom = _idk_noop_atom

idk_data = _idk_data

idk_cc_shared_library = _idk_cc_shared_library
idk_cc_shared_library_zx = _idk_cc_shared_library_zx
idk_cc_source_library = _idk_cc_source_library
idk_cc_source_library_zx = _idk_cc_source_library_zx
idk_cc_static_library = _idk_cc_static_library
idk_cc_static_library_zx = _idk_cc_static_library_zx

idk_cc_binary_host_tool = _idk_cc_binary_host_tool
idk_go_binary_host_tool = _idk_go_binary_host_tool
idk_rustc_binary_host_tool = _idk_rustc_binary_host_tool
