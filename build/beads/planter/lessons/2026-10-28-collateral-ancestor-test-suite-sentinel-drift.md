# Collateral ancestor test suite sentinel drift false positive

- **Learned from:** inner loop friction on a local task (task `b4-tee1`)
- **Date:** 2026-10-28
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When a migration creates a new ancestor area BUILD.bazel defining only a
test_suite() target and wires bazel_test_suite() into the ancestor BUILD.gn (as
required by cq_reachability when no ancestor suite exists yet), review_diff
emits a sentinel_drift warning on the ancestor BUILD.gn. Both
inspect_preexisting_collateral_pkg in checks/migration_sanity.sh and
is_registration_only_bazel_pkg in tools/run_checks.sh previously returned early
whenever BUILD.bazel did not already exist at change_base/HEAD~1, and
record_review_diff_compat_info only emitted INFO coverage for gn_unmodified
rather than sentinel_drift on packages without bazel2gn-convertible targets.

## Why not a one-off fix

Hardcoding an exemption for a single subsystem directory would leave every other
subsystem that gains its first migrated unit test and newly created ancestor
BUILD.bazel test_suite exposed to the same unresolvable sentinel_drift warning.
