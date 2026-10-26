# Stale landed parent commit rebase

- **Learned from:** cq failure on CL I35d897ef924a3b16c57d1eac675f6850d03fd072, patchset 1 (task `swd-pkg-multi-universal-tool`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

When a migration commit is stacked on a local cherry-picked prerequisite change
(such as a shared Bazel rule fix for deduplicating _solib_ runfile symlinks) and
that prerequisite subsequently lands on origin/main, git merge-tree --write-tree
against origin/main exits 0 because both branches apply identical changes to the
prerequisite files. Because cq_reachability.sh only checked git merge-tree for
textual conflicts, it failed to flag that HEAD remained parented on a local
unsubmitted copy of an already-landed Change-Id and still carried a stale
'Depends on:' reference instead of being rebased directly onto origin/main.

## Why not a one-off fix

Rebasing or editing only a single package migration branch does not prevent
future stacked migrations from remaining parented on local cherry-picks of
prerequisite CLs after those prerequisites land on origin/main; checking
merge_base..HEAD~1 against origin/main via git cherry and Change-Id footers
(plus landed 'Depends on:' CL references) in cq_reachability.sh catches this
across all future migrations.
