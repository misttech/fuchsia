# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Definitions for the GIDL conformance test suite."""

visibility([
    "//sdk/ctf/tests/pkg/fidl/...",
    "//sdk/lib/fidl/...",
    "//src/lib/fidl/...",
    "//src/lib/fidl_codec/...",
    "//src/tests/benchmarks/fidl/...",
    "//src/tests/fidl/...",
    "//tools/fidl/...",
])

# LINT.IfChange
CONFORMANCE_SUITE_GIDL_FILES = [
    "//src/tests/fidl/conformance_suite:aliases.gidl",
    "//src/tests/fidl/conformance_suite:alignment.gidl",
    "//src/tests/fidl/conformance_suite:bits_flexible.gidl",
    "//src/tests/fidl/conformance_suite:bits_strict.gidl",
    "//src/tests/fidl/conformance_suite:bool.gidl",
    "//src/tests/fidl/conformance_suite:conformance.gidl",
    "//src/tests/fidl/conformance_suite:empty_struct.gidl",
    "//src/tests/fidl/conformance_suite:enums_flexible.gidl",
    "//src/tests/fidl/conformance_suite:enums_strict.gidl",
    "//src/tests/fidl/conformance_suite:float.gidl",
    "//src/tests/fidl/conformance_suite:golden.gidl",
    "//src/tests/fidl/conformance_suite:handles.gidl",
    "//src/tests/fidl/conformance_suite:integer.gidl",
    "//src/tests/fidl/conformance_suite:memcpy_compatibility.gidl",
    "//src/tests/fidl/conformance_suite:padding.gidl",
    "//src/tests/fidl/conformance_suite:ported_from_walker_handles.gidl",
    "//src/tests/fidl/conformance_suite:ported_from_walker_strings.gidl",
    "//src/tests/fidl/conformance_suite:ported_from_walker_vectors.gidl",
    "//src/tests/fidl/conformance_suite:protocol_endpoints.gidl",
    "//src/tests/fidl/conformance_suite:recursive_depth.gidl",
    "//src/tests/fidl/conformance_suite:recursive_types.gidl",
    "//src/tests/fidl/conformance_suite:strings.gidl",
    "//src/tests/fidl/conformance_suite:strings_utf8.gen.gidl",
    "//src/tests/fidl/conformance_suite:tables.gidl",
    "//src/tests/fidl/conformance_suite:transformer1.gidl",
    "//src/tests/fidl/conformance_suite:transformer2.gidl",
    "//src/tests/fidl/conformance_suite:union.gidl",
    "//src/tests/fidl/conformance_suite:vectors.gidl",
]

CONFORMANCE_SUITE_FIDL_TARGET = "//src/tests/fidl/conformance_suite:conformance_fidl"

CONFORMANCE_SUITE_GOLDEN_GIDL_FILES = ["//src/tests/fidl/conformance_suite:golden.gidl"]
# LINT.ThenChange(//src/tests/fidl/conformance_suite/gidl-conformance-suite.gni)
