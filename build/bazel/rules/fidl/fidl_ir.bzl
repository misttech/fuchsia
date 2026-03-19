# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for generating and validating FIDL IR."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_build_info//:args.bzl", "runtime_supported_api_levels")
load("//build/json:validate_json.bzl", "validate_json")
load(":providers.bzl", "FidlLibraryInfo")

visibility("private")

# LINT.IfChange(available_default)
def _get_available(ctx):
    if ctx.attr.available:
        return ctx.attr.available

    api_level = ctx.attr._current_api_level[BuildSettingInfo].value

    if api_level == "PLATFORM":
        # FIDL directly supports targeting multiple API levels. "PLATFORM" is a
        # meta-level that refers to the set of all supported API levels.
        return ["fuchsia:" + ",".join(runtime_supported_api_levels)]
    else:
        return ["fuchsia:" + api_level]

# LINT.ThenChange(//build/fidl/fidl_library.gni:available_default)

def _fidlc_impl(ctx):
    library_name = ctx.attr.library_name

    response_file = ctx.actions.declare_file(ctx.attr.fidl_library_target_name + ".args")
    libraries_file = ctx.actions.declare_file(ctx.attr.fidl_library_target_name + ".libraries")

    dep_libraries = [dep[FidlLibraryInfo].libraries_file for dep in ctx.attr.deps]
    srcs_depset = depset(
        direct = ctx.files.srcs,
        transitive = [dep[FidlLibraryInfo].srcs_depset for dep in ctx.attr.deps],
    )

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
        response_file_args.add_all("--dep-libraries", dep_libraries)

    if ctx.attr.versioned:
        response_file_args.add("--versioned", ctx.attr.versioned)

    for available_value in _get_available(ctx):
        response_file_args.add("--available", available_value)

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
        inputs = [response_file] + srcs_depset.to_list(),
        outputs = [ctx.outputs.json_representation],
        mnemonic = "Fidlc",
    )

    return [
        FidlLibraryInfo(
            name = library_name,
            srcs_depset = srcs_depset,
            libraries_file = libraries_file,
        ),
    ]

_fidlc = rule(
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
            doc = "List of labels of other fidlc targets on which this library depends.",
            mandatory = False,
            providers = [FidlLibraryInfo],
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
        "_current_api_level": attr.label(
            default = "@//build/bazel:fuchsia_api_level",
        ),
    },
)

def _validated_ir_file_impl(ctx):
    return [
        # Allow the target to be used as the IR file.
        ctx.attr.unvalidated_file[DefaultInfo],
        # Pass through the `FidlLibraryInfo` provider so this target can be used
        # as a `deps` by `_fidlc()`.
        ctx.attr.unvalidated_file[FidlLibraryInfo],
        # Ensure the `validation_targets` are built.
        OutputGroupInfo(_validation = depset(ctx.files.validation_targets)),
    ]

_validated_ir_file = rule(
    doc = "Ensures `validation_targets` are built and returns.",
    implementation = _validated_ir_file_impl,
    attrs = {
        "unvalidated_file": attr.label(
            mandatory = True,
            providers = [FidlLibraryInfo],
        ),
        "validation_targets": attr.label_list(
            doc = "The build dependencies",
            mandatory = True,
            allow_files = False,
        ),
    },
)

def fidl_ir(name, deps, testonly, visibility, **kwargs):
    """Compiles a FIDL library to IR and returns the validated IR file.

    Args:
      name: Standard meaning.
      deps: List of labels of other FIDL libraries on which this library depends.
      testonly: Standard meaning.
      visibility: Standard meaning.

      **kwargs: Arguments to pass to the underlying `fidlc` rule.
    """
    fidlc_target_name = "%s_fidlc" % name

    _fidlc(
        name = fidlc_target_name,
        # IMPORTANT: The deps must be a label list that was passed to the
        # top-most symbolic macro in order for visibility to be checked
        # correctly. The reason for this is that label strings defined within
        # a symbolic macro will have their visibility checked against the
        # package containing the symbolic macro rather than the package
        # containing the BUILD.bazel file. Since targets defined within a
        # symbolic macro are visible to the package containing that macro, any
        # string labels added here that reference a target defined within the
        # symbolic macro (specifically, `name` below) would be visible to this
        # target regardless of the `visibility` passed to `fidl_library()`.
        # See https://fxbug.dev/446911800.
        deps = deps,
        json_representation = "%s.fidl.json" % name,
        testonly = testonly,
        visibility = ["//visibility:private"],
        **kwargs
    )

    validate_json_target_name = "%s_validate_json" % name
    validate_json(
        name = validate_json_target_name,
        data = fidlc_target_name,
        schema = "//tools/fidl/fidlc:schema.json",
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    # TODO(https://fxbug.dev/428285014): Implement linting of `srcs`.

    # IMPORTANT: The name of this target must be the the same as the name that
    # will be used in the `deps` of other FIDL libraries so that `deps` can be
    # used unmodified as explained above.
    _validated_ir_file(
        name = name,
        unvalidated_file = fidlc_target_name,
        validation_targets = [validate_json_target_name],
        testonly = testonly,
        visibility = visibility,
    )
