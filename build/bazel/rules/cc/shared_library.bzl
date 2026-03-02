# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("//build/bazel/rules/cc:providers.bzl", "PrebuiltLibraryInfo")

"""Shared library rules for Fuchsia."""

def _generate_link_stubs_for_shared_library_impl(ctx):
    shared_library = ctx.file.shared_library
    ifs_file_name = (
        shared_library.basename.removesuffix(shared_library.extension) + "ifs"
    )

    ifs_output = ctx.actions.declare_file(ifs_file_name)
    link_stub_output = ctx.actions.declare_file("link_stub/" + shared_library.basename)

    args = ctx.actions.args()
    args.add("--write-if-changed")
    args.add("--output-ifs", ifs_output)
    args.add("--output-elf", link_stub_output)
    args.add(shared_library)

    ctx.actions.run(
        inputs = [shared_library],
        outputs = [ifs_output, link_stub_output],
        executable = ctx.executable._llvm_ifs,
        arguments = [args],
        mnemonic = "LlvmIfs",
        # progress_message = "Generating link stubs for %s" % ctx.label.name,
    )

    return [
        DefaultInfo(files = depset([shared_library, ifs_output, link_stub_output])),
        PrebuiltLibraryInfo(
            type = "shared",
            debug = shared_library,
            link_lib = link_stub_output,
            ifs_file = ifs_output,
        ),
    ]

generate_link_stubs_for_shared_library = rule(
    doc = "Generates link stub `.so` and `.ifs` files for a shared library.",
    implementation = _generate_link_stubs_for_shared_library_impl,
    attrs = {
        "shared_library": attr.label(
            doc = "The shared library file for which to create link stubs.",
            allow_single_file = True,
            providers = [CcSharedLibraryInfo],
            mandatory = True,
        ),
        "_llvm_ifs": attr.label(
            default = "@prebuilt_clang//:bin/llvm-ifs",
            executable = True,
            allow_single_file = True,
            cfg = "exec",
        ),
    },
)

def _merge_library_info_for_shared_library_impl(ctx):
    prebuilt_library_info = ctx.attr.link_stub_info[PrebuiltLibraryInfo]

    if prebuilt_library_info.type != "shared":
        fail("`link_stub_info` must be a shared library.")
    if hasattr(prebuilt_library_info, "stripped"):
        fail("`link_stub_info` must not already contain `stripped`.")

    return [
        DefaultInfo(files = depset(transitive = [
            ctx.attr.link_stub_info[DefaultInfo].files,
            ctx.attr.stripped_binary[DefaultInfo].files,
        ])),
        # LINT.IfChange(prebuilt_library_info)
        PrebuiltLibraryInfo(
            type = prebuilt_library_info.type,
            debug = prebuilt_library_info.debug,
            stripped = ctx.attr.stripped_binary[DefaultInfo].files.to_list()[0],
            link_lib = prebuilt_library_info.link_lib,
            ifs_file = prebuilt_library_info.ifs_file,
        ),
        # LINT.ThenChange(//build/bazel/rules/cc/providers.bzl:prebuilt_library_info)
    ]

_merge_library_info_for_shared_library = rule(
    doc = "Returns a fully populated `PrebuiltLibraryInfo` for a shared library.",
    implementation = _merge_library_info_for_shared_library_impl,
    attrs = {
        "link_stub_info": attr.label(
            doc = "A target containing `PrebuiltLibraryInfo` for the shared library and its stubs.",
            providers = [PrebuiltLibraryInfo],
            mandatory = True,
        ),
        "stripped_binary": attr.label(
            doc = "The stripped binary for the shared library.",
            allow_single_file = True,
            mandatory = True,
        ),
    },
)

def _generate_companion_files_for_shared_library_impl(name, shared_library, testonly, visibility):
    link_stubs_target_name = name + ".generate_link_stubs"
    generate_link_stubs_for_shared_library(
        name = link_stubs_target_name,
        shared_library = shared_library,
        testonly = testonly,
    )

    _merge_library_info_for_shared_library(
        name = name,
        link_stub_info = link_stubs_target_name,
        # TODO(https://fxbug.dev/443982549): Use the stripped binary once it is available.
        stripped_binary = shared_library,
        testonly = testonly,
        visibility = visibility,
    )

generate_companion_files_for_shared_library = macro(
    doc = "Generates a link stub, IFS file, and stripped binary for a shared library.",
    implementation = _generate_companion_files_for_shared_library_impl,
    attrs = {
        "shared_library": attr.label(
            doc = "The shared library file for which to create companion files.",
            allow_single_file = True,
            providers = [CcSharedLibraryInfo],
            mandatory = True,
            configurable = False,
        ),
        "testonly": attr.bool(
            doc = "Usual meaning..",
            # mandatory = True,
            configurable = False,
        ),
    },
)

def _get_library_info_for_static_library_impl(ctx):
    static_library = ctx.file.static_library
    return [
        DefaultInfo(files = depset([static_library])),
        PrebuiltLibraryInfo(
            type = "static",
            link_lib = static_library,
        ),
    ]

get_library_info_for_static_library = rule(
    doc = "Returns `PrebuiltLibraryInfo` for a static library." +
          "This is useful when a rule supports passing both static and shared libraries.",
    implementation = _get_library_info_for_static_library_impl,
    attrs = {
        "static_library": attr.label(
            doc = "The static library file for which to create library info.",
            allow_single_file = True,
            # providers = [CcInfo],
            mandatory = True,
        ),
    },
)
