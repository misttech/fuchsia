# Commit message checkout relative path validation

- **Learned from:** human review on CL I156046cb33887480f29f9874d10fee37f57d49ef, patchset 1 (task `sockaddr`)
- **Date:** 2026-10-16
- **Changed:** `checks/commit_message_format.sh`

## Root cause

When running inside a jiri worktree (.jiri_root/worktrees/<name>), paths
prefixed with the worktree subpath (either written in the commit message or
passed in CoderReport.modified_files and rendered into the 'Also migrated in
this change:' block) produced broken '//.jiri_root/worktrees/<name>/<dir>'
labels in the commit message body because commit_message_format.sh lacked
validation that commit message directory labels and 'Also migrated in this
change:' bullets are relative to the Fuchsia checkout root.

## Why not a one-off fix

Fixing only the single commit message leaves all future migrations executed
inside jiri worktrees vulnerable to emitting broken
'//.jiri_root/worktrees/<name>/...' or host-prefixed paths in commit messages
and 'Also migrated in this change:' blocks; enforcing checkout-relative paths
deterministically in commit_message_format.sh catches this across every package
migration.
