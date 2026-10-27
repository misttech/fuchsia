# Dual build diffengine daemon binary skew deduplication

- **Learned from:** inner loop friction on CL Icafd4780c5159f1d1713dee3a2e5e4e9bd516e8e, patchset 1 (task `pkg-system-updater`)
- **Date:** 2026-10-19
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When the on-disk planter binary was rebuilt while a long-running planter fleet
daemon remained active in memory, migration_sanity.sh and run_checks.sh resolved
only shutil.which('planter') on disk (which emitted zero diffengine findings)
instead of also querying the running daemon executable via /proc/<pid>/exe, so
migration_sanity failed to emit the INFO deduplication findings needed to
suppress false-positive review_diff errors and warnings produced by the daemon's
in-process diffengine.

## Why not a one-off fix

Hardcoding target-specific suppressions would fail whenever any other dual-build
migration runs under an in-memory planter fleet daemon whose executable inode
differs from the updated on-disk binary; discovering all distinct running
/proc/<pid>/exe planter binaries alongside the on-disk binary in
migration_sanity.sh and run_checks.sh fixes the skew across all future
migrations.
