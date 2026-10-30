# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Next-generation Rust bindings and conversion libraries backed by a FIDL library."""

load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")
load("//build/rust:fidl_rust_next.bzl", "fidl_rust_next_allowlist")

visibility("private")

def fidl_rust_next_library(
        *,
        name,
        fidl_library_name,
        fidl_ir_json,
        deps,
        contains_drivers,
        enable_rust,
        testonly,
        visibility):
    """Generates `rustc_library()` targets providing the next-generation Rust bindings for a FIDL library.

    Args:
        name: String base name of the `rustc_library()` targets.
        fidl_library_name: String name of the FIDL library for which bindings are generated.
        fidl_ir_json: `Label` pointing to a single file containing the FIDL IR
            representation of the `fidl_library_name` library.
        deps: List of `Label`s for FIDL libraries that the `fidl_library_name` library depends on.
        contains_drivers: Boolean indicating whether the FIDL library contains driver transport
            protocols or references.
        enable_rust: Boolean indicating whether standard Rust bindings are enabled for this library.
        testonly: Usual meaning.
        visibility: Usual meaning.
    """
    rust_next_visibility = _get_rust_next_visibility(visibility)

    _fidl_rust_next_library_flavor(
        "common",
        name,
        fidl_library_name,
        fidl_ir_json,
        deps,
        contains_drivers,
        testonly,
        rust_next_visibility,
    )
    _fidl_rust_next_library_flavor(
        "fidl",
        name,
        fidl_library_name,
        fidl_ir_json,
        deps,
        contains_drivers,
        testonly,
        rust_next_visibility,
    )
    _fidl_rust_next_library_flavor(
        "fdomain",
        name,
        fidl_library_name,
        fidl_ir_json,
        deps,
        False,
        testonly,
        rust_next_visibility,
    )

    if enable_rust:
        _fidl_rust_next_library_convert(
            name,
            fidl_library_name,
            fidl_ir_json,
            testonly,
            visibility,
        )

def _get_rust_next_visibility(visibility):
    # Allow the FIDL symbolic macro package to depend on `_rust_next` targets
    # when generating bindings for dependent FIDL libraries or `_rust_next_convert`.
    macro_visibility = [Label("//build/bazel/rules/fidl:__subpackages__")]

    if Label("//visibility:public") in visibility:
        return [Label(a) for a in fidl_rust_next_allowlist] + macro_visibility

    allowlist_labels = [Label(a) for a in fidl_rust_next_allowlist]
    result = list(macro_visibility)
    for v in visibility:
        if v == Label("//visibility:private"):
            continue
        for a in allowlist_labels:
            if v.package == a.package or v.package.startswith(a.package + "/"):
                if v not in result:
                    result.append(v)
                break
    for a in allowlist_labels:
        for v in visibility:
            if v.name == "__subpackages__" and (a.package == v.package or a.package.startswith(v.package + "/")):
                if a not in result:
                    result.append(a)
                break
    return result

def _fidl_rust_next_flavor_crate_name(fidl_library_name, flavor):
    base = fidl_library_name.replace(".", "_")
    if flavor == "fidl":
        return "fidl_next_%s" % base
    elif flavor == "common":
        return "fidl_next_common_%s" % base
    elif flavor == "fdomain":
        return "fdomain_next_%s" % base
    else:
        fail("Unknown flavor: %s" % flavor)

def _fidl_rust_next_flavor_label_suffix(flavor):
    if flavor == "fidl":
        return "rust_next"
    elif flavor == "common":
        return "rust_next_common"
    elif flavor == "fdomain":
        return "rust_fdomain_next"
    else:
        fail("Unknown flavor: %s" % flavor)

def _fidl_rust_next_flavor_config(flavor):
    if flavor == "fidl":
        return "//tools/fidl/fidlgen_rust_next:configs/fuchsia.json"
    elif flavor == "common":
        return "//tools/fidl/fidlgen_rust_next:configs/common.json"
    elif flavor == "fdomain":
        return "//tools/fidl/fidlgen_rust_next:configs/fdomain.json"
    else:
        fail("Unknown flavor: %s" % flavor)

def _fidl_rust_next_flavor_label_name(label_name, flavor):
    return label_name + "_" + _fidl_rust_next_flavor_label_suffix(flavor)

def _fidl_rust_next_flavor_label(label, flavor):
    return "//%s:%s" % (label.package, _fidl_rust_next_flavor_label_name(label.name, flavor))

def _fidl_rust_next_library_flavor(
        flavor,
        name,
        fidl_library_name,
        fidl_ir_json,
        deps,
        contains_drivers,
        testonly,
        visibility):
    flavor_label = _fidl_rust_next_flavor_label_name(name, flavor)
    fidlgen_label = flavor_label + "_fidlgen"

    if flavor == "common":
        common_lib = ""
    else:
        common_lib = _fidl_rust_next_flavor_crate_name(fidl_library_name, "common")

    _fidlgen_rust_next(
        name = fidlgen_label,
        fidl_ir_json = fidl_ir_json,
        out = "bindings/rust_next/%s.rs" % _fidl_rust_next_flavor_crate_name(name, flavor),
        config = _fidl_rust_next_flavor_config(flavor),
        common_lib = common_lib,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    library_deps = [
        "//src/lib/fidl/rust_next/fidl_next",
        "//third_party/rust_crates/vendor:static_assertions",
    ]

    if flavor == "fdomain":
        library_deps.append("//src/lib/fdomain/client")

    has_zx_dep = False
    for dep in deps:
        if Label(dep) == Label("//zircon/vdso/zx"):
            has_zx_dep = True
            continue

        library_deps.append(_fidl_rust_next_flavor_label(dep, flavor))

    if has_zx_dep:
        library_deps.append("//sdk/rust/zx-types")

    if flavor != "common":
        library_deps.append(":" + _fidl_rust_next_flavor_label_name(name, "common"))

    if contains_drivers:
        library_deps += select({
            "@platforms//os:fuchsia": ["//sdk/lib/driver/runtime/rust/fidl"],
            "//conditions:default": [],
        })
        crate_features = select({
            "@platforms//os:fuchsia": [
                "fuchsia",
                "driver",
            ],
            "//conditions:default": [],
        })
    else:
        crate_features = select({
            "@platforms//os:fuchsia": ["fuchsia"],
            "//conditions:default": [],
        })

    rustc_library(
        name = flavor_label,
        crate_name = _fidl_rust_next_flavor_crate_name(fidl_library_name, flavor),
        srcs = [fidlgen_label],
        deps = library_deps,
        crate_features = crate_features,
        edition = "2024",
        version = "0.1.0",
        disable_clippy = True,
        build_flags = [
            # TODO(https://fxbug.dev/42055130): Avoid this suppression. Keep in
            # sync with //build/rust/fidl_rust_next.gni.
            "//build/config/rust/lints:allow_unused_crate_dependencies",
            "//build/config/rust/lints:deny_unused_results",
        ],
        testonly = testonly,
        visibility = visibility,
    )

def _fidl_rust_next_library_convert(
        name,
        fidl_library_name,
        fidl_ir_json,
        testonly,
        visibility):
    convert_label = name + "_rust_next_convert"
    fidlgen_label = convert_label + "_fidlgen"

    base_library_name = fidl_library_name.replace(".", "_")
    crate_name = "fidl_next_convert_%s" % base_library_name
    rust_crate = "fidl_%s" % base_library_name
    rust_next_crate = "fidl_next_%s" % base_library_name

    _fidlgen_rust_next_convert(
        name = fidlgen_label,
        fidl_ir_json = fidl_ir_json,
        out = "bindings/rust_next/fidl_next_convert_%s.rs" % name.replace(".", "_"),
        rust_crate = rust_crate,
        rust_next_crate = rust_next_crate,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    rustc_library(
        name = convert_label,
        crate_name = crate_name,
        srcs = [fidlgen_label],
        deps = [
            ":%s_rust" % name,
            ":%s_rust_next" % name,
        ],
        crate_features = select({
            "@platforms//os:fuchsia": ["fuchsia"],
            "//conditions:default": [],
        }),
        edition = "2024",
        version = "0.1.0",
        disable_clippy = True,
        build_flags = [
            "//build/config/rust/lints:deny_unused_results",
        ],
        testonly = testonly,
        visibility = visibility,
    )

def _fidlgen_rust_next_impl(ctx):
    ir = ctx.file.fidl_ir_json
    config = ctx.file.config
    rustfmt_config = ctx.file._rustfmt_config

    rust_toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    rustfmt = rust_toolchain.rustfmt

    arguments = [
        "--json",
        ir.path,
        "--output-filename",
        ctx.outputs.out.path,
        "--rustfmt",
        rustfmt.path,
        "--rustfmt-config",
        rustfmt_config.path,
        "--config",
        config.path,
    ]

    if ctx.attr.common_lib != "":
        arguments.extend([
            "--common-lib",
            ctx.attr.common_lib,
        ])

    ctx.actions.run(
        executable = ctx.executable._fidlgen_tool,
        arguments = arguments,
        inputs = [ir, config, rustfmt, rustfmt_config],
        tools = rust_toolchain.all_files,
        outputs = [ctx.outputs.out],
        mnemonic = "FidlGenRustNext",
    )

    return [
        DefaultInfo(files = depset([ctx.outputs.out])),
    ]

_fidlgen_rust_next = rule(
    implementation = _fidlgen_rust_next_impl,
    toolchains = ["@rules_rust//rust:toolchain_type"],
    attrs = {
        "fidl_ir_json": attr.label(
            doc = "The FIDL IR for which to generate code.",
            allow_single_file = True,
            mandatory = True,
        ),
        "config": attr.label(
            doc = "The JSON configuration file for fidlgen_rust_next.",
            allow_single_file = True,
            mandatory = True,
        ),
        "common_lib": attr.string(
            doc = "If not empty, use the given crate name for the common (POD) data structures.",
            default = "",
        ),
        "_fidlgen_tool": attr.label(
            doc = "fidlgen_rust_next tool.",
            executable = True,
            cfg = "exec",
            default = "@//tools/fidl/fidlgen_rust_next",
        ),
        "_rustfmt_config": attr.label(
            doc = "rustfmt configuration used to format the generated code.",
            allow_single_file = True,
            default = "@//:rustfmt.toml",
        ),
        "out": attr.output(
            doc = "Output filename.",
            mandatory = True,
        ),
    },
)

def _fidlgen_rust_next_convert_impl(ctx):
    ir = ctx.file.fidl_ir_json
    rustfmt_config = ctx.file._rustfmt_config

    rust_toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    rustfmt = rust_toolchain.rustfmt

    arguments = [
        "--json",
        ir.path,
        "--output-filename",
        ctx.outputs.out.path,
        "--rustfmt",
        rustfmt.path,
        "--rustfmt-config",
        rustfmt_config.path,
        "--rust-crate",
        ctx.attr.rust_crate,
        "--rust-next-crate",
        ctx.attr.rust_next_crate,
    ]

    ctx.actions.run(
        executable = ctx.executable._fidlgen_tool,
        arguments = arguments,
        inputs = [ir, rustfmt, rustfmt_config],
        tools = rust_toolchain.all_files,
        outputs = [ctx.outputs.out],
        mnemonic = "FidlGenRustNextConvert",
    )

    return [
        DefaultInfo(files = depset([ctx.outputs.out])),
    ]

_fidlgen_rust_next_convert = rule(
    implementation = _fidlgen_rust_next_convert_impl,
    toolchains = ["@rules_rust//rust:toolchain_type"],
    attrs = {
        "fidl_ir_json": attr.label(
            doc = "The FIDL IR for which to generate conversion code.",
            allow_single_file = True,
            mandatory = True,
        ),
        "rust_crate": attr.string(
            doc = "Crate name of the standard Rust bindings.",
            mandatory = True,
        ),
        "rust_next_crate": attr.string(
            doc = "Crate name of the next-generation Rust bindings.",
            mandatory = True,
        ),
        "_fidlgen_tool": attr.label(
            doc = "fidlgen_rust_next_convert tool.",
            executable = True,
            cfg = "exec",
            default = "@//tools/fidl/fidlgen_rust_next/convert:fidlgen_rust_next_convert",
        ),
        "_rustfmt_config": attr.label(
            doc = "rustfmt configuration used to format the generated code.",
            allow_single_file = True,
            default = "@//:rustfmt.toml",
        ),
        "out": attr.output(
            doc = "Output filename.",
            mandatory = True,
        ),
    },
)
