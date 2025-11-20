# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for generating FIDL IR."""

load(":fidl_summary.bzl", "fidl_summary")
load(":providers.bzl", "FidlLibraryInfo")

visibility("private")

def _fidlc_impl(ctx):
    library_name = ctx.attr.library_name

    response_file = ctx.actions.declare_file(ctx.attr.fidl_library_target_name + ".args")
    libraries_file = ctx.actions.declare_file(ctx.attr.fidl_library_target_name + ".libraries")

    dep_libraries = []
    for dep in ctx.attr.deps:
        dep_libraries.append(dep[FidlLibraryInfo].libraries_file)

    response_file_args = ctx.actions.args()
    response_file_args.add_all([
        "--out-response-file",
        response_file.path,
        "--out-libraries",
        libraries_file.path,
        "--json",
        ctx.outputs.json_representation.path,
        "--name",
        library_name,
    ])
    response_file_args.add_all("--sources", ctx.files.srcs)

    if dep_libraries:
        response_file_args.add("--dep-libraries", dep_libraries)

    if ctx.attr.versioned:
        response_file_args.add("--versioned", ctx.attr.versioned)

    for available in ctx.attr.available:
        response_file_args.add("--available", available)

    for flag in ctx.attr.experimental_flags:
        response_file_args.add("--experimental", flag)

    ctx.actions.run(
        executable = ctx.executable._gen_response_file_script,
        arguments = [response_file_args],
        inputs = ctx.files.srcs + dep_libraries,
        outputs = [response_file, libraries_file],
        mnemonic = "GenFidlResponseFile",
    )

    ctx.actions.run(
        executable = ctx.executable._fidlc,
        arguments = ["@" + response_file.path],
        inputs = [response_file] + ctx.files.srcs,
        outputs = [ctx.outputs.json_representation],
        mnemonic = "Fidlc",
    )

    return [
        FidlLibraryInfo(
            name = library_name,
            ir = ctx.outputs.json_representation,
            libraries_file = libraries_file,
        ),
    ]

fidlc = rule(
    doc = "Runs the FIDL compiler to generate the FIDL IR.",
    implementation = _fidlc_impl,
    attrs = {
        "library_name": attr.string(
            doc = "Name of the FIDL library.",
            mandatory = True,
        ),
        "fidl_library_target_name": attr.string(
            doc = "Name of the `fidl_library()` target. Used in the name of some generated files.",
            mandatory = True,
        ),
        "srcs": attr.label_list(
            doc = "List of `.fidl` source files.",
            mandatory = True,
            allow_files = True,
            allow_empty = False,
        ),
        "deps": attr.label_list(
            doc = "List of labels of other FIDL libraries on which this library depends.",
            mandatory = False,
            providers = [FidlLibraryInfo],
        ),
        "gen_dir": attr.string(
            doc = "The directory into which intermediate output files should be written. " +
                  "This is used to prevent multiple FIDL targets from generating the same output files.",
            mandatory = True,
        ),
        "json_representation": attr.output(
            doc = "Where to generate the FIDL IR.",
            mandatory = True,
        ),
        "available": attr.string_list(
            doc = "See `fidl_library()`.",
            mandatory = True,
        ),
        "versioned": attr.string(
            doc = "See `fidl_library()`.",
        ),
        "experimental_flags": attr.string_list(
            doc = "A list of experimental fidlc features to enable.",
        ),
        "_fidlc": attr.label(
            doc = "The FIDL compiler.",
            default = "@//tools/fidl/fidlc:fidlc_tool",
            executable = True,
            cfg = "exec",
        ),
        "_gen_response_file_script": attr.label(
            default = "//build/fidl:gen_response_file",
            executable = True,
            cfg = "exec",
        ),
    },
)

def fidl_ir(name, json_representation, out_json_summary, testonly, visibility, **kwargs):
    """Defines a FIDL library that will be compiled to IR.

    Args:
      name: Standard meaning.
      json_representation: Where to generate the FIDL IR.
      out_json_summary: If set, a JSON API summary file will be generated at the given path. Should be in `gen_dir`.
      testonly: Standard meaning.
      visibility: Standard meaning.

      **kwargs: Arguments to pass to the underlying `fidlc` rule.
    """
    fidlc_target_name = "%s_fidlc" % name
    main_target_deps = [fidlc_target_name]

    fidlc(
        name = fidlc_target_name,
        json_representation = json_representation,
        testonly = testonly,
        visibility = ["//visibility:private"],
        **kwargs
    )

    if out_json_summary:
        fidl_summary_json_target_name = "%s_summary_json" % name
        fidl_summary(
            name = fidl_summary_json_target_name,
            input = json_representation,
            output = out_json_summary,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        main_target_deps.append(fidl_summary_json_target_name)

    native.filegroup(
        name = name,
        srcs = main_target_deps,
        testonly = testonly,
        visibility = visibility,
    )
