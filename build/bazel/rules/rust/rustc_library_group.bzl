# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""rustc_library_group(): Rust crates plus native libraries to link, as one dep."""

load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust:rust_common.bzl", "BuildInfo", "rust_common")

def _rustc_library_group_impl(ctx):
    variants = []
    transitive = []
    cc_infos = []
    runfiles = []
    for dep in ctx.attr.deps:
        if rust_common.crate_info in dep:
            variants.append(rust_common.dep_variant_info(
                crate_info = dep[rust_common.crate_info],
                dep_info = dep[rust_common.dep_info],
                build_info = dep[BuildInfo] if BuildInfo in dep else None,
                cc_info = dep[CcInfo] if CcInfo in dep else None,
                crate_group_info = None,
            ))
        else:
            transitive.append(dep[rust_common.crate_group_info].dep_variant_infos)
        if CcInfo in dep:
            cc_infos.append(dep[CcInfo])
        if dep[DefaultInfo].default_runfiles != None:
            runfiles.append(dep[DefaultInfo].default_runfiles)

    # Native libraries are linked into the final binary but, unlike crates,
    # are not passed to rustc with --extern.
    for dep in ctx.attr.link_deps:
        variants.append(rust_common.dep_variant_info(
            crate_info = None,
            dep_info = None,
            build_info = None,
            cc_info = dep[CcInfo],
            crate_group_info = None,
        ))
        cc_infos.append(dep[CcInfo])
        if dep[DefaultInfo].default_runfiles != None:
            runfiles.append(dep[DefaultInfo].default_runfiles)

    merged_cc_info = cc_common.merge_cc_infos(cc_infos = cc_infos)

    # cc_import() exposes shared libraries only through CcInfo, not runfiles.
    # Add them, so targets that only look at runfiles (e.g. rust_test(crate =
    # ...), which ignores the crate's deps) still get them at runtime.
    dynamic_libs = [
        lib.dynamic_library
        for li in merged_cc_info.linking_context.linker_inputs.to_list()
        for lib in li.libraries
        if lib.dynamic_library
    ]

    return [
        rust_common.crate_group_info(
            dep_variant_infos = depset(variants, transitive = transitive),
        ),
        # Lets dependents (and the binaries and packages built from them) see
        # the native libraries, e.g. to package shared libraries. rules_rust
        # uses CrateGroupInfo before CcInfo, so nothing is linked twice.
        merged_cc_info,
        DefaultInfo(runfiles = ctx.runfiles(files = dynamic_libs).merge_all(runfiles)),
        coverage_common.instrumented_files_info(
            ctx,
            dependency_attributes = ["deps", "link_deps"],
        ),
    ]

rustc_library_group = rule(
    implementation = _rustc_library_group_impl,
    doc = """Groups Rust crates with native libraries to link.

    Like rules_rust's rust_library_group(), which only accepts crates, plus
    `link_deps`. This is the Bazel counterpart of a GN group() whose
    public_deps are a Rust crate and the C/C++ library it should link (e.g.
    one of several link flavors). Dependents list it in `deps` like a crate.
    """,
    attrs = {
        "deps": attr.label_list(
            doc = "Rust crates (or crate groups) to forward.",
            providers = [[rust_common.crate_info], [rust_common.crate_group_info]],
        ),
        "link_deps": attr.label_list(
            doc = "C/C++ libraries to link into dependents' final binaries.",
            providers = [CcInfo],
        ),
    },
    provides = [rust_common.crate_group_info, CcInfo],
)
