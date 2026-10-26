# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load(
    ":providers.bzl",
    "FuchsiaComponentManifestInfo",
)

def compile_component_manifest(
        ctx,
        cmc_tool,
        manifest_in,
        component_name,
        includes,
        include_paths,
        toolchain = None,
        extra_args = []):
    """Compiles a `.cml` component manifest into a `.cm` file with `cmc compile`.

    Args:
        ctx: The rule context.
        cmc_tool: The `cmc` executable.
        manifest_in: The `.cml` File to compile.
        component_name: The name of the component.
        includes: A list of `.cml` shard Files that `manifest_in` may include.
        include_paths: A list of directories to search for included shards.
        toolchain: The toolchain type to run `cmc` with, if any.
        extra_args: Additional arguments to pass to `cmc compile`, such as
            `--features` or `--must-offer-dictionary`.

    Returns:
        A list of `DefaultInfo` and `FuchsiaComponentManifestInfo` providers.
    """

    # output should have the .cm extension
    manifest_out = ctx.actions.declare_file("meta/{}.cm".format(component_name))
    config_package_path = "meta/%s.cvf" % component_name

    include_path_args = []
    for w in include_paths:
        include_path_args.extend(["--includepath", w])

    config_values_package_path_args = [
        "--config-package-path",
        config_package_path,
    ]

    args = [
        "compile",
        "--output",
        manifest_out.path,
        manifest_in.path,
        "--includepath",
        manifest_in.dirname,
    ] + include_path_args + config_values_package_path_args + extra_args

    ctx.actions.run(
        executable = cmc_tool,
        arguments = args,
        inputs = [manifest_in] + includes,
        outputs = [manifest_out],
        mnemonic = "CmcCompile",
        **({"toolchain": toolchain} if toolchain else {})
    )

    return [
        DefaultInfo(files = depset([manifest_out])),
        FuchsiaComponentManifestInfo(
            compiled_manifest = manifest_out,
            component_name = component_name,
            config_package_path = config_package_path,
        ),
    ]
