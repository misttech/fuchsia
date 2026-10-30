# Accidental submodule gitlink replacement with worktree symlink and [bazel2gn]/[bazel] commit change-base resolution in build only scope and run checks

- **Learned from:** cq failure on [CL 1932207](https://fuchsia-review.googlesource.com/c/fuchsia/+/1932207), patchset 2 (task `b2gn-compiled-action`)
- **Date:** 2026-10-30
- **Changed:** `checks/build_only_scope.sh`, `tools/run_checks.sh`

## Root cause

The CQ builders failed in checkout.jiri update with 'could not create work tree
dir ...: File exists' because the committed change replaced a third-party
submodule gitlink (mode 160000) with a local worktree symlink (mode 120000)
during rebase/commit. When run_checks.sh and build_only_scope.sh ran without an
explicit PLANTER_CHANGE_BASE=HEAD~1 override on a commit whose subject started
with [bazel2gn] or [bazel] rather than [bazel_migration], their
is_task_migration_head logic failed to recognize HEAD as the task commit and
kept change_base at HEAD instead of advancing to HEAD~1, causing
build_only_scope to diff HEAD against HEAD and miss the committed non-build
submodule symlink diff.

## Why not a one-off fix

Reverting the single third-party submodule symlink in the current commit would
not prevent future [bazel2gn] or [bazel] tooling/migration commits from
bypassing HEAD~1 change-base detection in run_checks.sh and build_only_scope.sh,
nor would it instruct agents on how to revert staged worktree symlinks that
replace submodule gitlinks and break CQ jiri update across all third-party
subrepositories.
