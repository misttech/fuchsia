# Post rebase change base and bazel2gn upstream rule scoping

- **Learned from:** inner loop friction on a local task (task `netstack3-tcp`)
- **Date:** 2026-10-26
- **Changed:** `checks/build_only_scope.sh`, `checks/cq_reachability.sh`, `tools/run_checks.sh`

## Root cause

cq_reachability unconditionally flagged any upstream commit under
build/tools/bazel2gn as upstream_rebase_conflict even when the migrated package
did not use any newly added bazel2gn rule or skip annotation, and rebasing HEAD
onto a newer origin/main left PLANTER_CHANGE_BASE pinned to the pre-rebase
ancestor commit so git diff $PLANTER_CHANGE_BASE included all intermediate
upstream commits.

## Why not a one-off fix

Every migration worktree created before an upstream commit to
build/tools/bazel2gn or rebased onto origin/main hits the same false-positive
upstream_rebase_conflict and stale PLANTER_CHANGE_BASE diff explosion unless
cq_reachability scopes the bazel2gn check to newly added rules used by the
package and the check runner/scope checks advance the effective diff base to
merge-base(origin/main, HEAD).
