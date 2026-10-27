# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that .bzl files declare top-level load visibility()."""

from framework import (
    BazelCheck,
    FileContext,
    FileKind,
    register_check,
)

from .visibility_utils import (
    LOAD_VISIBILITY_REFERENCE_URL,
    is_load_visibility,
)

_MISSING_VISIBILITY_ERROR_MSG = (
    "`.bzl` files must declare top-level load `visibility(...)`. "
    'Use `visibility("private")` if only used within the same package, '
    "or specify the packages or subtrees that may load this file "
    '(e.g., `visibility(["//src/..."])`).'
)


@register_check
class BzlRequireLoadVisibilityCheck(BazelCheck):
    """Requires `.bzl` files to declare top-level load `visibility(...)`."""

    name = "bazel_bzl_require_load_visibility"
    file_kinds = frozenset({FileKind.BZL})
    # Unlike `bazel_load_visibility` (which exempts `build/bazel_sdk/` rules
    # directories because SDK rules are loaded by external repositories and may
    # legitimately use `visibility("public")`), `bazel_bzl_require_load_visibility`
    # has no `policy_exempt_globs` because even SDK `.bzl` files should
    # explicitly declare `visibility(...)` (e.g., `visibility("private")` in
    # `fuchsia/private/**`).
    supports_allowlist = True

    def check_module(self, fc: FileContext) -> None:
        if not any(is_load_visibility(call) for call in fc.calls):
            fc.report(
                _MISSING_VISIBILITY_ERROR_MSG,
                level="error",
                url=LOAD_VISIBILITY_REFERENCE_URL,
            )
