# Partial Dual-Build Package Migration Pre-Sentinel Target & Test Retention Coverage

- **Learned from:** human review on CL I02888491aed06632ef5a1321864b95a79a22e1c7, patchset 2 (task `src_graphics_display_lib_driver-framework-migration-utils_metadata`)
- **Date:** 2026-10-30
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When a multi-target BUILD.gn package is partially migrated in Case 2 dual-build
mode (with ## BAZEL2GN SENTINEL) and retains sibling libraries, test binaries,
or fuchsia_unittest_package/fuchsia_test_package targets above the sentinel
because their external dependencies lack Bazel targets or use GN-only
constructs, migration_sanity skipped flagging them as errors but did not emit
authoritative INFO unmatched_target coverage for their current and base line
numbers, leaving raw review_diff unmatched_target findings that prompted
disputes or reviewer rejections.

## Why not a one-off fix

Suppressing unmatched_target findings for a single package directory would leave
every other multi-target BUILD.gn directory that partially migrates unblocked
targets while retaining sibling targets and tests with unmigrated external
GN-only dependencies exposed to the same raw review_diff unmatched_target
findings.
