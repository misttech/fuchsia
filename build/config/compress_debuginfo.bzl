# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A custom rule to compute the build_flags() values for //build/config:compress_debuginfo.

This supports any string value set in args.gn, or changed per platform.
"""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_rules_common//build_flags:providers.bzl", "BuildFlagsInfo")

def _compress_debuginfo_build_flags_impl(ctx):
    # LINT.IfChange(compress_debuginfo)
    cflags = []
    ldflags = []
    rustflags = []

    compress_debuginfo = ctx.attr._flag[BuildSettingInfo].value
    if compress_debuginfo != "none":
        gzflag = "-gz={}".format(compress_debuginfo)
        cflags = [gzflag]

        # NOTE: asmflags not supported. Bazel uses cflags.
        # asmflags = cflags
        ldflags = cflags

        # Unlike GN, Bazel's @rules_rust invokes the Clang driver as the linker
        # on both Fuchsia and host platforms, so we use the Clang flag spelling.
        rustflags = ["-Clink-arg={}".format(gzflag)]

    # LINT.ThenChange(BUILD.gn:compress_debuginfo)
    return [
        BuildFlagsInfo(
            label = ctx.label,
            cflags = cflags,
            ldflags = ldflags,
            rustflags = rustflags,
        ),
    ]

compress_debuginfo_build_flags = rule(
    doc = "Create a build_flags() target from the compress_debuginfo toolchain-specific variable.",
    implementation = _compress_debuginfo_build_flags_impl,
    attrs = {
        "_flag": attr.label(
            doc = "Label to the compress_debuginfo string_flag() target",
            default = "//build/bazel/config/variables:compress_debuginfo",
            providers = [BuildSettingInfo],
        ),
    },
)
