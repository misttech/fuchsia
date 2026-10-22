# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks load visibility() declarations in .bzl files."""

from __future__ import annotations

import ast
from pathlib import PurePosixPath

from framework import (
    BazelCheck,
    CallInfo,
    FileContext,
    FileKind,
    register_check,
)

from .visibility_utils import (
    LOAD_VISIBILITY_REFERENCE_URL,
    PUBLIC_VISIBILITY_REFERENCE_URL,
    VENDOR_VISIBILITY_REFERENCE_URL,
    is_disallowed_vendor_load_visibility,
    is_load_visibility,
)

_PUBLIC_ERROR_MSG = (
    'Do not use `visibility("public")`. '
    "Specify the packages or subtrees that may load this file "
    '(e.g., `visibility(["//src/..."])`), or `visibility("private")` '
    "if only used within the same package."
)

_VENDOR_ERROR_MSG = (
    "Do not include vendor subpaths in load visibility. "
    'Only `"//vendor/..."` and `"//vendor/google/..."` '
    "are permitted in public Fuchsia code."
)

_BROAD_WARNING_MSG = (
    'Avoid broad `visibility(["//..."])` in `.bzl` files outside '
    "`//build/bazel/rules/`. Prefer scoping load visibility to specific "
    'packages or subtrees (e.g., `visibility(["//src/..."])`).'
)

# General-purpose platform rules and macros under //build/bazel/rules/ are
# legitimately loaded across the entire repository and may use "//...".
# This exemption must be applied manually rather than via `policy_exempt_globs`
# because these files are only exempt from this finding, not the entire check.
_BROAD_VISIBILITY_EXEMPT_GLOBS: tuple[str, ...] = ("build/bazel/rules/**",)


def _is_broad_visibility_exempt(rel_path: str) -> bool:
    path = PurePosixPath(rel_path)
    return any(
        path.full_match(pattern) for pattern in _BROAD_VISIBILITY_EXEMPT_GLOBS
    )


def _iter_load_visibility_constants(call: CallInfo) -> list[ast.Constant]:
    """Returns string literal `ast.Constant` nodes passed to `visibility(...)`."""
    exprs: list[ast.expr] = list(call.args)
    if "value" in call.keywords:
        exprs.append(call.keywords["value"].value)

    nodes: list[ast.Constant] = []
    for expr in exprs:
        if isinstance(expr, ast.Constant) and isinstance(expr.value, str):
            nodes.append(expr)
        elif isinstance(expr, (ast.List, ast.Tuple)):
            for elt in expr.elts:
                if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                    nodes.append(elt)
    return nodes


@register_check
class LoadVisibilityCheck(BazelCheck):
    """Flags `visibility("public")` (error) and `visibility("//...")` (warning) in `.bzl` files."""

    name = "bazel_load_visibility"
    file_kinds = frozenset({FileKind.BZL})
    policy_exempt_globs = (
        # Files in the Bazel SDK's rules directories are loaded by external
        # repositories and SDK users.
        "build/bazel_sdk/bazel_rules_fuchsia/**",
        "build/bazel_sdk/fuchsia_rules_common/**",
    )

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if not is_load_visibility(call):
            return
        for node in _iter_load_visibility_constants(call):
            if node.value == "public":
                fc.report_string(
                    _PUBLIC_ERROR_MSG,
                    node,
                    "public",
                    level="error",
                    url=PUBLIC_VISIBILITY_REFERENCE_URL,
                )
            elif is_disallowed_vendor_load_visibility(node.value):
                fc.report_string(
                    _VENDOR_ERROR_MSG,
                    node,
                    node.value,
                    level="error",
                    url=VENDOR_VISIBILITY_REFERENCE_URL,
                )
            elif node.value == "//..." and not _is_broad_visibility_exempt(
                fc.path
            ):
                fc.report_string(
                    _BROAD_WARNING_MSG,
                    node,
                    "//...",
                    level="warning",
                    url=LOAD_VISIBILITY_REFERENCE_URL,
                )
