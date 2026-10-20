# Commit message redundant body and jj working copy resolution

- **Learned from:** human review on CL I7f7bf431b91b6940e77233e1157a31cf630a7edd, patchset 1 (task `dns-server-watcher`)
- **Date:** 2026-10-16
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

In a jj colocated checkout where the working-copy commit @ holds the described
migration change while git HEAD remains at @- (equal to PLANTER_CHANGE_BASE),
commit_message_format.sh resolved HEAD equal to base_rev (and git rev-parse
treated @ as an alias for HEAD before consulting jj), causing the entire commit
message check to be skipped. In addition, commit_body_redundant_title inspected
individual lines rather than joined body paragraphs, missing multi-line wrapped
restatements of the subject line (or bare full-path restatements when the
subject abbreviates the path with //...).

## Why not a one-off fix

Deleting the redundant body lines only in the current CL would leave
commit_message_format.sh unable to inspect @ in jj colocated worktrees and
unable to catch line-wrapped or path-only restatements of the [bazel_migration]
//<dir> subject line across future migrations.
