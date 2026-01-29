# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for declaring a Fuchsia starnix container."""

load("//fuchsia/private:providers.bzl", "FuchsiaPackageInfo")
load(
    ":providers.bzl",
    "FuchsiaAssembledPackageInfo",
    "FuchsiaStarnixContainerInfo",
)
load(":utils.bzl", "create_pkg_detail")

def _fuchsia_starnix_container_impl(ctx):
    hals_config = []
    for hal in ctx.attr.hals:
        hals_config.append(create_pkg_detail(hal))

    all_files = [ctx.file.system]
    if ctx.attr.vendor:
        all_files.append(ctx.file.vendor)
    if ctx.attr.ramdisk:
        all_files.append(ctx.file.ramdisk)
    if ctx.attr.fstab:
        all_files.append(ctx.file.fstab)
    all_files += ctx.files.init
    all_files += ctx.files.hals
    return [
        DefaultInfo(files = depset(all_files)),
        FuchsiaStarnixContainerInfo(
            name = ctx.attr.package_name if ctx.attr.package_name else ctx.label.name,
            hals = hals_config,
            skip_subpackages = ctx.attr.skip_subpackages,
            system = ctx.file.system.path,
            vendor = ctx.file.vendor.path if ctx.attr.vendor else None,
            ramdisk = ctx.file.ramdisk.path if ctx.attr.ramdisk else None,
            fstab = ctx.file.fstab.path if ctx.attr.fstab else None,
            init = [f.path for f in ctx.files.init],
        ),
    ]

fuchsia_starnix_container = rule(
    doc = "Declare a starnix container configuration.",
    implementation = _fuchsia_starnix_container_impl,
    attrs = {
        "package_name": attr.string(
            doc = "Name of the starnix container package",
        ),
        "hals": attr.label_list(
            doc = "List of HAL packages",
            allow_files = True,
            providers = [[FuchsiaAssembledPackageInfo], [FuchsiaPackageInfo]],
        ),
        "skip_subpackages": attr.bool(
            doc = "Whether to skip including HALs as subpackages",
            default = False,
        ),
        "system": attr.label(
            doc = "Path to system image",
            allow_single_file = True,
            mandatory = True,
        ),
        "vendor": attr.label(
            doc = "Path to vendor image",
            allow_single_file = True,
        ),
        "ramdisk": attr.label(
            doc = "Path to ramdisk image",
            allow_single_file = True,
        ),
        "fstab": attr.label(
            doc = "Path to fstab",
            allow_single_file = True,
        ),
        "init": attr.label_list(
            doc = "List of paths to extra init scripts",
            allow_files = True,
            default = [],
        ),
    },
)
