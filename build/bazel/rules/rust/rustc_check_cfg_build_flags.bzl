# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A macro to define build_flags() for Rust check-cfg flags."""

load("@fuchsia_rules_common//build_flags:build_flags.bzl", "build_flags")

def _rustc_check_cfg_build_flags_impl(
        name,
        cfgs,
        features,
        visibility):
    all_cfgs = list(cfgs)
    if features:
        values = ", ".join(["\"{}\"".format(f) for f in features])
        all_cfgs.append("feature, values({})".format(values))

    flags = ["--check-cfg=cfg({})".format(c) for c in all_cfgs]

    build_flags(
        name = name,
        rustflags = flags,
        visibility = visibility,
    )

rustc_check_cfg_build_flags = macro(
    doc = "Generates a `build_flags` target with `--check-cfg` flags from cfgs and features.",
    implementation = _rustc_check_cfg_build_flags_impl,
    attrs = {
        "cfgs": attr.string_list(
            doc = "List of cfg names to check (e.g. `[\"test\"]`).",
            default = [],
            configurable = False,
        ),
        "features": attr.string_list(
            doc = "List of feature names to check (e.g. `[\"benchmark\"]`).",
            default = [],
            configurable = False,
        ),
    },
)
