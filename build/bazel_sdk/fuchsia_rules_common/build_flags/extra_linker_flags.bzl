# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Implementation of the extra_linker_flags() rule.

See README.md for full technical details.
"""

load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load(":providers.bzl", "BuildFlagsInfo")

def _extra_linker_flags_impl(ctx):
    return [
        BuildFlagsInfo(
            label = ctx.label,
            ldflags = list(ctx.attr.ldflags),
            link_inputs = list(ctx.files.link_inputs),
            link_deps = [dep[CcInfo] for dep in ctx.attr.link_deps],
            link_deps_runfiles = [
                dep[DefaultInfo].default_runfiles
                for dep in ctx.attr.link_deps
                if DefaultInfo in dep and dep[DefaultInfo].default_runfiles != None
            ],
            executable_link_deps = [dep[CcInfo] for dep in ctx.attr.executable_link_deps],
            executable_link_deps_runfiles = [
                dep[DefaultInfo].default_runfiles
                for dep in ctx.attr.executable_link_deps
                if DefaultInfo in dep and dep[DefaultInfo].default_runfiles != None
            ],
        ),
    ]

extra_linker_flags = rule(
    doc = """Define a target exposing linker flags, additional linker input files, and linker library dependencies.

    Unlike `build_flags()`, this rule only accepts `ldflags`, `link_inputs`, `link_deps`, and `executable_link_deps`,
    making it easy to search for targets that inject linker inputs or library dependencies.
    It provides `BuildFlagsInfo`, so it can be used directly in `build_flags = [...]` on
    `fx_cc_*()` targets or referenced via `subflags = [...]` in `build_flags()` and
    `compiler_build_flags()` targets.
    """,
    implementation = _extra_linker_flags_impl,
    provides = [BuildFlagsInfo],
    attrs = {
        "ldflags": attr.string_list(
            doc = "List of linker flags.",
            default = [],
        ),
        "link_inputs": attr.label_list(
            doc = "List of additional input files required by the linker action (e.g. linker scripts or stub files).",
            allow_files = True,
            default = [],
        ),
        "link_deps": attr.label_list(
            doc = "List of C++ library dependencies (providing CcInfo) to link into all dependent linking targets (executables and shared libraries).",
            providers = [CcInfo],
            default = [],
        ),
        "executable_link_deps": attr.label_list(
            doc = "List of C++ library dependencies (providing CcInfo) to link into dependent executable targets only (ignored for shared libraries).",
            providers = [CcInfo],
            default = [],
        ),
    },
)
