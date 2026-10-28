# Bazel test suite wiring via standardized test groups and root test groups

- **Learned from:** inner loop friction on a local task (task `src-lib-assembly-component-manager-config`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

The `suite_is_wired` and ancestor test suite discovery logic in
`checks/cq_reachability.sh` only matched `group("<name>")` blocks rooted at
`group("tests")` or directly referenced from the immediate parent `BUILD.gn`. It
did not recognize `standardized_test_groups("tests")` (from
`//build/testing/standardized_test_groups.gni`, which expands `host`,
`hermetic`, `non_hermetic`, and `e2e` lists into `group("host_tests")`,
`group("hermetic_tests")`, `group("non_hermetic_tests")`, and
`group("e2e_tests")`) or standard subsystem root test groups (`host_tests`,
`target_tests`, `hermetic_tests`, `non_hermetic_tests`, `e2e_tests`,
`tests_no_e2e`) wired directly from top-level test bundles such as
`//bundles/buildbot/host:no_e2e`, causing a false-positive
`unwired_bazel_test_suite` finding.

## Why not a one-off fix

Special-casing a single directory would leave `cq_reachability.sh` blind to any
other subsystem or package that wires `bazel_test_suite` through
`standardized_test_groups(...)` or standard category-specific root test groups
(`host_tests`, `hermetic_tests`, `non_hermetic_tests`, `e2e_tests`,
`tests_no_e2e`). Updating `suite_is_wired` and root test group regexes in
`checks/cq_reachability.sh` handles all such GN test group templates and
standard root group names tree-wide.
