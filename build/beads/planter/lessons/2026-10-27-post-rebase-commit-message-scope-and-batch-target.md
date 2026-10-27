# Post rebase commit message scope and batch target parity

- **Learned from:** human review on CL Iaa2e4e4a06b03bca4b85c475637090a69a5c8251, patchset 2 (task `src-connectivity-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

When a multi-directory batch task rebases onto a newer origin/main commit where
a subset of its batch target directories have already landed upstream,
commit_message_format.sh compared HEAD against a stale pre-rebase
PLANTER_CHANGE_BASE commit SHA rather than advancing base_rev to the post-rebase
merge-base(HEAD, origin/main), and its Bazel-Migration-Target fallback branch
treated unmigrated batch directories in PLANTER_TARGET_DIRS as fallback targets
instead of adding their pre-existing BUILD.bazel targets to excluded_bmt.

## Why not a one-off fix

Updating only the single task commit message does not prevent future batch
migrations that rebase onto origin/main after overlapping upstream migrations
land from retaining stale subject directories, bullet lists, or
Bazel-Migration-Target footers when evaluated against a pre-rebase base SHA.
