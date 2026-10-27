# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs all Starlark tests for bazel_lint checks."""

load("./bzl_require_load_visibility_test.star", "run_bzl_require_load_visibility_tests")
load("./host_constraints_test.star", "run_host_constraints_tests")
load("./load_visibility_test.star", "run_load_visibility_tests")
load("./package_default_visibility_test.star", "run_package_default_visibility_tests")
load("./root_package_labels_test.star", "run_root_package_labels_tests")
load("./target_visibility_test.star", "run_target_visibility_tests")
load("./vendor_paths_test.star", "run_vendor_paths_tests")
load("./visibility_for_public_by_default_types_test.star", "run_visibility_for_public_by_default_types_tests")

def test_bazel_lint_checks():
    """Executes all registered bazel_lint check test suites."""

    # LINT.IfChange(run_all_check_tests)
    run_bzl_require_load_visibility_tests()
    run_host_constraints_tests()
    run_load_visibility_tests()
    run_package_default_visibility_tests()
    run_root_package_labels_tests()
    run_target_visibility_tests()
    run_vendor_paths_tests()
    run_visibility_for_public_by_default_types_tests()
    # LINT.ThenChange(//scripts/shac/bazel_lint/checks/__init__.py:all_checks)
