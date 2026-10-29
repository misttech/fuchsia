# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel equivalent of the GN `measure_tape()` template."""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

visibility([
    "//sdk/fidl/...",
    "//src/...",
    "//tools/fidl/measure-tape/...",
])

# LINT.IfChange
def _get_fidl_ir_json(fidl):
    """Returns the single FIDL IR JSON `File` of a `fidl_library()` target."""
    json_files = [f for f in fidl[DefaultInfo].files.to_list() if f.basename.endswith(".fidl.json")]
    if len(json_files) != 1:
        fail("Expected exactly one `.fidl.json` file in %s, got: %s" % (fidl.label, json_files))
    return json_files[0]

def _measure_tape_gen_impl(ctx):
    target_binding = ctx.attr.target_binding
    json_files = [_get_fidl_ir_json(fidl) for fidl in ctx.attr.fidls]

    args = ctx.actions.args()
    args.add("--target-binding", target_binding)
    if target_binding == "hlcpp":
        args.add("--out-cc", ctx.outputs.out_cc)
        args.add("--out-h", ctx.outputs.out_h)
        args.add("--h-include-path", ctx.attr.h_include_path)
        outputs = [ctx.outputs.out_cc, ctx.outputs.out_h]
    elif target_binding == "rust":
        args.add("--out-rs", ctx.outputs.out_rs)
        if ctx.attr.use_rust_common:
            args.add("--rust-common")
        outputs = [ctx.outputs.out_rs]
    else:
        fail("Unsupported `target_binding`: %s" % target_binding)
    args.add_all(ctx.attr.target_types, before_each = "--target-types")
    args.add_all(json_files, before_each = "--json")

    ctx.actions.run(
        executable = ctx.executable._measure_tape,
        arguments = [args],
        inputs = json_files,
        outputs = outputs,
        mnemonic = "MeasureTape",
        progress_message = "Generating measure tape %{label}",
    )

    return [DefaultInfo(files = depset(outputs))]

_measure_tape_gen = rule(
    doc = "Runs the `measure-tape` tool to generate a measuring tape.",
    implementation = _measure_tape_gen_impl,
    attrs = {
        "fidls": attr.label_list(
            doc = "`fidl_library()` targets providing the FIDL IR of all types to measure.",
            mandatory = True,
            allow_empty = False,
        ),
        "target_binding": attr.string(
            doc = "The bindings to generate the measure tape for.",
            mandatory = True,
            values = ["hlcpp", "rust"],
        ),
        "target_types": attr.string_list(
            doc = "Fully qualified FIDL types for which to generate a measuring tape.",
            mandatory = True,
            allow_empty = False,
        ),
        "use_rust_common": attr.bool(
            doc = "Whether to use _common crate names for Rust bindings (Rust only).",
            default = False,
        ),
        "h_include_path": attr.string(
            doc = "Path used to `#include` the generated header (HLCPP only).",
        ),
        "out_cc": attr.output(doc = "Generated .cc file (HLCPP only)."),
        "out_h": attr.output(doc = "Generated .h file (HLCPP only)."),
        "out_rs": attr.output(doc = "Generated .rs file (Rust only)."),
        "_measure_tape": attr.label(
            default = "//tools/fidl/measure-tape/src:measure-tape",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _measure_tape_impl(
        name,
        fidls,
        bindings_deps,
        target_binding,
        target_types,
        use_rust_common,
        target_compatible_with,
        testonly,
        visibility):
    gen_target_name = name + "_gen"

    # Place all generated files in a directory specific to this target, to
    # avoid collisions and to support `strip_include_prefix` for HLCPP.
    gen_dir = name + "_measure_tape"

    if target_binding == "hlcpp":
        h_include_path = "measure_tape/hlcpp/%s.h" % name
        out_h = "%s/%s" % (gen_dir, h_include_path)
        out_cc = "%s/measure_tape/hlcpp/%s.cc" % (gen_dir, name)
        _measure_tape_gen(
            name = gen_target_name,
            fidls = fidls,
            target_binding = target_binding,
            target_types = target_types,
            h_include_path = h_include_path,
            out_cc = out_cc,
            out_h = out_h,
            target_compatible_with = target_compatible_with,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        fx_cc_library(
            name = name,
            srcs = [out_cc],
            hdrs = [out_h],
            # Allows `#include <measure_tape/hlcpp/<name>.h>`.
            strip_include_prefix = gen_dir,
            deps = bindings_deps,
            target_compatible_with = target_compatible_with,
            testonly = testonly,
            visibility = visibility,
        )
    elif target_binding == "rust":
        out_rs = "%s/rust/src/lib.rs" % gen_dir
        _measure_tape_gen(
            name = gen_target_name,
            fidls = fidls,
            target_binding = target_binding,
            target_types = target_types,
            use_rust_common = use_rust_common,
            out_rs = out_rs,
            target_compatible_with = target_compatible_with,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        rustc_library(
            name = name,
            crate_name = name.replace("-", "_"),
            edition = "2024",
            crate_root = out_rs,
            srcs = [out_rs],
            deps = [
                "//sdk/rust/zx-types",
                "//src/lib/fidl/rust/fidl",
            ] + bindings_deps,
            disable_clippy = True,
            target_compatible_with = target_compatible_with,
            testonly = testonly,
            visibility = visibility,
        )
    else:
        fail("Unsupported `target_binding`: %s" % target_binding)

_measure_tape = macro(
    doc = """Declares a Measure Tape.

    Use the `measure_tape()` wrapper instead.

    This generates a measuring tape library for one or multiple FIDL types.

    For `target_binding = "hlcpp"`, this defines a `fx_cc_library()` named `name`,
    whose generated header can be included with
    `#include <measure_tape/hlcpp/<name>.h>`.

    For `target_binding = "rust"`, this defines a `rustc_library()` named `name`,
    whose crate name is `name` (with `-` replaced by `_`).

    This is the Bazel equivalent of the GN `measure_tape()` template in
    //tools/fidl/measure-tape/measure_tape.gni, and is converted to it by bazel2gn.
    """,
    implementation = _measure_tape_impl,
    attrs = {
        "fidls": attr.label_list(
            doc = "`fidl_library()` targets defining all the types (transitively) referenced by `target_types`. The library depends on the `target_binding` bindings of each of them.",
            mandatory = True,
            allow_empty = False,
            configurable = False,
        ),
        "target_binding": attr.string(
            doc = "The binding for which to create the measure tape. Either 'hlcpp' or 'rust'.",
            mandatory = True,
            values = ["hlcpp", "rust"],
            configurable = False,
        ),
        "bindings_deps": attr.label_list(
            doc = "The `target_binding` bindings libraries of `fidls`. Set by the wrapper macro.",
            mandatory = True,
        ),
        "target_types": attr.string_list(
            doc = "Fully qualified FIDL types (e.g. 'fuchsia.pkg/BlobInfo') for which to generate a measuring tape.",
            mandatory = True,
            allow_empty = False,
            configurable = False,
        ),
        "use_rust_common": attr.bool(
            doc = "Whether to use _common crate names for Rust bindings.",
            default = False,
        ),
        "target_compatible_with": attr.label_list(
            doc = "Standard meaning.",
            default = [],
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)

def measure_tape(*, name, fidls, target_binding, use_rust_common = None, **kwargs):
    """Declares a Measure Tape.

    This is a wrapper around `_measure_tape()` that computes the labels of the
    bindings libraries of `fidls`. This must be done here rather than in the
    symbolic macro so that their visibility is checked against the package
    calling `measure_tape()` rather than against the package of this file.

    See `_measure_tape()` for documentation.

    Args:
      name: Standard meaning.
      fidls: See `_measure_tape()`.
      target_binding: See `_measure_tape()`.
      use_rust_common: For `target_binding == "rust"`, whether to use
        `_rust_common` bindings. Defaults to `True` on host and `False` on
        Fuchsia.
      **kwargs: Passed to `_measure_tape()`.
    """
    if target_binding == "hlcpp":
        if use_rust_common != None:
            fail('`use_rust_common` is only supported when `target_binding` is "rust"')
        bindings_deps = []
        for fidl in fidls:
            fidl_label = native.package_relative_label(fidl)
            bindings_deps.append(fidl_label.same_package_label(fidl_label.name + "_hlcpp"))
        use_rust_common_val = False
    elif target_binding == "rust":
        rust_deps = []
        rust_common_deps = []
        for fidl in fidls:
            fidl_label = native.package_relative_label(fidl)
            rust_deps.append(fidl_label.same_package_label(fidl_label.name + "_rust"))
            rust_common_deps.append(fidl_label.same_package_label(fidl_label.name + "_rust_common"))
        if use_rust_common == None:
            use_rust_common_val = select({
                "@platforms//os:fuchsia": False,
                "//conditions:default": True,
            })
            bindings_deps = select({
                "@platforms//os:fuchsia": rust_deps,
                "//conditions:default": rust_common_deps,
            })
        elif use_rust_common:
            use_rust_common_val = True
            bindings_deps = rust_common_deps
        else:
            use_rust_common_val = False
            bindings_deps = rust_deps
    else:
        fail('`target_binding` must be "hlcpp" or "rust", got: %s' % target_binding)

    _measure_tape(
        name = name,
        fidls = fidls,
        bindings_deps = bindings_deps,
        target_binding = target_binding,
        use_rust_common = use_rust_common_val,
        **kwargs
    )

# LINT.ThenChange(//tools/fidl/measure-tape/measure_tape.gni)
