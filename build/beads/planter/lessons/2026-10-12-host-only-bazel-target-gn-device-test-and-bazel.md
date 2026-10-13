# Host only bazel target gn device test and bazel input decommissioning

- **Learned from:** inner loop friction on CL I204d0e1985edb489eb73e1e34bfe5f49cc6924b0, patchset 4 (task `dmlc`)
- **Date:** 2026-10-12
- **Changed:** `checks/migration_sanity.sh`

## Root cause

migration_sanity treated host-only Bazel Rust targets (restricted with
target_compatible_with = HOST_OS_CONSTRAINTS) as buildable for Fuchsia device
test packages, failing to populate gn_kept_pkg_test_deps and falsely triggering
gn_test_package_not_migrated and test_generated_by_bazel2gn.

## Why not a one-off fix

Any dual-build library migrated as host-only in Bazel because its Fuchsia-only
dependencies remain in GN must keep its GN fuchsia_unittest_package and
bazel2gn-emitted with_unit_tests without false-positive test migration errors
across all future tasks.
