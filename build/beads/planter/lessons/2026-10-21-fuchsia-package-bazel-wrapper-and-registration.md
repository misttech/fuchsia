# Fuchsia package bazel wrapper and registration parity

- **Learned from:** inner loop friction on CL I6645d3a562cd2ab0c17cacd9850c451ae7b768a3, patchset 15 (task `stressor`)
- **Date:** 2026-10-21
- **Changed:** `checks/build_only_scope.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

When a GN fuchsia_package or fuchsia_package_with_single_component is migrated
to fx_package in BUILD.bazel and retains a thin bazel_fuchsia_package wrapper in
BUILD.gn so package labels remain usable in fx set --with-base,
gn_target_type_parity flagged the fuchsia_package-to-bazel_fuchsia_package
transition as gn_target_type_changed, build_only_scope rejected updating the
package README.md target label reference, and is_registration_only_bazel_pkg
only recognized test_suite/exports_files (missing stamp_group in
//build/bazel/rules/tests) so review_diff emitted false-positive errors on
registration-only build-test aggregators.

## Why not a one-off fix

Any Fuchsia package migrated to fx_package in BUILD.bazel that is referenced by
developer workflows (fx set --with-base) or build-only package test aggregators
(stamp_group in //build/bazel/rules/tests) will retain a bazel_fuchsia_package
wrapper in BUILD.gn and update its package label references; handling these AST
patterns generally in gn_target_type_parity, build_only_scope, and review_diff
deduplication prevents false positives across all future package migrations.
