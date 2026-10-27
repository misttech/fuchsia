# Batch migration upstream snipe and rebase pruning

- **Learned from:** human review on CL Iaa2e4e4a06b03bca4b85c475637090a69a5c8251, patchset 1 (task `src-connectivity-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`

## Root cause

When an upstream commit on origin/main already migrated a subset of the target
directories in a multi-directory batch task before rebase,
commit_message_format.sh only read PLANTER_TARGET_DIR (ignoring
PLANTER_TARGET_DIRS), did not resolve leading //.../ subject path abbreviations,
and only checked whether Also migrated in this change: directories still had
unmigrated BUILD.gn targets rather than verifying that each subject directory
and Also migrated in this change: bullet actually migrates targets against
base_rev.

## Why not a one-off fix

Fixing only the single batch commit leaves future multi-directory batch
migrations vulnerable whenever an overlapping upstream CL lands on origin/main
and rebase leaves stale subject paths, stale Also migrated in this change:
bullets, or residual edits in already-migrated batch directories.
