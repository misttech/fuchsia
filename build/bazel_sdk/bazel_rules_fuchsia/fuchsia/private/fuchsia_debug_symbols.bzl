# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for extracting, creating, and manipulating debug symbols."""

load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaDebugSymbolInfo",
    "FuchsiaUnstrippedBinaryInfo",
)

def _fuchsia_debug_symbols_impl(ctx):
    return [
        FuchsiaDebugSymbolInfo(build_id_dirs_mapping = {
            ctx.file.source_search_root: depset(transitive = [
                target[DefaultInfo].files
                for target in ctx.attr.build_id_dirs
            ]),
        }),
    ]

fuchsia_debug_symbols = rule(
    doc = """Rule-based constructor for FuchsiaDebugSymbolInfo.""",
    implementation = _fuchsia_debug_symbols_impl,
    attrs = {
        "source_search_root": attr.label(
            doc = "A search root file or directory, used by zxdb to locate source files.",
            mandatory = True,
            allow_single_file = True,
        ),
        "build_id_dirs": attr.label_list(
            doc = "The build_id directories with symbols to be registered.",
            mandatory = True,
            allow_files = True,
        ),
    },
)

def _fuchsia_unstripped_binary_impl(ctx):
    return FuchsiaUnstrippedBinaryInfo(
        dest = ctx.attr.dest,
        unstripped_file = ctx.file.unstripped_file,
        stripped_file = ctx.file.stripped_file if ctx.attr.stripped_file else None,
        source_search_root = ctx.attr.source_search_root,
    )

fuchsia_unstripped_binary = rule(
    doc = "Rule-based constructor for a FuchsiaUnstrippedBinaryInfo value.",
    implementation = _fuchsia_unstripped_binary_impl,
    attrs = {
        "dest": attr.string(
            doc = "Installation location in Fuchsia package for the stripped binary.",
            mandatory = True,
        ),
        "unstripped_file": attr.label(
            doc = "Unstripped ELF binary file",
            mandatory = True,
            allow_single_file = True,
        ),
        "stripped_file": attr.label(
            doc = "Optional stripped ELF binary file, if available as prebuilt.",
            mandatory = False,
            allow_single_file = True,
        ),
        "source_search_root": attr.label(
            doc = "Optional label to source directory or file inside source directory.",
            mandatory = False,
            allow_single_file = True,
        ),
    },
)
