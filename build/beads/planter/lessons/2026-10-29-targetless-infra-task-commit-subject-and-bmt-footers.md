# Align commit_message_format with cl_message.go on targetless build-infrastructure tasks

- **Date:** 2026-10-29
- **Changed:** `checks/commit_message_format.sh`

## Root cause

Planter supports tasks with empty target directories (`PLANTER_TARGET_DIR == ""`
and `PLANTER_TARGET_DIRS == ""`) for `bazel2gn` and Starlark rule infrastructure
changes, where `cl_message.go` preserves the task's `[<tag>] <summary>` subject.
`checks/commit_message_format.sh` fell back to `first = "<dir>"` when `dirs` was
empty, demanding literal `[bazel_migration] //<dir>` subjects and
`Bazel-Migration-Target: //<dir>:<target>` footers even when zero GN targets were
migrated.

## Why not a one-off fix

Every prerequisite `bazel2gn` or Bazel rule task without target directories hit
false `commit_subject_format` errors (and `missing_bazel_migration_target_footer`
warnings when migrating zero packages), while tasks that do migrate targets still
require `Bazel-Migration-Target:` footers.
