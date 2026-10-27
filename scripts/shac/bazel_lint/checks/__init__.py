# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Imports all Bazel lint check modules to register them with the framework."""

# Ensure shac check registration, Starlark tests, and test filegroups are
# updated when adding or removing checks.
# LINT.IfChange(all_checks)
from . import (
    load_visibility,
    package_default_visibility,
    root_package_labels,
    target_visibility,
    vendor_paths,
)

# LINT.ThenChange(
#     //scripts/shac/bazel.star:register_all_checks,
#     //scripts/shac/bazel_lint/checks/BUILD.bazel:all_check_files,
#     //scripts/shac/bazel_lint/checks/tests/BUILD.bazel:all_check_test_files,
#     //scripts/shac/bazel_lint/checks/tests/checks_test.star:run_all_check_tests,
# )
