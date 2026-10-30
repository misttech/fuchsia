# Checks or check-runner advice that contradict each other: checks run in parallel on a shared GN/Bazel build directory, and test-component fix advice that drops GN-only test type semantics

- **Learned from:** inner loop friction on a local task (task `sys-test-manager-test-data`)
- **Date:** 2026-10-29
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

run_checks.sh ran every enabled check in a parallel thread pool, including
build_verification and tests_json_parity, which both run fx gen
(tests_json_parity also swaps args.gn and the tree) on the same out dir, so they
raced in the regenerator. Separately, the test_component_not_test_root
remediation always prescribed fx_component(testonly = True), ignoring that a
test_type-bearing component cannot be expressed that way and is only translated
by bazel2gn inside a test fx_package, which left the coder with no consistent
fix.

## Why not a one-off fix

Every future migration that runs the full run_checks.sh hits the out-dir race,
and any package with a fuchsia_test_component setting test_type (chromium,
system, etc.) used only by GN test packages hits the same contradiction. Fixing
only this directory would leave both problems in place for all later tasks.
