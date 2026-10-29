# Test component template parity and fx test footer reachability

- **Learned from:** inner loop friction on a local task (task `b4-fuzz1`)
- **Date:** 2026-10-28
- **Changed:** `checks/build_verification.sh`, `checks/gn_target_type_parity.sh`, `tools/run_checks.sh`

## Root cause

First, when a pre-migration BUILD.gn declared a test root component using
`fuchsia_component` (with `testonly = true` or listed in
`fuchsia_test_package.test_components`) rather than
`fuchsia_unittest_component`, migrating it to `fx_test_component` (required by
`fx_package(test_components = [...])` in Bazel) causes `bazel2gn` to generate
`fuchsia_test_component` below the sentinel. `checks/gn_target_type_parity.sh`
only recognized `fuchsia_unittest_component` -> `fuchsia_test_component` on
identically named targets and did not emit deduplicating `INFO`
`template_parity` findings for `fuchsia_component` test roots or renamed test
component/package targets, while `tools/run_checks.sh` had a broad local
downgrade on `template_parity` that hid `review_diff` errors during
`run_checks.sh` that still failed in Planter's post-report runner. Second,
`checks/build_verification.sh` instructed the coder that
`Test: fx test //<dir>:<test>` is always accepted even when `//<dir>:<test>` is
absent from `<build dir>/tests.json` (which happens whenever a Bazel `fx_test`
is wired into an ancestor `test_suite` not included in `args.gn` while `args.gn`
only includes leaf `:tests` groups), causing Planter's `test_verifier` (`fx test
--dry`) to fail with `missing_fx_test_target`.

## Why not a one-off fix

Many Fuchsia packages declare custom-manifest unit or integration test root
components with `fuchsia_component(testonly = true)` inside
`fuchsia_test_package(test_components = [...])` and wire migrated `fx_test`
targets into ancestor `test_suite` aggregators outside the leaf `:tests` group
in `args.gn`. Updating `gn_target_type_parity.sh`, `run_checks.sh`, and
`build_verification.sh` handles this pattern across all packages.
