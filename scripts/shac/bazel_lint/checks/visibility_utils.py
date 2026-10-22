# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared visibility constants and helper functions for Bazel lint checks."""

from __future__ import annotations

import ast

from framework import CallInfo

# Common reference URLs for visibility documentation in the Fuchsia Bazel style guide.
PUBLIC_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-public-visibility"
LOAD_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/bzl_files#load-visibility"
VENDOR_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-reference-vendor-subpaths"

# Prefix strings used to match vendor references in visibility declarations
# for both target visibility (BUILD.bazel) and load visibility (.bzl).
#
# We check for two specific prefixes:
#
# 1. "//vendor":
#    - Matches standard repository-relative labels targeting vendor code (e.g.
#      "//vendor:__pkg__", "//vendor/google/...").
#    - By matching "//vendor" without requiring an immediate ":" or "/", this
#      catches typos where a delimiter was omitted (e.g. "//vendorgoogle/..."
#      or "//vendorfoo") as well as bare labels like "//vendor" (which in
#      Bazel resolves to "//vendor:vendor"). Any string starting with "//vendor"
#      that is not one of the explicit allowed root labels will be caught.
#
# 2. "vendor":
#    - Catches any instances where the leading slashes are accidentally left off
#      (e.g. "vendor/google:__pkg__" or "vendor/foo/...").
VENDOR_VISIBILITY_PREFIXES_TO_CHECK: tuple[str, ...] = ("//vendor", "vendor")

# Allowed vendor visibility labels in public Fuchsia BUILD.bazel files. More
# specific private //vendor/... paths cannot be referenced in public code.
ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS: frozenset[str] = frozenset(
    {
        "//vendor:__pkg__",
        "//vendor:__subpackages__",
        "//vendor/google:__pkg__",
        "//vendor/google:__subpackages__",
    }
)

# Allowed vendor load visibility labels in public Fuchsia .bzl files. Unlike
# BUILD files, .bzl load visibility uses package specifications (e.g. "//vendor/...")
# rather than target labels (e.g. "//vendor:__subpackages__").
ALLOWED_VENDOR_LOAD_VISIBILITY_LABELS: frozenset[str] = frozenset(
    {
        "//vendor",
        "//vendor/...",
        "//vendor/google",
        "//vendor/google/...",
    }
)


def _is_vendor_visibility_label_to_check(label: str) -> bool:
    """Returns True if `label` references the vendor directory (including typos and relative paths)."""
    return label.startswith(VENDOR_VISIBILITY_PREFIXES_TO_CHECK)


def is_disallowed_vendor_load_visibility(label: str) -> bool:
    """Returns True if `label` is a disallowed vendor reference for .bzl load visibility."""
    return (
        _is_vendor_visibility_label_to_check(label)
        and label not in ALLOWED_VENDOR_LOAD_VISIBILITY_LABELS
    )


def is_disallowed_vendor_target_visibility(label: str) -> bool:
    """Returns True if `label` is a disallowed vendor reference for target visibility."""
    return (
        _is_vendor_visibility_label_to_check(label)
        and label not in ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS
    )


def has_disallowed_vendor_target_visibility(value_node: ast.expr) -> bool:
    """Returns True if `value_node` references a vendor path not in `ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS`."""
    for node in ast.walk(value_node):
        if (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and is_disallowed_vendor_target_visibility(node.value)
        ):
            return True
    return False


def is_load_visibility(call: CallInfo) -> bool:
    """Returns True if `call` is a top-level load `visibility(...)` declaration in a .bzl file."""
    return call.is_top_level and call.func_name == "visibility"
