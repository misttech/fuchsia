# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines a Zircon-specific libraries.

These rules provide functionality similar to the GN template `zx_library()`.

Because the Zircon/kernel toolchain is not yet supported in Bazel, the rules
are currently just thin wrappers around the built-in C++ rules.
"""

load("@rules_cc//cc:defs.bzl", "cc_library")

visibility([
    "//src/devices/...",
    "//src/firmware/lib/...",
    "//src/media/audio/...",
    "//zircon/...",
])

def cc_source_library_zx_impl(
        name,
        includes,
        **kwargs):
    """Implementation for the cc_source_library_zx() macro."""

    # LINT.IfChange

    # `zx_library()` assumes headers files are under `include/`.
    if includes != ["include"]:
        fail('`includes` must be `["include"]`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    cc_library(
        name = name,
        includes = includes,
        **kwargs
    )

cc_source_library_zx = macro(
    doc = "Defines a Zircon C++ source library that will be a `zx_library()` in GN. " +
          "Bazel may create a static library as it does not have a concept of source libraries. " +
          "This macro is for libraries not in the IDK. For IDK libraries, use `idk_cc_source_library_zx()`.",
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_library()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_library,
    implementation = cc_source_library_zx_impl,
    attrs = {
        "includes": attr.string_list(
            doc = 'Path to the root directory for includes. Must always be `["include"]`.',
            mandatory = True,
            configurable = False,
        ),
    },
)
