# Dual build host test data and prefix rdep attribution

- **Learned from:** inner loop friction on a local task (task `tools-gn-tier0-2`)
- **Date:** 2026-10-21
- **Changed:** `checks/migration_sanity.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

Two static check gaps caused false-positive findings during dual-build host
binary and test migrations: (1) migration_sanity flagged hand-written GN
go_test/rustc_test targets above the BAZEL2GN SENTINEL as
duplicate_test_above_sentinel even when the corresponding Bazel test target had
a valid @bazel2gn:skip condition (such as runtime test_data/data attributes that
bazel2gn cannot translate, requiring the GN copy to remain hand-written above
the sentinel while the library has GN consumers); and (2) visibility_audit
merged per_target_rdeps across any explicit target pair sharing a '<name>_'
prefix and lacked host binary / host_test_data rules in
BAZEL_NON_GN_SYNCED_RULES, falsely requiring visibility on private binary
targets whenever a sibling '<name>_lib' library had external callers.

## Why not a one-off fix

Fixing or allowlisting only the individual packages in this migration would
leave migration_sanity, visibility_audit, and find_rdeps broken for every
future package that pairs a private host binary with a shared '<name>_lib'
library or migrates a dual-build Go/Rust host test with runtime test_data files.
