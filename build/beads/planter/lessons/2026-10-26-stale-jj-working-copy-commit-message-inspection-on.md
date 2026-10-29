# Stale jj working copy commit message inspection on uncommitted change base

- **Learned from:** inner loop friction on CL I10a869eef9a52f979795511e5e0e5708fd7429c2, patchset 1 (task `network-device-log`)
- **Date:** 2026-10-26
- **Changed:** `checks/commit_message_format.sh`

## Root cause

When git HEAD equals PLANTER_CHANGE_BASE with uncommitted working-tree changes
in a checkout containing a .jj directory, commit_message_format.sh
unconditionally fell back to resolving jj '@' via 'jj --ignore-working-copy',
which returned the stale previous uploaded commit (PLANTER_UPLOADED_COMMIT) from
before git reset HEAD to PLANTER_CHANGE_BASE and linted that outdated commit
message before planter created the new commit from CoderReport.

## Why not a one-off fix

Every migration task running in a colocated jj worktree where git HEAD is reset
to PLANTER_CHANGE_BASE during iterative or review rounds will hit false-positive
commit_message_format warnings against the stale jj '@' commit unless
commit_message_format.sh guards the jj '@' fallback to only apply when
PLANTER_CHANGE_BASE is '@-', '@' is not PLANTER_UPLOADED_COMMIT, and the working
tree has no uncommitted diff against '@'.
