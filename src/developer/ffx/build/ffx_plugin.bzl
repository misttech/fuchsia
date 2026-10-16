# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines the ffx_plugin macro for building ffx plugins."""

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")
load("//build/tools/json_merge:json_merge.bzl", "json_merge")

def _ffx_generate_cmd_impl(ctx):
    out_file = ctx.actions.declare_file(ctx.attr.output_name)
    args = ctx.actions.args()
    args.add("--out", out_file.path)
    args.add("--deps", ",".join(ctx.attr.crate_names))

    ctx.actions.run(
        outputs = [out_file],
        inputs = [],
        executable = ctx.executable._tool,
        arguments = [args],
        mnemonic = "FfxGenCmd",
        progress_message = "Generating command args for %s" % ctx.label,
    )
    return [DefaultInfo(files = depset([out_file]))]

_ffx_generate_cmd = rule(
    implementation = _ffx_generate_cmd_impl,
    attrs = {
        "output_name": attr.string(mandatory = True),
        "crate_names": attr.string_list(mandatory = True),
        "_tool": attr.label(
            executable = True,
            cfg = "exec",
            default = "//src/developer/ffx/build:gn_generate_cmd",
        ),
    },
)

def _ffx_generate_plugins_impl(ctx):
    out_file = ctx.actions.declare_file(ctx.attr.output_name)
    args = ctx.actions.args()
    args.add("--out", out_file.path)
    args.add("--args", ctx.attr.args_lib)
    args.add("--execution_lib", ctx.attr.execution_lib)
    if ctx.attr.includes_execution:
        args.add("--includes_execution")
    if ctx.attr.includes_subcommands:
        args.add("--includes_subcommands")
    if ctx.attr.plugin_crate_names:
        args.add("--deps", ",".join(ctx.attr.plugin_crate_names))
    if ctx.attr.sub_command_lib:
        args.add("--sub_command", ctx.attr.sub_command_lib)

    ctx.actions.run(
        outputs = [out_file],
        inputs = [],
        executable = ctx.executable._tool,
        arguments = [args],
        mnemonic = "FfxGenPlugins",
        progress_message = "Generating plugins registry for %s" % ctx.label,
    )
    return [DefaultInfo(files = depset([out_file]))]

_ffx_generate_plugins = rule(
    implementation = _ffx_generate_plugins_impl,
    attrs = {
        "output_name": attr.string(mandatory = True),
        "args_lib": attr.string(mandatory = True),
        "execution_lib": attr.string(mandatory = True),
        "includes_execution": attr.bool(default = False),
        "includes_subcommands": attr.bool(default = False),
        "plugin_crate_names": attr.string_list(),
        "sub_command_lib": attr.string(),
        "_tool": attr.label(
            executable = True,
            cfg = "exec",
            default = "//src/developer/ffx/build:gn_generate_plugins",
        ),
    },
)

def _ffx_plugin_impl(
        name,
        args_srcs,
        args_deps,
        args_crate_root,
        srcs,
        deps,
        proc_macro_deps,
        link_deps,
        compile_data,
        data,
        crate_features,
        lint_config,
        disable_clippy,
        plugin_deps,
        config_data,
        edition,
        sdk_category,
        version,
        visibility):
    _crate_name = name.replace("-", "_")

    if sdk_category and sdk_category not in ["partner", "not-yet-specified"]:
        fail("Unexpected SDK category '{}'.".format(sdk_category))

    # TODO(https://fxbug.dev/454397833): Support SDK category and compatibility enforcements.
    # GN version has complex logic for sdk_category and category_marker.

    includes_execution = bool(srcs)

    # Subcommand library
    if plugin_deps:
        _ffx_generate_cmd(
            name = name + "_sub_command_gen",
            output_name = name + "_cmd_args.rs",
            crate_names = [Label(d).name.replace("-", "_") for d in plugin_deps],
        )

        rustc_library(
            name = name + "_sub_command",
            srcs = [":" + name + "_sub_command_gen"],
            crate_name = _crate_name + "_sub_command",
            edition = edition,
            deps = [
                "//third_party/rust_crates/vendor:argh",
            ] + [str(d) + "_args" for d in plugin_deps],
            lint_config = lint_config,
            disable_clippy = disable_clippy,
            target_compatible_with = HOST_CONSTRAINTS,
            version = version,
            visibility = visibility,
        )

    # Configuration merging
    config_srcs = list(config_data)
    if not config_srcs:
        native.genrule(
            name = name + "_empty_config_json",
            outs = [name + "_empty_config.json"],
            cmd = "echo '{}' > $@",
        )
        config_srcs = [":" + name + "_empty_config_json"]
    for dep in plugin_deps:
        config_target_name = Label(dep).name + "_config"
        config_srcs.append(Label(dep).same_package_label(config_target_name))

    json_merge(
        name = name + "_config",
        srcs = config_srcs,
        minify = True,
        visibility = visibility,
    )

    # Args library
    rustc_library(
        name = name + "_args",
        srcs = args_srcs,
        crate_name = _crate_name + "_args",
        crate_root = args_crate_root or native.package_relative_label("src/args.rs"),
        crate_features = crate_features,
        edition = edition,
        deps = args_deps + ([":" + name + "_sub_command"] if plugin_deps else []),
        lint_config = lint_config,
        disable_clippy = disable_clippy,
        visibility = visibility,
        version = version,
        target_compatible_with = HOST_CONSTRAINTS,
    )

    # Execution library
    if includes_execution:
        rustc_library(
            name = name,
            srcs = srcs,
            crate_name = _crate_name,
            crate_features = crate_features,
            edition = edition,
            deps = deps + [":" + name + "_args"],
            proc_macro_deps = proc_macro_deps,
            link_deps = link_deps,
            compile_data = compile_data,
            data = data,
            lint_config = lint_config,
            disable_clippy = disable_clippy,
            target_compatible_with = HOST_CONSTRAINTS,
            version = version,
            visibility = visibility,
        )

    # Plugins generation
    _ffx_generate_plugins(
        name = name + "_plugins_gen",
        args_lib = _crate_name + "_args",
        execution_lib = _crate_name,
        includes_execution = includes_execution,
        includes_subcommands = bool(plugin_deps),
        plugin_crate_names = [Label(d).name.replace("-", "_") for d in plugin_deps],
        sub_command_lib = _crate_name + "_sub_command" if plugin_deps else None,
        output_name = name + "_plugins.rs",
    )

    # Suite library
    suite_deps = [
        ":" + name + "_args",
        "//src/developer/ffx/lib/fho:lib",
    ]
    if includes_execution:
        suite_deps.append(":" + name)
    if plugin_deps:
        suite_deps.append(":" + name + "_sub_command")
        for dep in plugin_deps:
            suite_deps.append(str(dep) + "_suite")

    rustc_library(
        name = name + "_suite",
        srcs = [":" + name + "_plugins_gen"],
        crate_name = _crate_name + "_suite",
        edition = edition,
        deps = suite_deps,
        data = [":" + name + "_plugins_gen"],
        lint_config = lint_config,
        disable_clippy = disable_clippy,
        version = version,
        visibility = visibility,
        target_compatible_with = HOST_CONSTRAINTS,
    )

ffx_plugin = macro(
    doc = """Defines an FFX plugin.

This macro generates the necessary targets for an FFX plugin.

Public subtargets created and exposed:

  - `{name}_suite` (Always): Registers the plugin's args, execution library (if any) and all
    `plugin_deps` subcommands. This is what ffx binaries depend on.
  - `{name}_args` (Always): Compiles the CLI arguments parser (usually `src/args.rs`).
  - `{name}_config` (Always): Merges configuration data from `config_data` and all `plugin_deps`.
  - `{name}_sub_command` (When `plugin_deps` is non-empty): Compiles the subcommand enum
    aggregating all `plugin_deps` argument parsers.
  - `{name}` (When `srcs` is non-empty): The plugin execution library.

Other internal targets (code generators for `{name}_sub_command` and `{name}_suite`) are
implementation details and should not be depended on.

Unit tests for `:{name}` and `:{name}_args` are defined via standalone
`host_rustc_test(crate = ":{name}")` and `host_rustc_test(crate = ":{name}_args")` targets
and registered in `//src/developer/ffx:tests`.
""",
    implementation = _ffx_plugin_impl,
    attrs = {
        "args_srcs": attr.label_list(
            doc = "List of source files for the args library.",
            mandatory = True,
            allow_files = True,
        ),
        "args_deps": attr.label_list(
            doc = "List of targets on which the args library depends.",
            default = [],
        ),
        "args_crate_root": attr.label(
            doc = "Root source file of the args library. Defaults to `src/args.rs`.",
            allow_single_file = [".rs"],
            default = None,
            configurable = False,
        ),
        "srcs": attr.label_list(
            doc = """List of source files for the plugin execution library.

If empty, the execution library is not created.""",
            default = [],
            allow_files = True,
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "List of targets on which the plugin library depends.",
            default = [],
        ),
        "proc_macro_deps": attr.label_list(
            doc = "List of proc macro targets on which the plugin library depends.",
            default = [],
        ),
        "link_deps": attr.label_list(
            doc = "List of non-Rust targets linked into the plugin library.",
            default = [],
        ),
        "compile_data": attr.label_list(
            doc = "List of files used by the plugin library at compile time.",
            default = [],
            allow_files = True,
        ),
        "data": attr.label_list(
            doc = "List of files used by the plugin library at runtime.",
            default = [],
            allow_files = True,
        ),
        "crate_features": attr.string_list(
            doc = "List of crate features enabled for the plugin and args libraries.",
            default = [],
        ),
        "lint_config": attr.label(
            doc = "Lint configuration applied to all Rust libraries of the plugin.",
            default = None,
        ),
        "disable_clippy": attr.bool(
            doc = "If true, disables clippy lints on all Rust libraries of the plugin.",
            default = False,
            configurable = False,
        ),
        "plugin_deps": attr.label_list(
            doc = """List of ffx_plugin() targets to include as subcommands.

Each label must name the base target of an ffx_plugin() (e.g. `//path/to:ffx_foo`), whose
`_args`, `_suite` and `_config` subtargets are depended on.""",
            default = [],
            configurable = False,
        ),
        "config_data": attr.label_list(
            doc = "List of JSON files containing configuration data.",
            default = [],
            allow_files = True,
            configurable = False,
        ),
        "edition": attr.string(
            doc = "Edition of the Rust language to be used.",
            default = "2024",
        ),
        "sdk_category": attr.string(
            doc = """SDK category for the plugin.

Must be unset, "partner", or "not-yet-specified". Only the value is validated; SDK
category dependency enforcement is not yet implemented (https://fxbug.dev/454397833).""",
            configurable = False,
        ),
        "version": attr.string(
            doc = "Crate version.",
            default = "0.1.0",
        ),
    },
)
