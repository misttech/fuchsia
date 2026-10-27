# Preexisting bazel exports and target preservation

- **Learned from:** cq failure on CL I1862615828a5aa2bfe0bc8807e5ab7d517f5ca49, patchset 1 (task `src-recovery-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When migrating a package (or a transitive dependency) whose directory already
had a partial BUILD.bazel file at $PLANTER_CHANGE_BASE (for example exporting
resource or template files via exports_files(...) to
//bundles/assembly:__subpackages__), the coding agent overwrote BUILD.bazel from
BUILD.gn and dropped the pre-existing exports_files(...) entries because they
have no counterpart in BUILD.gn. Existing static checks compared BUILD.bazel
against BUILD.gn rather than against the pre-existing BUILD.bazel at
$PLANTER_CHANGE_BASE, and target-scoped fx bazel build //<dir>:all did not
evaluate external callers such as //bundles/assembly:platform.

## Why not a one-off fix

Restoring the dropped exports_files(...) entries only in the single failing
package would not prevent future migrations from overwriting pre-existing
exports_files(...) declarations, named targets, or their visibility scopes
whenever a partially populated BUILD.bazel file is expanded with newly migrated
targets.
