# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@rules_cc//cc:defs.bzl", "cc_library")

def _fx_cc_library_impl(
        name,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Implementation for the fx_cc_library() macro."""

    cc_library(
        name = name,
        **kwargs
    )

fx_cc_library = macro(
    doc = """Wrapper for cc_library() libraries for Fuchsia.

  For now, it just allows passing attributes to bazel2gn.
  """,
    implementation = _fx_cc_library_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_library()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_library,
    attrs = {
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
    },
)
