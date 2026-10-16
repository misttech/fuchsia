# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Structured configuration client libraries for in-tree components.

These are translations of the templates defined in
//build/components/fuchsia_structured_config.gni and
//tools/configc/build/config.gni. They are kept close to their GN
counterparts so that bazel2gn can convert a Bazel target to an equivalent
call of the GN template.
"""

load("@fuchsia_rules_common//components:providers.bzl", "FuchsiaComponentManifestInfo")
load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

# TODO(wittrock): Build configc with Bazel and use `//tools/configc` instead,
# once it and its dependencies (in particular //src/sys/pkg/lib/fuchsia-pkg)
# are available in Bazel. Until then, configc is built by GN and exposed to
# Bazel through @gn_targets, which requires every bazel_action() that builds
# these targets to depend on "//tools/configc:bazel_input($host_toolchain)".
_CONFIGC = "@gn_targets//toolchain_host_x64/tools/configc:configc"

_CONFIGC_ATTRS = {
    "compiled_manifest": attr.label(
        doc = "The fx_component_manifest() target whose `config` schema is used.",
        mandatory = True,
        providers = [FuchsiaComponentManifestInfo],
    ),
    "fidl_library_name": attr.string(
        doc = "Name of the internal FIDL library that holds the configuration.",
        mandatory = True,
    ),
    "_configc": attr.label(
        default = _CONFIGC,
        executable = True,
        cfg = "exec",
    ),
}

def _fidl_config_client_lib_source_impl(ctx):
    compiled_manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest
    fidl_format = ctx.executable._fidl_format
    output = ctx.actions.declare_file(ctx.label.name + ".fidl")
    ctx.actions.run(
        executable = ctx.executable._configc,
        arguments = [
            "fidl",
            "--cm",
            compiled_manifest.path,
            "--output",
            output.path,
            "--library-name",
            ctx.attr.fidl_library_name,
            "--fidl-format",
            fidl_format.path,
        ],
        inputs = [compiled_manifest],
        tools = [fidl_format],
        outputs = [output],
        mnemonic = "ConfigcFidl",
        progress_message = "Generating structured config FIDL library %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

_fidl_config_client_lib_source = rule(
    doc = "Generates the FIDL source of a structured configuration client library.",
    implementation = _fidl_config_client_lib_source_impl,
    attrs = _CONFIGC_ATTRS | {
        "_fidl_format": attr.label(
            default = "//tools/fidl/fidlc:fidl-format",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _rust_config_client_lib_source_impl(ctx):
    compiled_manifest = ctx.attr.compiled_manifest[FuchsiaComponentManifestInfo].compiled_manifest
    rust_toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    rustfmt = rust_toolchain.rustfmt
    rustfmt_config = ctx.file._rustfmt_config
    output = ctx.actions.declare_file(ctx.label.name + ".rs")

    # The rustfmt config must be passed explicitly so that the output does not
    # depend on where the action runs. See `_fidlgen_rust_impl()` in
    # //build/bazel/rules/fidl/fidl_rust_library.bzl.
    ctx.actions.run(
        executable = ctx.executable._configc,
        arguments = [
            "rust",
            "--cm",
            compiled_manifest.path,
            "--output",
            output.path,
            "--fidl-library-name",
            ctx.attr.fidl_library_name,
            "--rustfmt",
            rustfmt.path,
            "--rustfmt-config",
            rustfmt_config.path,
        ],
        inputs = [compiled_manifest, rustfmt, rustfmt_config],
        tools = rust_toolchain.all_files,
        outputs = [output],
        mnemonic = "ConfigcRust",
        progress_message = "Generating structured config Rust library %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

_rust_config_client_lib_source = rule(
    doc = "Generates the Rust source of a structured configuration client library.",
    implementation = _rust_config_client_lib_source_impl,
    toolchains = ["@rules_rust//rust:toolchain_type"],
    attrs = _CONFIGC_ATTRS | {
        "_rustfmt_config": attr.label(
            default = "@//:rustfmt.toml",
            allow_single_file = True,
        ),
    },
)

def _fuchsia_structured_config_rust_lib_impl(
        name,
        cm_label,
        crate_name,
        testonly,
        visibility):
    # LINT.IfChange(rust_lib_names)
    rust_library_name = (crate_name or name).replace(".", "_").replace("-", "_")

    # The FIDL library name is only used internally by configc when generating
    # the FIDL and Rust sources. It is not visible to users.
    fidl_library_name = "cf.sc.internal." + rust_library_name.replace("_", "")

    fidl_source_target = name + "_fidl_config_lib_source"
    rust_source_target = name + "_rust_config_lib_source"
    fidl_library_target = name + "_fidl_internal"

    # LINT.ThenChange(//build/components/fuchsia_structured_config.gni:rust_lib_names)

    _fidl_config_client_lib_source(
        name = fidl_source_target,
        compiled_manifest = cm_label,
        fidl_library_name = fidl_library_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    _rust_config_client_lib_source(
        name = rust_source_target,
        compiled_manifest = cm_label,
        fidl_library_name = fidl_library_name,
        target_compatible_with = ["@platforms//os:fuchsia"],
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fidl_library(
        name = fidl_library_target,
        srcs = [":" + fidl_source_target],
        library_name = fidl_library_name,
        excluded_checks = [
            "invalid-copyright-for-platform-source-library",
            "wrong-prefix-for-platform-source-library",
        ],
        # Only the Rust bindings are used.
        enable_cpp = False,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    rustc_library(
        name = name,
        crate_name = rust_library_name,
        srcs = [":" + rust_source_target],
        edition = "2024",
        build_flags = ["//build/config/rust/lints:deny_unused_results"],
        deps = [
            ":" + fidl_library_target + "_rust",
            "//sdk/rust/zx",
            "//src/lib/diagnostics/inspect/rust:fuchsia-inspect",
            "//src/lib/fidl/rust/fidl",
            "//src/lib/fuchsia-component/config",
        ],
        testonly = testonly,
        visibility = visibility,
    )

fuchsia_structured_config_rust_lib = macro(
    doc = """Defines a Rust structured configuration client library for a component.

    The library is generated from the `config` schema of a compiled component
    manifest, and exposes the configuration of the component, e.g.:

    ```
    fx_component_manifest(
        name = "manifest",
        component_name = "my_component",
        manifest = "meta/my_component.cml",
    )

    fuchsia_structured_config_rust_lib(
        name = "my_component_config",
        cm_label = ":manifest",
    )

    rustc_binary(
        name = "bin",
        ...
        deps = [":my_component_config", ...],
    )
    ```

    And in Rust:

    ```
    use my_component_config::Config;

    let config = Config::take_from_startup_handle();
    ```
    """,
    implementation = _fuchsia_structured_config_rust_lib_impl,
    attrs = {
        "cm_label": attr.label(
            doc = "The fx_component_manifest() target of the component.",
            mandatory = True,
            configurable = False,
            providers = [FuchsiaComponentManifestInfo],
        ),
        "crate_name": attr.string(
            doc = "Name of the generated Rust crate. Defaults to `name`. `.` and `-` are replaced by `_`.",
            configurable = False,
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)
