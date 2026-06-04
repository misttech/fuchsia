# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load(
    "@fuchsia_rules_common//:utils.bzl",
    "make_resource_struct",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "package_resources_providers",
)

def _fuchsia_package_resource_collection_impl(ctx):
    resources = []

    for dep in ctx.attr.resources:
        resources.extend(dep[FuchsiaPackageResourcesInfo].resources)

    return package_resources_providers(ctx, resources)

fuchsia_package_resource_collection = rule(
    doc = """Declares a collection of resources to be included in a Fuchsia package.
""",
    implementation = _fuchsia_package_resource_collection_impl,
    attrs = {
        "resources": attr.label_list(
            doc = "The resources to include in the package.",
            mandatory = True,
        ),
    },
)

def _fuchsia_package_resource_group_impl(ctx):
    resources = []
    dest = ctx.attr.dest.removesuffix("/")

    for src in ctx.files.srcs:
        if ctx.attr.basename_only:
            name = src.basename
        else:
            name = src.path
            if src.root:
                # Remove gen directories, e.g. bazel-bin
                name = name.removeprefix(src.root.path + "/")
            if src.owner.workspace_root:
                # Remove workspace name, e.g. external/fuchsia_sdk
                name = name.removeprefix(src.owner.workspace_root + "/")
            name = name.removeprefix(ctx.label.package + "/").removeprefix(ctx.attr.strip_prefix).removeprefix("/")

        resources.append(
            make_resource_struct(src = src, dest = dest + "/" + name),
        )

    return package_resources_providers(ctx, resources)

fuchsia_package_resource_group = rule(
    doc = """Declares a group of resources to be included in a Fuchsia package.
""",
    implementation = _fuchsia_package_resource_group_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = "The resource to include in the package.",
            mandatory = True,
            allow_files = True,
        ),
        "dest": attr.string(
            doc = "The path where this will be installed in the package.",
            mandatory = True,
        ),
        "strip_prefix": attr.string(
            doc = "A path to remove from the srcs",
            default = "",
        ),
        "basename_only": attr.bool(
            doc = "The dir will be removed from srcs attribute, and installed in dest + basename",
        ),
    },
)
