# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules used to define IDK atoms."""

load("//build/bazel/bazel_idk/private:idk_atom.bzl", "idk_atom")
load(
    "//build/bazel/bazel_idk/private:idk_molecule.bzl",
    _idk_molecule = "idk_molecule",
)
load(
    "//build/bazel/bazel_idk/private:idk_cc_prebuilt_library.bzl",
    "idk_cc_prebuilt_library",
)
load(
    "//build/bazel/bazel_idk/private:idk_cc_source_library.bzl",
    _idk_cc_source_library = "idk_cc_source_library",
    _idk_cc_source_library_zx = "idk_cc_source_library_zx",
)
load(
    "//build/bazel/bazel_idk/private:idk_host_tool.bzl",
    _idk_host_tool = "idk_host_tool",
)

idk_molecule = _idk_molecule
idk_cc_source_library = _idk_cc_source_library
idk_cc_source_library_zx = _idk_cc_source_library_zx
idk_host_tool = _idk_host_tool

def _idk_cc_shared_library_impl(name, **kwargs):
    idk_cc_prebuilt_library(name = name, prebuilt_library_type = "shared", **kwargs)

idk_cc_shared_library = macro(
    doc = """Defines a C++ prebuilt shared library that can be exported to an IDK.""",
    inherit_attrs = idk_cc_prebuilt_library,
    attrs = {
        # Do not inherit as this attribute is specified in the implementation.
        "prebuilt_library_type": None,
    },
    implementation = _idk_cc_shared_library_impl,
)

def _idk_cc_static_library_impl(name, **kwargs):
    idk_cc_prebuilt_library(name = name, prebuilt_library_type = "static", **kwargs)

idk_cc_static_library = macro(
    doc = """Defines a C++ prebuilt static library that can be exported to an IDK.""",
    inherit_attrs = idk_cc_prebuilt_library,
    attrs = {
        # Do not inherit as this attribute is specified in the implementation.
        "prebuilt_library_type": None,
        # Disallow an empty list, unlike the underlying macro.
        "hdrs": attr.label_list(
            doc = """The list of C and C++ header files published by this library to be directly
included by sources in dependent rules. Does not include internal headers that are included from
public headers but not meant to be included by dependents - see `hdrs_for_internal_use`.
Atoms providing headers used by these headers must be included in the (public) `deps`.
May only be empty if `no_headers` is True.
GN equivalent: `public`
GN note: Unlike the GN template, this list does not include `hdrs_for_internal_use`.""",
            allow_files = True,
            allow_empty = False,
            mandatory = True,
            configurable = False,
        ),
        # Do not inherit as this attribute is not supported.
        "output_name": None,
    },
    implementation = _idk_cc_static_library_impl,
)

def create_idk_atom_for_test(name, testonly, **kwargs):
    """Wrapper to allow creating an atom directly for tests only."""
    if not testonly:
        fail("Atom must be `testonly`.")
    idk_atom(name = name + "_idk", testonly = testonly, **kwargs)
