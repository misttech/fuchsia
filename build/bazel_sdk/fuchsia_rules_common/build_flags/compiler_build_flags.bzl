# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""compiler_build_flags() is the Bazel equivalent to the GN compiler_config() template."""

load(":build_flags.bzl", "BUILD_FLAGS_ATTRS", "create_build_flags_info")
load(":providers.bzl", "BuildFlagsInfo")

def _compiler_build_flags_impl(ctx):
    extra_cflags = list(ctx.attr.c_family_flags)
    extra_ldflags = list(ctx.attr.c_family_flags)
    extra_rustflags = []

    for arg in ctx.attr.llvm_flags:
        extra_cflags.extend(["-mllvm", arg])

    for arg in ctx.attr.llvm_flags + ctx.attr.rust_llvm_flags:
        extra_rustflags.append("-Cllvm-args={}".format(arg))

    _linker_flags = list(ctx.attr.linker_flags) + [
        "--mllvm={}".format(arg)
        for arg in ctx.attr.linker_llvm_flags
    ]
    _rust_linker_flags = (
        list(ctx.attr.linker_flags) +
        list(ctx.attr.rust_linker_flags) +
        [
            "--mllvm={}".format(arg)
            for arg in ctx.attr.linker_llvm_flags + ctx.attr.rust_linker_llvm_flags
        ]
    )

    if _linker_flags:
        extra_ldflags.append("-Wl," + ",".join(_linker_flags))

    if _rust_linker_flags:
        # Bazel invocations always require the -Wl,
        # prefix, as the Rust toolchain configuration
        # differs from the GN one.
        extra_rustflags.append(
            "-Clink-arg=-Wl," + ",".join(_rust_linker_flags),
        )

    for feature in ctx.attr.rust_target_features:
        extra_rustflags.append("-Ctarget-feature={}".format(feature))

    return [
        create_build_flags_info(
            ctx,
            extra_cflags = extra_cflags,
            extra_ldflags = extra_ldflags,
            extra_rustflags = extra_rustflags,
            extra_subflags = ctx.attr.compiler_subflags,
        ),
    ]

compiler_build_flags = rule(
    doc = """A Bazel equivalent to the GN compiler_config() template.

    Note that rust_target_edits is not supported as it should never
    be set in Bazel (only for GN Rust kernel targets).
    """,
    implementation = _compiler_build_flags_impl,
    provides = [BuildFlagsInfo],
    attrs = BUILD_FLAGS_ATTRS | {
        "compiler_subflags": attr.label_list(
            doc = "A list of compiler_build_flags() dependencies. Just like subflags for build_flags(), their flags will be injected directly into the current instance.",
            providers = [BuildFlagsInfo],
            default = [],
        ),
        "c_family_flags": attr.string_list(
            doc = "Flags added to cflags and ldflags. (Note that asmflags is not supported by Bazel build_flags()).",
            default = [],
        ),
        "llvm_flags": attr.string_list(
            doc = "LLVM specific flags, each one will be added as `-mllvm <arg>` to cflags, and as `-Cllvm-args=<arg>` to rustflags.",
            default = [],
        ),
        "rust_llvm_flags": attr.string_list(
            doc = "Rust-only LLVM specific flags, each one will be added as `-Cllvm-args=<arg>` to rustflags.",
            default = [],
        ),
        "linker_flags": attr.string_list(
            doc = "These flags are added to both `ldflags` and `rustflags`, with a `-Clink-arg=-Wl,` prefix for Rust.",
            default = [],
        ),
        "rust_linker_flags": attr.string_list(
            doc = "Rust-specific linker flags. Also added with an implicit `-Clink-arg=-Wl,` prefix.",
            default = [],
        ),
        "linker_llvm_flags": attr.string_list(
            doc = "LLVM specific linker flags, added as `--mllvm=<arg>` to linker_flags and rust_linker_flags.",
            default = [],
        ),
        "rust_linker_llvm_flags": attr.string_list(
            doc = "Rust-only LLVM specific linker flags, added as `--mllvm=<arg>` to rust_linker_flags.",
            default = [],
        ),
        "rust_target_features": attr.string_list(
            doc = "Each item is added as `-Ctarget-feature=<feature>` to rustflags.",
            default = [],
        ),
    },
)
