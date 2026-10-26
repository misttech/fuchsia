# Unmigrated pre-sentinel GN rustc test and go test targets whose dependencies all have Bazel targets

- **Learned from:** human review on [CL 1837927](https://fuchsia-review.googlesource.com/c/fuchsia/+/1837927), patchset 3 (task `src_developer_ffx_lib_doctor_utils`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh enforced migrating device test packages
(gn_test_package_not_migrated) and rejected duplicate pre-sentinel GN tests
(duplicate_test_above_sentinel), but whenever a standalone or host rustc_test or
go_test remained hand-written above the BAZEL2GN SENTINEL without a
corresponding BUILD.bazel test target, it unconditionally emitted an INFO
finding (gn_test_kept_above_sentinel) without checking whether the test had any
GN-only blockers or whether all of its dependencies already had Bazel targets in
the checkout or on origin/main.

## Why not a one-off fix

Migrating the unit test in only a single host library package would leave
migration_sanity.sh permissive for all other dual-build packages where a
standalone or host rustc_test or go_test is left above the BAZEL2GN SENTINEL
even after its dependencies have Bazel targets.
