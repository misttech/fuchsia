# Cq reachability uncommitted change base and depends on parsing

- **Learned from:** inner loop friction on a local task (task `src-sys-lib-batch`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When running in an uncommitted working tree or parallel batch migration where
PLANTER_CHANGE_BASE resolves to HEAD, or when HEAD is on origin/main,
cq_reachability must not treat the base commit at HEAD as the migration task's
commit when checking for stale landed ancestor commits or Depends on: footers,
and Depends on: extraction must only match explicit Depends on: lines and
continuation entries rather than Gerrit Reviewed-on: footers.

## Why not a one-off fix

Any parallel batch migration or uncommitted working-tree run starts with
PLANTER_CHANGE_BASE at HEAD (often an upstream origin/main commit carrying a
Gerrit Reviewed-on: footer or an ancestor base commit), so gating
landed-ancestor and Depends on: checks on HEAD being the task's own commit and
scoping extraction to Depends on: blocks is required across all packages.
