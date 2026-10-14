# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""fuchsia_cxx_version derived value."""

load("@fuchsia_build_info//:args.bzl", "experimental_cxx_version")

# LINT.IfChange(fuchsia_cxx_version)
_default_cxx_version = 23

fuchsia_cxx_version = (
    int(experimental_cxx_version) if experimental_cxx_version else _default_cxx_version
)

# The C++ standard versions supported by the Fuchsia SDK.
#
# This list is defined in //docs/development/idk/documentation/compilation.md.
_base_sdk_cxx_supported_versions = [
    17,
    20,
    23,
]

fuchsia_sdk_cxx_supported_versions = (
    [v for v in _base_sdk_cxx_supported_versions if v != fuchsia_cxx_version] + [fuchsia_cxx_version] if experimental_cxx_version else _base_sdk_cxx_supported_versions
)
# LINT.ThenChange(//build/config/fuchsia_cxx_version.gni:fuchsia_cxx_version)
