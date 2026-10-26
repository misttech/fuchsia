# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that BUILD.bazel files avoid default_visibility in package()."""

import ast

from framework import (
    BazelCheck,
    CallInfo,
    FileContext,
    FileKind,
    register_check,
)

from .visibility_utils import (
    ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS,
    VENDOR_VISIBILITY_REFERENCE_URL,
    has_disallowed_vendor_target_visibility,
)

_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#avoid-package-default-visibility"

_BROAD_VISIBILITY_ERROR_MSG = (
    "Do not use `default_visibility` in `package()` for broad visibility "
    "(such as `//visibility:public` or top-level `__subpackages__`). "
    "Specify `visibility` on individual targets instead."
)

_VENDOR_ERROR_MSG = (
    "Do not include vendor subpaths in `default_visibility`. "
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, '
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` '
    "are permitted in public Fuchsia code."
)

_NARROW_VISIBILITY_WARNING_MSG = (
    "Prefer specifying `visibility` on individual targets rather than setting "
    "`default_visibility` in `package()`. `default_visibility` should only be "
    "used when every target in the file (with no private helper targets) "
    "shares the same narrow visibility."
)


def _is_broad_visibility_label(label: str) -> bool:
    """Returns True if `label` grants broad package visibility."""
    if label == "//visibility:public":
        return True
    if label in ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS:
        return False
    if label.startswith("//") and label.endswith(":__subpackages__"):
        pkg = label[2 : -len(":__subpackages__")]
        # Root package ("") or top-level directory (e.g. //src:__subpackages__).
        return "/" not in pkg
    return False


def _has_broad_visibility(value_node: ast.expr) -> bool:
    """Returns True if `value_node` contains any broad visibility label."""
    for node in ast.walk(value_node):
        if (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and _is_broad_visibility_label(node.value)
        ):
            return True
    return False


@register_check
class PackageDefaultVisibilityCheck(BazelCheck):
    """Flags `default_visibility` in `package()` (error on broad, warning on narrow)."""

    name = "bazel_package_default_visibility"
    file_kinds = frozenset({FileKind.BUILD})

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if (
            call.func_name != "package"
            or "default_visibility" not in call.keywords
        ):
            return
        kw = call.keywords["default_visibility"]
        if has_disallowed_vendor_target_visibility(kw.value):
            fc.report(
                _VENDOR_ERROR_MSG,
                kw,
                level="error",
                url=VENDOR_VISIBILITY_REFERENCE_URL,
            )
        elif _has_broad_visibility(kw.value):
            fc.report(
                _BROAD_VISIBILITY_ERROR_MSG,
                kw,
                level="error",
                url=_REFERENCE_URL,
            )
        else:
            # Reaching here means visibility is not broad and does not reference
            # disallowed vendor subpaths (e.g., narrow subpackages or allowed vendor
            # root labels). Emit a warning recommending target-level visibility.
            fc.report(
                _NARROW_VISIBILITY_WARNING_MSG,
                kw,
                level="warning",
                url=_REFERENCE_URL,
            )
