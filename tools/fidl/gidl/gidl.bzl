# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel equivalents of the GN `gidl()` and `gidl_*()` templates."""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")

visibility([
    "//sdk/ctf/tests/pkg/fidl/...",
    "//sdk/lib/fidl/...",
    "//src/lib/fidl/...",
    "//src/lib/fidl_codec/...",
    "//src/tests/benchmarks/fidl/...",
    "//src/tests/fidl/...",
    "//tools/fidl/...",
])

# LINT.IfChange

def _get_fidl_ir_json(fidl):
    """Returns the single FIDL IR JSON `File` of a `fidl_library()` target."""
    json_files = [f for f in fidl[DefaultInfo].files.to_list() if f.basename.endswith(".fidl.json")]
    if len(json_files) != 1:
        fail("Expected exactly one `.fidl.json` file in %s, got: %s" % (fidl.label, json_files))
    return json_files[0]

def _gidl_impl(ctx):
    fidl_ir_json = _get_fidl_ir_json(ctx.attr.fidl)

    args = ctx.actions.args()
    args.add("--type", ctx.attr.type)
    args.add("--language", ctx.attr.language)
    args.add("--json", fidl_ir_json)
    args.add("-out", ctx.outputs.output)
    args.add_all(ctx.attr.extra_args)
    args.add_all(ctx.files.inputs)

    ctx.actions.run(
        executable = ctx.executable._gidl_tool,
        arguments = [args],
        inputs = [fidl_ir_json] + ctx.files.inputs,
        outputs = [ctx.outputs.output],
        mnemonic = "Gidl",
        progress_message = "Generating GIDL (%s/%s) for %%{label}" % (ctx.attr.type, ctx.attr.language),
    )

    return [DefaultInfo(files = depset([ctx.outputs.output]))]

_gidl = rule(
    doc = "Runs the `gidl` tool to generate binding-specific tests or benchmarks from GIDL inputs.",
    implementation = _gidl_impl,
    attrs = {
        "type": attr.string(
            doc = "String indicating the type of generation.",
            mandatory = True,
            values = [
                "conformance",
                "benchmark",
                "measure_tape",
            ],
        ),
        "language": attr.string(
            doc = "String indicating the binding name.",
            mandatory = True,
            values = [
                "cpp",
                "hlcpp",
                "llcpp",
                "dynfidl",
                "rust",
                "rust_codec",
                "rust_next",
                "python",
                "fuzzer_corpus",
                "driver_cpp",
                "driver_llcpp",
                "reference",
            ],
        ),
        "inputs": attr.label_list(
            doc = "List of input `.gidl` files.",
            mandatory = True,
            allow_empty = False,
            allow_files = [".gidl"],
        ),
        "fidl": attr.label(
            doc = "`fidl_library()` target that defines types used in the GIDL files.",
            mandatory = True,
        ),
        "output": attr.output(
            doc = "Path of the output file to generate.",
            mandatory = True,
        ),
        "extra_args": attr.string_list(
            doc = "Additional command-line arguments to pass to `gidl`.",
            default = [],
        ),
        "_gidl_tool": attr.label(
            default = "//tools/fidl/gidl/cmd/gidl",
            executable = True,
            cfg = "exec",
        ),
    },
)

def gidl(
        *,
        name,
        type,
        language,
        inputs,
        fidl,
        output,
        extra_args = [],
        testonly = True,
        **kwargs):
    """Declares a GIDL generation target.

    Args:
      name: Standard meaning.
      type: Type of generation ("conformance", "benchmark", or "measure_tape").
      language: Binding name.
      inputs: List of input `.gidl` files.
      fidl: Label of the `fidl_library()` target defining types used in `inputs`.
      output: Output file name.
      extra_args: Optional extra arguments to pass to the `gidl` tool.
      testonly: Defaults to True.
      **kwargs: Additional standard rule attributes (e.g., `visibility`, `target_compatible_with`).
    """
    _gidl(
        name = name,
        type = type,
        language = language,
        inputs = inputs,
        fidl = fidl,
        output = output,
        extra_args = extra_args,
        testonly = testonly,
        **kwargs
    )

def _gidl_cc_source_set_impl(
        name,
        type,
        language,
        inputs,
        fidl,
        bindings_dep,
        extra_args,
        deps,
        build_flags,
        target_compatible_with,
        testonly,
        visibility):
    out_cc = "%s_gidl_generated.cc" % name

    _gidl(
        name = "%s_gidl" % name,
        type = type,
        language = language,
        inputs = inputs,
        fidl = fidl,
        output = out_cc,
        extra_args = extra_args,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fx_cc_library(
        name = name,
        srcs = [out_cc],
        deps = [bindings_dep] + (deps or []),
        build_flags = build_flags,
        alwayslink = True,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

_gidl_cc_source_set = macro(
    doc = "Internal symbolic macro generating a GIDL C++ library target.",
    implementation = _gidl_cc_source_set_impl,
    attrs = {
        "type": attr.string(
            doc = "Type of generation ('conformance' or 'benchmark').",
            mandatory = True,
            values = ["conformance", "benchmark"],
            configurable = False,
        ),
        "language": attr.string(
            doc = "C++ binding language.",
            mandatory = True,
            configurable = False,
        ),
        "inputs": attr.label_list(
            doc = "List of input `.gidl` files.",
            mandatory = True,
            allow_empty = False,
            allow_files = [".gidl"],
        ),
        "fidl": attr.label(
            doc = "`fidl_library()` target that defines types used in the GIDL files.",
            mandatory = True,
        ),
        "bindings_dep": attr.label(
            doc = "C++ bindings library target corresponding to `fidl`.",
            mandatory = True,
            # Must be non-configurable because it is placed inside a list literal
            # (`[bindings_dep] + (deps or [])`), and Bazel promotes configurable
            # macro attributes to `select(...)` objects, which cannot be nested
            # inside a list literal.
            configurable = False,
        ),
        "extra_args": attr.string_list(
            doc = "Additional command-line arguments to pass to `gidl`.",
            default = [],
        ),
        "deps": attr.label_list(
            doc = "Additional C++ dependencies.",
            default = [],
        ),
        "build_flags": attr.label_list(
            doc = "List of `build_flags()` targets.",
            default = [],
        ),
        "target_compatible_with": attr.label_list(
            doc = "Standard meaning.",
            default = [],
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = True,
            configurable = False,
        ),
    },
)

def _gidl_cc_library_wrapper(
        *,
        name,
        type,
        language,
        bindings_suffix,
        fidl,
        build_flags = [],
        **kwargs):
    fidl_label = native.package_relative_label(fidl)
    bindings_dep = fidl_label.same_package_label(fidl_label.name + bindings_suffix)
    _gidl_cc_source_set(
        name = name,
        type = type,
        language = language,
        fidl = fidl,
        bindings_dep = bindings_dep,
        build_flags = build_flags,
        **kwargs
    )

def gidl_cpp_conformance(*, name, fidl, **kwargs):
    """Declares a GIDL C++ natural bindings conformance test library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "conformance",
        language = "cpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        **kwargs
    )

def gidl_hlcpp_conformance(*, name, fidl, **kwargs):
    """Declares a GIDL HLCPP bindings conformance test library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "conformance",
        language = "hlcpp",
        bindings_suffix = "_hlcpp",
        fidl = fidl,
        **kwargs
    )

def gidl_llcpp_conformance(*, name, fidl, **kwargs):
    """Declares a GIDL C++ wire (LLCPP) bindings conformance test library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "conformance",
        language = "llcpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        # TODO(https://fxbug.dev/42176699): This target uses mutable tables which are deprecated,
        # rather than builders.
        build_flags = ["//build/cpp:fidl-wire-deprecated-mutable-tables"],
        **kwargs
    )

def gidl_cpp_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL C++ natural bindings benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "cpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        **kwargs
    )

def gidl_hlcpp_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL HLCPP bindings benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "hlcpp",
        bindings_suffix = "_hlcpp",
        fidl = fidl,
        **kwargs
    )

def gidl_llcpp_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL C++ wire (LLCPP) bindings benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "llcpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        # TODO(https://fxbug.dev/42176699): This target uses mutable tables which are deprecated,
        # rather than builders.
        build_flags = ["//build/cpp:fidl-wire-deprecated-mutable-tables"],
        **kwargs
    )

def gidl_reference_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL C++ reference benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "reference",
        bindings_suffix = "_cpp",
        fidl = fidl,
        # TODO(https://fxbug.dev/42176699): This target uses mutable tables which are deprecated,
        # rather than builders.
        build_flags = ["//build/cpp:fidl-wire-deprecated-mutable-tables"],
        **kwargs
    )

def gidl_driver_cpp_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL Driver C++ natural bindings benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "driver_cpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        **kwargs
    )

def gidl_driver_llcpp_benchmarks(*, name, fidl, **kwargs):
    """Declares a GIDL Driver C++ wire (LLCPP) bindings benchmark library."""
    _gidl_cc_library_wrapper(
        name = name,
        type = "benchmark",
        language = "driver_llcpp",
        bindings_suffix = "_cpp",
        fidl = fidl,
        # TODO(https://fxbug.dev/42176699): This target uses mutable tables which are deprecated,
        # rather than builders.
        build_flags = ["//build/cpp:fidl-wire-deprecated-mutable-tables"],
        **kwargs
    )

# LINT.ThenChange(//tools/fidl/gidl/gidl.gni)
