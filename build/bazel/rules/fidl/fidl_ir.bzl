# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for generating FIDL IR."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_build_info//:args.bzl", "runtime_supported_api_levels")
load(":providers.bzl", "FidlLibraryInfo")

visibility("private")

def _gather_dependencies(deps):
    info = []
    libs_added = []
    for dep in deps:
        for lib in dep[FidlLibraryInfo].info:
            name = lib.name
            if name in libs_added:
                continue
            libs_added.append(name)
            info.append(lib)
    return info

def _get_api_levels(ctx):
    current_build_target_api_level = ctx.attr._current_api_level[BuildSettingInfo].value

    if current_build_target_api_level == "PLATFORM":
        # FIDL directly supports targeting multiple API levels. "PLATFORM" is a
        # meta-level that refers to the set of all supported API levels.
        return ",".join(runtime_supported_api_levels)
    else:
        return current_build_target_api_level

# TODO(https://fxbug.dev/428285014): Build and use a response file rather than
# command line arguments.
def _fidlc_impl(ctx):
    library_name = ctx.attr.library_name

    ir = ctx.outputs.json_representation

    info = _gather_dependencies(ctx.attr.deps)
    info.append(struct(
        name = library_name,
        files = ctx.files.srcs,
    ))

    files_argument = []
    inputs = []
    for lib in info:
        files_argument += ["--files"] + [f.path for f in lib.files]
        inputs.extend(lib.files)

    api_level = _get_api_levels(ctx)

    ctx.actions.run(
        executable = ctx.executable._fidlc,
        arguments = [
            "--json",
            ir.path,
            "--name",
            library_name,
            "--available",
            "fuchsia:%s" % api_level,
        ] + files_argument,
        inputs = inputs,
        outputs = [ir],
        mnemonic = "Fidlc",
    )

    return [
        # Passing library info for dependent libraries.
        FidlLibraryInfo(info = info, name = library_name, ir = ir),
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
        "json_dir": attr.string(
            doc = "The sub-directory, if any, containing the `json_representation` file. " +
                  "Other generated files will be written to this directory." +
                  "This is used to prevent multiple FIDL targets from generating the same output files.",
            mandatory = True,
        ),
        "json_representation": attr.output(
            doc = "Where to generate the FIDL IR. Should be in `json_dir`.",
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
            default = "@//tools/fidl/fidlc:fidlc",
            executable = True,
            cfg = "exec",
        ),
        "_current_api_level": attr.label(
            default = "@//build/bazel:fuchsia_api_level",
        ),
    },
)

def _fidl_ir_impl(name, json_representation, out_json_summary, testonly, visibility, **kwargs):
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
        # TODO(https://fxbug.dev/428285014): Generate the JSON summary in
        # `out_json_summary` using `json_representation` as input.
        pass

    native.filegroup(
        name = name,
        srcs = main_target_deps,
        testonly = testonly,
        visibility = visibility,
    )

fidl_ir = macro(
    doc = "Defines a FIDL library that will be compiled to IR.",
    inherit_attrs = fidlc,
    implementation = _fidl_ir_impl,
    attrs = {
        "out_json_summary": attr.output(
            doc = "If set, a JSON API summary file will be generated at the given path. Should be in `json_dir`.",
        ),
    },
)
