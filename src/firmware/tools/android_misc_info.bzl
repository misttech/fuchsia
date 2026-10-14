# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for processing Android misc_info.txt files."""

load("//build/bazel/rules/python:py_toolchain.bzl", "PY_TOOLCHAIN_ATTRS", "generate_python_build_action")

visibility("//vendor/...")

AndroidMiscInfo = provider(
    doc = """Provider containing the output artifact generated from an
             Android misc_info.txt file.""",
    fields = {
        "file": "The processed misc_info output file (txt or json)",
    },
)

def _android_misc_info_impl(ctx):
    ext = ".json" if ctx.attr.format == "json" else ".txt"
    out = ctx.actions.declare_file(ctx.label.name + ext)

    args = [
        ctx.file.misc_info.path,
        "-o",
        out.path,
        "--format",
        ctx.attr.format,
    ]
    for prop in ctx.attr.props:
        args += ["--prop", prop]

    generate_python_build_action(
        ctx = ctx,
        py_script = ctx.file._script,
        arguments = args,
        outputs = [out],
        inputs = [ctx.file.misc_info],
        mnemonic = "AndroidMiscInfo",
    )

    return [
        DefaultInfo(files = depset([out])),
        AndroidMiscInfo(file = out),
    ]

android_misc_info = rule(
    doc = """Extracts vbmeta properties from an Android misc_info.txt file.""",
    implementation = _android_misc_info_impl,
    provides = [AndroidMiscInfo, DefaultInfo],
    attrs = {
        "misc_info": attr.label(
            doc = "The misc_info.txt file to parse",
            mandatory = True,
            allow_single_file = True,
        ),
        "props": attr.string_list(
            doc = """Optional list of properties to extract. If omitted, all
                     properties are extracted.

                     Any listed properties may use NAME:NEW_NAME syntax to
                     select NAME but rename it to NEW_NAME in the
                     output.""",
            default = [],
        ),
        "format": attr.string(
            doc = "Output format ('raw_value' or 'json')",
            default = "raw_value",
            values = ["raw_value", "json"],
        ),
        "_script": attr.label(
            default = "//src/firmware/tools:android_misc_info.py",
            allow_single_file = True,
        ),
    } | PY_TOOLCHAIN_ATTRS,
)
