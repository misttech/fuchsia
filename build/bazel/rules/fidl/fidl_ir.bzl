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

def _get_api_levels(context):
    current_build_target_api_level = context.attr._current_api_level[BuildSettingInfo].value

    if current_build_target_api_level == "PLATFORM":
        # FIDL directly supports targeting multiple API levels. "PLATFORM" is a
        # meta-level that refers to the set of all supported API levels.
        return ",".join(runtime_supported_api_levels)
    else:
        return current_build_target_api_level

# TODO(https://fxbug.dev/428285014): Build and use a response file rather than
# command line arguments.
def _fidlc_impl(context):
    ir = context.outputs.ir
    library_name = context.attr.library_name

    info = _gather_dependencies(context.attr.deps)
    info.append(struct(
        name = library_name,
        files = context.files.srcs,
    ))

    files_argument = []
    inputs = []
    for lib in info:
        files_argument += ["--files"] + [f.path for f in lib.files]
        inputs.extend(lib.files)

    api_level = _get_api_levels(context)

    context.actions.run(
        executable = context.executable._fidlc,
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
    implementation = _fidlc_impl,
    attrs = {
        "library_name": attr.string(
            doc = "Name of the FIDL library.",
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
    outputs = {
        # The intermediate representation of the library, to be consumed by
        # bindings generators.
        "ir": "%{name}.fidl.json",
    },
)
