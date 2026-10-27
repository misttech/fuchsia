# Parallel working tree commit message reachability guard

- **Learned from:** inner loop friction on a local task (task `src-lib-network-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`

## Root cause

cq_reachability inspected the HEAD commit message for stale_landed_parent_commit
even when the target package's migration was uncommitted in the working tree on
top of a shared base or already-submitted HEAD commit that did not touch the
target directory, instructing parallel coders who cannot run git commit --amend
to rewrite HEAD.

## Why not a one-off fix

Suppressing the finding for a single package directory would leave every future
parallel or uncommitted working-tree migration vulnerable to false-positive
stale_landed_parent_commit errors whenever HEAD is a shared base commit or an
upstream commit with Reviewed-on footers.
