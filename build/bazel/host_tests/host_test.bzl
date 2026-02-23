# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_build_config//:defs.bzl", "build_config")
load("//build/bazel/rules:current_platform_info.bzl", "CurrentPlatformInfo")
load("//build/bazel/rules/python:py_toolchain.bzl", "PY_TOOLCHAIN_ATTRS", "generate_python_build_action")

# Set this to True to enable debug print() statements during analysis.
_DEBUG = False

FuchsiaHostTestInfo = provider(
    doc = "Provider for Bazel host tests visible to Fuchsia test runners (`fx test` and `botanist`).",
    fields = {
        "name": "The test name, used in `tests.json`, this may be different from test_label.name.",
        "test_label": "The canonical Bazel label of the host_test() target. Used by `fx test` " +
                      "to rebuild the test on demand.",
        "test_launcher": "A File value for the test launcher script.",
        "test_runtime_dir": "A string path the test runtime directory.",
        "test_runtime_deps_json": "A File value for the runtime_deps.json file for the test.",
        "os": "The OS of the test, using Fuchsia conventions",
        "cpu": "The CPU of the test, using Fuchsia conventions.",
    },
)

def _host_test_impl(ctx):
    # This generates three outputs:
    #
    #  - a `foo` launcher shell script, which changes the current directory to `foo.runtime_dir`
    #    then invokes the entry point (foo.runtime_dir/<binary>) with the test arguments.
    #
    #  - a `foo.runtime_dir` directory that contains symlinks to the binary entry point and
    #    its runfiles. However, the runfiles manifest in it has been adjusted to contain
    #    target paths that are relative to `foo.runtime_dir`.
    #
    #  - a `foo.runtime_deps.json` file that contains the runtime dependencies of the test,
    #    where all paths are relative to the Ninja build directory. This will be used by the
    #    tests.json entry for this test.

    launcher = ctx.actions.declare_file(ctx.attr.name)
    runtime_dir = ctx.actions.declare_directory(ctx.attr.name + ".runtime_dir")
    test_runtime_deps_json = ctx.actions.declare_file(ctx.attr.name + ".runtime_deps.json")

    outputs = [launcher, runtime_dir, test_runtime_deps_json]

    binary_info = ctx.attr.binary[DefaultInfo]
    binary_runfiles_manifest = binary_info.files_to_run.runfiles_manifest
    binary_repo_mapping_manifest = binary_info.files_to_run.repo_mapping_manifest
    inputs = (
        binary_info.files.to_list() +
        binary_info.default_runfiles.files.to_list() +
        ctx.files.data +
        ctx.files._python_modules
    ) + [
        binary_runfiles_manifest,
        binary_repo_mapping_manifest,
    ]

    entry_point = binary_info.files_to_run.executable

    # `args` is always added implicitly by Bazel for any rule whose name ends with `_test`.
    # However, because these values are treated specially (i.e. they are not stored in providers)
    # do not use them here, in favor of `test_args`.
    if ctx.attr.args:
        fail("Do not use `args` for this rule. Use `test_args` instead.")

    test_args = []
    for arg in ctx.attr.test_args:
        loc_expanded = ctx.expand_location(arg, ctx.attr.data)
        test_args.append(ctx.expand_make_variables(
            "args",
            loc_expanded,
            {},
        ))

    data_runfiles = ctx.runfiles(
        files = ctx.files.data,
    ).merge_all(
        [target[DefaultInfo].default_runfiles for target in ctx.attr.data],
    )

    if _DEBUG:
        def files_list_dump(name, files):
            content = "{} files count={}:\n".format(name, len(files))
            for f in files:
                content += "  {}\n".format(f.path)
            content += "\n"
            return content

        print("launcher path: {}".format(launcher.path))
        print("runtime_dir path: {}".format(runtime_dir.path))
        print("test_args: {}".format(test_args))
        print(files_list_dump("data_runfiles", data_runfiles.files.to_list()))

    generate_python_build_action(
        ctx = ctx,
        py_script = ctx.file._generate_host_test_wrapper,
        arguments = [
            "--entry-point={}".format(entry_point.path),
            "--entry-runfiles-manifest={}".format(binary_runfiles_manifest.path),
            "--test-label={}".format(ctx.label),
            "--output-launcher={}".format(launcher.path),
            "--output-runtime-dir={}".format(runtime_dir.path),
            "--output-test-runtime-deps-json={}".format(test_runtime_deps_json.path),
            "--bazel-execroot={}".format(ctx.label.workspace_root),
        ] + [
            "--data-runfile={}".format(f.path)
            for f in data_runfiles.files.to_list()
        ] + [
            "--test-arg={}".format(arg)
            for arg in test_args
        ],
        outputs = outputs,
        inputs = inputs,
        # Don't run in a sandbox to ensure the current path is the execroot. This is required
        # to compute the path to the Ninja build directory inside the script.
        execution_requirements = {
            "local": "1",
            "no-sandbox": "1",
        },
    )

    runfiles = ctx.runfiles(
        files = [launcher, runtime_dir],
    ).merge_all([binary_info.default_runfiles, data_runfiles])

    current_platform = ctx.attr._current_platform[CurrentPlatformInfo]

    return [
        DefaultInfo(
            files = depset(outputs),
            runfiles = runfiles,
            executable = launcher,
        ),
        FuchsiaHostTestInfo(
            name = ctx.attr.test_name or ctx.label.name,
            test_label = ctx.label,
            test_launcher = launcher,
            test_runtime_dir = runtime_dir.path,
            test_runtime_deps_json = test_runtime_deps_json,
            os = current_platform.os,
            cpu = current_platform.cpu,
        ),
    ]

host_test = rule(
    implementation = _host_test_impl,
    doc = """Create a Bazel host test for Fuchsia.

    Use this rule to create a host test that can be run with `fx test` or `botanist` on infra.

    Regular Bazel test targets are designed to be run from the Bazel execroot only, while the
    Fuchsia test runners invoke the tests from the Ninja build directory, or its equivalent on
    infra test bots. Another issue is that Bazel test attributes (passed through the `args`
    attribute of test rules like `cc_test()` or `py_test()`) are not stored in the generated
    test artifacts. Instead `bazel run` or `bazel test` extract them from the build definition
    before invoking the corresponding test entry point. And regretably there is no way to
    properly query for these arguments (these are never stored in providers).

    This rule solves both problems by ensuring that it produces a test artifact that can be
    invoked from any location, and which hard-codes the test arguments passed in the build
    definition. It also generates an auxiliary file required by the Fuchsia test runners to
    locate the test's runtime dependencies.

    The resulting target *is* also a Bazel test that can be invoked locally using
    `fx bazel test --config=host <label>`.

    This rule does *not* work like the GN host_test() template, because runtime dependencies are
    specified using regular `data` label lists, and the test binary *must* use a runfiles library
    to access these at runtime. For more information, see //build/bazel/BAZEL_RUNFILES.md.
    """,
    test = True,
    executable = True,
    attrs = {
        "binary": attr.label(
            doc = "The test binary to wrap. This can be any executable target, including a Bazel " +
                  "test target, as long as it doesn't have its own `args` attribute values.",
            mandatory = True,
        ),
        "test_name": attr.string(
            doc = "Optional override for the name of the test, as seen by `fx test` and `botanist`. " +
                  "The default is the host_test() target name. Used for display and grouping.",
        ),
        "test_args": attr.string_list(
            doc = "Arguments to pass to the test binary. Do *not* use `args` for this purpose.",
            default = [],
        ),
        "data": attr.label_list(
            doc = "Data files to include in the test's runfiles. Note that the test should always " +
                  "use a runfiles library to access these at runtime.",
            default = [],
            allow_files = True,
        ),
        "_generate_host_test_wrapper": attr.label(
            doc = "Internal script used to generate the final test wrapper script.",
            default = "//build/bazel/scripts:generate_host_test_wrapper.py",
            allow_single_file = True,
        ),
        "_python_modules": attr.label_list(
            doc = "Python modules imported by _host_test_wrapper.",
            allow_files = True,
            default = [
                "//build/bazel/scripts:build_utils.py",
                "//build/bazel/scripts:runfiles_utils.py",
            ],
        ),
        "_current_platform": attr.label(
            providers = [CurrentPlatformInfo],
            default = "@//build/bazel:current_platform",
        ),
    } | PY_TOOLCHAIN_ATTRS,
)
