# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that platform Bazel files do not access Fuchsia Bazel SDK paths."""

from framework import (
    BazelCheck,
    FileContext,
    FileKind,
    StringInfo,
    normalize_repo_prefix,
    register_check,
)

_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-fuchsia-bazel-sdk-paths"

_DISALLOWED_REPO_PREFIXES = (
    "@fuchsia_sdk//",
    "@internal_sdk//",
)

_RULES_FUCHSIA_SDK_PREFIX = "@rules_fuchsia//fuchsia"


def _matches_prefix_boundary(value: str, prefix: str) -> bool:
    return value == prefix or value.startswith((f"{prefix}/", f"{prefix}:"))


def _is_disallowed_sdk_path(value: str) -> bool:
    normalized = normalize_repo_prefix(value)
    if normalized.startswith(
        _DISALLOWED_REPO_PREFIXES
    ) or _matches_prefix_boundary(normalized, _RULES_FUCHSIA_SDK_PREFIX):
        return True
    if (
        normalized.startswith("//")
        or (normalized.startswith("@") and "//" in normalized)
    ) and "bazel_sdk" in normalized:
        return True
    return False


def _error_message(label: str) -> str:
    return (
        f"Do not use Fuchsia Bazel SDK path `{label}` in platform code; "
        "use platform rules under `//build/bazel/rules/` or in-tree platform targets instead."
    )


@register_check
class RepositoryPathsCheck(BazelCheck):
    """Disallows Fuchsia Bazel SDK repository and `bazel_sdk` label paths in platform code."""

    name = "bazel_repository_paths"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})
    policy_exempt_globs = (
        # Files under //build/bazel_sdk/ are part of the Fuchsia Bazel SDK
        # itself and its tests.
        "build/bazel_sdk/**",
        # Board and partition definitions load board/partition assembly rules
        # (`fuchsia_board_configuration`, `fuchsia_board_input_bundle`,
        # `fuchsia_board_input_bundle_set`, `fuchsia_prebuilt_board_input_bundle`,
        # `fuchsia_prebuilt_board_input_bundle_set`,
        # `fuchsia_partitions_configuration`, `fuchsia_partition`,
        # `fuchsia_bootloader_partition`, `PARTITION_TYPE`, and `SLOT`) from
        # `@rules_fuchsia//fuchsia:assembly.bzl`.
        "boards/**",
        # TODO(https://fxbug.dev/521387104): Review these driver directories for more precise patterns.
        "examples/drivers/**",
        "sdk/lib/driver/**",
        "src/camera/drivers/**",
        "src/connectivity/openthread/drivers/**",
        "src/connectivity/wlan/drivers/**",
        "src/devices/**",
        "src/graphics/display/drivers/**",
        "src/graphics/drivers/**",
        "src/lib/ddk/**",
        "src/media/audio/drivers/**",
        "src/media/drivers/**",
        "src/ui/backlight/drivers/**",
        "src/ui/input/drivers/**",
        "src/ui/light/drivers/**",
    )
    supports_allowlist = True
    supports_targets_allowlist = True

    def check_string(self, fc: FileContext, s: StringInfo) -> None:
        if (
            s.is_docstring
            or s.in_compare
            or s.in_fail
            or s.keyword_arg == "doc"
        ):
            return
        if not _is_disallowed_sdk_path(s.value):
            return
        fc.report_string(
            _error_message(s.value),
            s.node,
            s.value,
            level="error",
            url=_REFERENCE_URL,
        )
