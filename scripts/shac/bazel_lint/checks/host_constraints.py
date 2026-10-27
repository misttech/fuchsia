# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that Bazel files do not load or use HOST_CONSTRAINTS."""

import ast

from framework import (
    ALL_BAZEL_FILE_KINDS,
    BazelCheck,
    CallInfo,
    FileContext,
    LoadInfo,
    register_check,
)

_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#target-compatible-with-host-targets"

_HOST_CONSTRAINTS_MODULE = "@platforms//host:constraints.bzl"
_HOST_CONSTRAINTS_SYMBOL = "HOST_CONSTRAINTS"

_LOAD_ERROR_MSG = (
    "Do not load `HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl`; "
    "load `HOST_OS_CONSTRAINTS` from `//build/bazel/platforms:constraints.bzl` instead."
)

_TARGET_COMPATIBLE_WITH_ERROR_MSG = (
    "Do not use `HOST_CONSTRAINTS` in `target_compatible_with`; "
    "use `HOST_OS_CONSTRAINTS` from `//build/bazel/platforms:constraints.bzl` instead."
)


def _is_host_constraints_name(fc: FileContext, name: str) -> bool:
    """Returns True if `name` is `HOST_CONSTRAINTS` or an alias loaded from `@platforms//host:constraints.bzl`."""
    if name == _HOST_CONSTRAINTS_SYMBOL:
        return True
    loaded = fc.loaded_symbols.get(name)
    return (
        loaded is not None
        and loaded[0] == _HOST_CONSTRAINTS_MODULE
        and loaded[1] == _HOST_CONSTRAINTS_SYMBOL
    )


@register_check
class HostConstraintsCheck(BazelCheck):
    """Disallows loading `HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl` or using it in `target_compatible_with`."""

    name = "bazel_host_constraints"
    file_kinds = ALL_BAZEL_FILE_KINDS
    policy_exempt_globs = (
        # Standalone workspace that only runs during development when updating
        # third-party Rust crates.
        "build/bazel/update-rustc-third-party/**",
        # Files in the Bazel SDK's rules directories are in separate Bzlmod
        # modules and cannot load //build/bazel/platforms:constraints.bzl;
        # additionally, their uses are for the build rather than host tool
        # targets.
        "build/bazel_sdk/bazel_rules_fuchsia/**",
        "build/bazel_sdk/fuchsia_rules_common/**",
    )
    supports_allowlist = True
    supports_targets_allowlist = True

    def check_load(self, fc: FileContext, load: LoadInfo) -> None:
        if load.module_label != _HOST_CONSTRAINTS_MODULE:
            return
        for _local_name, exported_name, sym_node in load.symbols:
            if exported_name == _HOST_CONSTRAINTS_SYMBOL:
                fc.report(
                    _LOAD_ERROR_MSG,
                    sym_node,
                    level="error",
                    url=_REFERENCE_URL,
                )

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if "target_compatible_with" not in call.keywords:
            return
        kw = call.keywords["target_compatible_with"]
        for node in ast.walk(kw.value):
            if isinstance(node, ast.Name) and _is_host_constraints_name(
                fc, node.id
            ):
                fc.report(
                    _TARGET_COMPATIBLE_WITH_ERROR_MSG,
                    node,
                    level="error",
                    url=_REFERENCE_URL,
                )
