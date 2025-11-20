# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules used to define IDK atoms."""

load(
    "//build/bazel/bazel_idk/private:idk_atom.bzl",
    "idk_atom",
    _idk_noop_atom = "idk_noop_atom",
)
load(
    "//build/bazel/bazel_idk/private:idk_molecule.bzl",
    _idk_host_molecule = "idk_host_molecule",
    _idk_molecule = "idk_molecule",
)
load(
    "//build/bazel/bazel_idk/private:idk_cc_prebuilt_library.bzl",
    _idk_cc_shared_library = "idk_cc_shared_library",
    _idk_cc_shared_library_zx = "idk_cc_shared_library_zx",
    _idk_cc_static_library = "idk_cc_static_library",
    _idk_cc_static_library_zx = "idk_cc_static_library_zx",
)
load(
    "//build/bazel/bazel_idk/private:idk_cc_source_library.bzl",
    _idk_cc_source_library = "idk_cc_source_library",
    _idk_cc_source_library_zx = "idk_cc_source_library_zx",
)
load(
    "//build/bazel/bazel_idk/private:idk_host_tool.bzl",
    _idk_cc_binary_host_tool = "idk_cc_binary_host_tool",
    _idk_host_tool = "idk_host_tool",
)

idk_host_molecule = _idk_host_molecule
idk_molecule = _idk_molecule

idk_noop_atom = _idk_noop_atom

idk_cc_shared_library = _idk_cc_shared_library
idk_cc_shared_library_zx = _idk_cc_shared_library_zx
idk_cc_source_library = _idk_cc_source_library
idk_cc_source_library_zx = _idk_cc_source_library_zx
idk_cc_static_library = _idk_cc_static_library
idk_cc_static_library_zx = _idk_cc_static_library_zx

idk_host_tool = _idk_host_tool
idk_cc_binary_host_tool = _idk_cc_binary_host_tool

def create_idk_atom_for_test(name, testonly, **kwargs):
    """Wrapper to allow creating an atom directly for tests only."""
    if not testonly:
        fail("Atom must be `testonly`.")
    idk_atom(name = name + "_idk", testonly = testonly, **kwargs)
