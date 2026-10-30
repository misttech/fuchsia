# Preexisting skipped bazel helper preservation in partial dual build

- **Learned from:** inner loop friction on a local task (task `pkg-tests-pkg-resolver`)
- **Date:** 2026-10-29
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

In checks/migration_sanity.sh, check_unused_skipped_gn_duplicates flagged every
package-private # @bazel2gn:skip packaging or component helper target (such as
resource, fuchsia_package_resource_group, or a GN-blocked component helper) that
duplicated a pre-sentinel BUILD.gn target and had no in-file Bazel consumer,
without checking whether that target already existed in BUILD.bazel at
$PLANTER_CHANGE_BASE. When a directory already contained pre-staged skipped
helper targets in BUILD.bazel while its enclosing fuchsia_package or
fuchsia_test_package had to remain above ## BAZEL2GN SENTINEL due to unmigrated
GN-only component dependencies, keeping the target triggered
unused_skipped_duplicate_target while deleting it triggered
dropped_preexisting_bazel_target. (The companion gn_attr_parity find_planter_bin
lookup and GN forwarding-group dep_alignment INFO coverage were already resolved
in checks/gn_attr_parity.sh.)

## Why not a one-off fix

Any incremental dual-build migration of a package directory whose pre-existing
BUILD.bazel already defines package-private # @bazel2gn:skip resource or blocked
component helper targets for a future package migration—while the enclosing GN
package or test package remains above ## BAZEL2GN SENTINEL—hits the exact same
catch-22 unless check_unused_skipped_gn_duplicates exempts non-convertible and
GN-blocked helper targets present at $PLANTER_CHANGE_BASE while still converting
unblocked convertible manifests/components and rejecting newly added unused
skipped duplicates.
