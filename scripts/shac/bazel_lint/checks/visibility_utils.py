# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared visibility constants and helper functions for Bazel lint checks."""

from framework import CallInfo

# Common reference URLs for visibility documentation in the Fuchsia Bazel style guide.
PUBLIC_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-public-visibility"
LOAD_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/bzl_files#load-visibility"
VENDOR_VISIBILITY_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-reference-vendor-subpaths"

# Vendor packages that may be referenced in visibility declarations in public
# Fuchsia (`fuchsia.git`) Bazel files.
#
# Note: These values are in normalized form (without leading `@` or `@@`) and
# must only be used after normalizing strings with `normalize_repo_prefix()`.
_VENDOR_PACKAGES_ALLOWED_IN_FUCHSIA_GIT: tuple[str, ...] = (
    # Avoid adding to this list. Only generic immediate subdirectories of
    # `//vendor/google/` should ever be added.
    # keep-sorted start
    "//vendor",
    "//vendor/google",
    "//vendor/google/boards",
    "//vendor/google/bundles",
    "//vendor/google/products",
    "//vendor/google/sdk",
    "//vendor/google/storage",
    "//vendor/google/tools",
    # keep-sorted end
)

# Allowed vendor visibility labels in public Fuchsia Bazel files.
# More specific private //vendor/... paths cannot be referenced in public code.
# Note: These values are in normalized form (without leading `@` or `@@`) and
# must only be used after normalizing strings with `normalize_repo_prefix()`.
ALLOWED_VENDOR_TARGET_VISIBILITY_LABELS: frozenset[str] = frozenset(
    f"{pkg}:{suffix}"
    for pkg in _VENDOR_PACKAGES_ALLOWED_IN_FUCHSIA_GIT
    for suffix in ("__pkg__", "__subpackages__")
)

# Allowed vendor package-specification labels in public Fuchsia .bzl load
# visibility() and BUILD.bazel package_group(packages = [...]) declarations.
# Note: These values are in normalized form (without leading `@` or `@@`) and
# must only be used after normalizing strings with `normalize_repo_prefix()`.
ALLOWED_VENDOR_LOAD_VISIBILITY_LABELS: frozenset[str] = frozenset(
    f"{pkg}{suffix}"
    for pkg in _VENDOR_PACKAGES_ALLOWED_IN_FUCHSIA_GIT
    for suffix in ("", "/...")
)


def is_load_visibility(call: CallInfo) -> bool:
    """Returns True if `call` is a top-level load `visibility(...)` declaration in a .bzl file."""
    return call.is_top_level and call.func_name == "visibility"
