# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks target visibility in BUILD.bazel and .bzl files."""

from framework import (
    BazelCheck,
    FileContext,
    FileKind,
    StringInfo,
    register_check,
)

from .visibility_utils import (
    PUBLIC_VISIBILITY_REFERENCE_URL,
    VENDOR_VISIBILITY_REFERENCE_URL,
    is_disallowed_vendor_target_visibility,
)

_ERROR_MSG = (
    'Do not use `"//visibility:public"`. '
    "Scope visibility to specific packages or subtrees "
    '(e.g., `"//src:__subpackages__"`), or `"//:__subpackages__"` '
    "if truly needed across the codebase."
)

_VENDOR_ERROR_MSG = (
    "Do not include vendor subpaths in target visibility. "
    'Only `"//vendor:__pkg__"`, `"//vendor:__subpackages__"`, '
    '`"//vendor/google:__pkg__"`, and `"//vendor/google:__subpackages__"` '
    "are permitted in public Fuchsia code."
)


@register_check
class TargetVisibilityCheck(BazelCheck):
    """Disallows `"//visibility:public"` on targets outside exempted directories."""

    name = "bazel_target_visibility"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})
    policy_exempt_globs = (
        # Files in the Bazel SDK's rules directories are consumed by external
        # repositories and SDK users.
        "build/bazel_sdk/bazel_rules_fuchsia/**",
        "build/bazel_sdk/fuchsia_rules_common/**",
        # Some build_flags() targets under //build/config/ are referenced from
        # @fuchsia_build_info//default_build_flags/BUILD.bazel, which is in a
        # different repository.
        "build/config/**",
    )
    supports_targets_allowlist = True

    def check_string(self, fc: FileContext, s: StringInfo) -> None:
        if s.in_compare or s.in_fail:
            return
        # package(default_visibility = ...) in BUILD.bazel files is checked
        # by bazel_package_default_visibility.
        if (
            fc.kind == FileKind.BUILD
            and s.enclosing_statement_call is not None
            and s.enclosing_statement_call.func_name == "package"
            and s.keyword_arg == "default_visibility"
        ):
            return

        if s.value == "//visibility:public":
            fc.report_string(
                _ERROR_MSG,
                s.node,
                s.value,
                level="error",
                url=PUBLIC_VISIBILITY_REFERENCE_URL,
            )
        elif (
            s.keyword_arg == "visibility"
            and is_disallowed_vendor_target_visibility(s.value)
        ):
            fc.report_string(
                _VENDOR_ERROR_MSG,
                s.node,
                s.value,
                level="error",
                url=VENDOR_VISIBILITY_REFERENCE_URL,
            )
