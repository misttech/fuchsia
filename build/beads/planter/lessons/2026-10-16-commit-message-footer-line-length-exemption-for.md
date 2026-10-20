# Commit message footer line length exemption for unwrapped Bazel-Migration-Target labels

- **Learned from:** inner loop friction on CL I7f7bf431b91b6940e77233e1157a31cf630a7edd, patchset 1 (task `dns-server-watcher`)
- **Date:** 2026-10-16
- **Changed:** `checks/commit_message_format.sh`

## Root cause

checks/commit_message_format.sh enforced a 72-character line length cap
(bazel_migration_target_line_too_long) on Bazel-Migration-Target: footers,
contradicting both the Fuchsia Bazel migration commit message guidelines (which
explicitly exempt unwrapped Bazel-Migration-Target: and Test: footers from the
72-character body line limit) and missing_bazel_migration_target_footer whenever
Bazel-Migration-Target: //<dir> itself exceeds 72 characters (any directory path
> 46 characters).

## Why not a one-off fix

Exempting only one directory path would leave every migration of a package whose
path exceeds 46 characters (or whose //<dir>:<target> label exceeds 48
characters) blocked by contradictory missing_bazel_migration_target_footer and
bazel_migration_target_line_too_long findings.
