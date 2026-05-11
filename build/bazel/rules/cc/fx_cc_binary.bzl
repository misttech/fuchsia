# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@rules_cc//cc:defs.bzl", "cc_binary")

def _fx_cc_binary_impl(
        name,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Implementation for the fx_cc_binary() macro."""

    cc_binary(
        name = name,
        **kwargs
    )

fx_cc_binary = macro(
    doc = """Wrapper for cc_binary() binaries for Fuchsia.

  For now, it just allows passing attributes to bazel2gn.
  """,
    implementation = _fx_cc_binary_impl,
    # TODO(https://fxbug.dev/446694542): Remove `native.` once the
    # `cc_binary()` wrapper is a symbolic macro.
    inherit_attrs = native.cc_binary,
    attrs = {
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
    },
)
