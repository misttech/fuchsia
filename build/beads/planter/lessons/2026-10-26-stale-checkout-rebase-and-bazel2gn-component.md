# Stale checkout rebase and bazel2gn component conversion

- **Learned from:** human review on CL I6c67c92e89847a6089233b3a242d32d0da29000f, patchset 1 (task `netemul-test-support`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`

## Root cause

When a worktree branch was behind origin/main and had uncommitted edits,
cq_reachability.sh allocated its temporary git index inside git_dir (which fails
when the parent .git/worktrees directory is read-only) and only reported a
rebase requirement on textual git merge-tree conflicts, missing cases where
origin/main had newer build/tools/bazel2gn rule support (such as fx_component
and fx_component_manifest) that prevented parallel BUILD.gn definitions above
the sentinel.

## Why not a one-off fix

Fixing only a single package directory leaves any future migration running on a
worktree behind origin/main vulnerable to outdated local bazel2gn support and
read-only git_dir temporary index failures during merge-tree conflict checks.
