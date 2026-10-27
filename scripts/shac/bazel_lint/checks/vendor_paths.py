# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that public Fuchsia Bazel files do not reference private vendor subpaths."""

from framework import (
    ALL_BAZEL_FILE_KINDS,
    BazelCheck,
    FileContext,
    FileKind,
    StringInfo,
    normalize_repo_prefix,
    register_check,
)

from .visibility_utils import (
    ALLOWED_VENDOR_LOAD_VISIBILITY_LABELS,
    ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS,
    VENDOR_VISIBILITY_REFERENCE_URL,
    is_load_visibility,
)

_ERROR_MSG = (
    "Do not reference vendor subpaths in public Fuchsia code. "
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, '
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` '
    "are permitted for target visibility."
)


# Safe generic vendor strings that do not reference a specific vendor
# repository or private subpath (e.g. the wildcard template passed to
# `visibility_templates` in `//build/bazel/toplevel.MODULE.bazel`).
# Note: These values are in normalized form (without leading `@` or `@@`) and
# must only be used after normalizing strings with `normalize_repo_prefix()`.
_ALLOWED_VENDOR_STRINGS: frozenset[str] = frozenset(
    {
        "//vendor/*/build:hlcpp_allowlist",
    }
)


def _is_startswith_vendor_prefix_check(s: StringInfo, normalized: str) -> bool:
    """Returns True for `package_path.startswith("//vendor/")` checks in build rules.

    `normalized` has already been normalized with `normalize_repo_prefix()`.
    """
    return (
        s.enclosing_call is not None
        and s.enclosing_call.func_name.endswith(".startswith")
        and normalized in ("//vendor", "//vendor/")
    )


def _starts_with_vendor_prefix(
    normalized: str, *, in_visibility_arg: bool
) -> bool:
    """Returns True if `normalized` references a `//vendor` or `vendor` path.

    `normalized` has already been normalized with `normalize_repo_prefix()`
    (which converts `"@//..."` and `"@@//..."` to `"//..."`).

    - Strings starting with `"//vendor"` are checked everywhere in the file
      (catching standard labels like `"//vendor/foo:bar"`, bare `"//vendor"`,
      and missing-delimiter typos like `"//vendorfoo"`).
    - Relative vendor paths starting with `"vendor/"` or `"vendor:"` are checked
      everywhere in the file.
    - Inside visibility arguments (`visibility(...)`, `package_group(packages = ...)`,
      `visibility = [...]`, and `default_visibility = [...]`), any string
      starting with `"vendor"` is also checked (catching missing-slash typos
      such as `"vendorfoo:__pkg__"`).
    """
    if normalized.startswith("//vendor"):
        return True
    if normalized.startswith(("vendor/", "vendor:")):
        return True
    if in_visibility_arg and normalized.startswith("vendor"):
        return True
    return False


@register_check
class VendorPathsCheck(BazelCheck):
    """Disallows private `//vendor/...` subpath references in public Fuchsia Bazel files."""

    name = "bazel_vendor_paths"
    file_kinds = ALL_BAZEL_FILE_KINDS
    policy_exempt_globs = (
        # Vendor repositories themselves may reference paths within //vendor/.
        "vendor/**",
    )
    # Vendor subpaths must never be referenced in public Fuchsia code.
    supports_allowlist = False

    def check_string(self, fc: FileContext, s: StringInfo) -> None:
        if (
            s.is_docstring
            or s.in_compare
            or s.in_fail
            or s.keyword_arg == "doc"
        ):
            return

        # Normalize leading `@//` and `@@//` to `//` before matching prefixes
        # or comparing against the normalized allowed path constants.
        normalized = normalize_repo_prefix(s.value)
        if (
            normalized in _ALLOWED_VENDOR_STRINGS
            or _is_startswith_vendor_prefix_check(s, normalized)
        ):
            return

        stmt_call = s.enclosing_statement_call
        in_visibility_arg = (
            fc.kind == FileKind.BZL
            and stmt_call is not None
            and is_load_visibility(stmt_call)
        ) or (
            stmt_call is not None
            and stmt_call.func_name == "package_group"
            and s.keyword_arg == "packages"
        )
        allowed_labels = (
            ALLOWED_VENDOR_LOAD_VISIBILITY_LABELS
            if in_visibility_arg
            else ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS
        )
        in_visibility_arg |= s.keyword_arg == "visibility" or (
            stmt_call is not None
            and stmt_call.func_name == "package"
            and s.keyword_arg == "default_visibility"
        )

        if not _starts_with_vendor_prefix(
            normalized, in_visibility_arg=in_visibility_arg
        ):
            return

        # `ALLOWED_VENDOR_*_LABELS` contain normalized `//vendor...` strings and
        # are compared against `normalized` (with leading `@` / `@@` stripped).
        if normalized in allowed_labels:
            return

        fc.report_string(
            _ERROR_MSG,
            s.node,
            s.value,
            level="error",
            url=VENDOR_VISIBILITY_REFERENCE_URL,
        )
