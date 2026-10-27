# Upstream origin main fetch and preexisting dependency skip preservation

- **Learned from:** cq failure on CL I55a3e9fa7750e063cd8690f31427eeb603bb10fc, patchset 1 (task `src-sys-run-test-suite-directory-and-fixtures`)
- **Date:** 2026-10-26
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`

## Root cause

In checks/cq_reachability.sh, maybe_refresh_origin_main() returned early
whenever a local origin/main ref existed unless PLANTER_FORCE_FETCH=1 was set,
preventing origin/main from ever refreshing and causing git merge-tree to
compare against a stale local ref. Additionally, checks/migration_sanity.sh did
not exclude pre-existing base_target_skips and base_attr_skips when checking
bare # @bazel2gn:skip annotations on test targets and attributes, forcing
drive-by edits in already-migrated dependency packages touched only for
visibility.

## Why not a one-off fix

Rebasing or reverting only a single conflicted package in the current task
leaves maybe_refresh_origin_main() short-circuited and leaves pre-existing #
@bazel2gn:skip annotations in touched dependency packages flagged as
unjustified_test_skip across all future migration tasks.
