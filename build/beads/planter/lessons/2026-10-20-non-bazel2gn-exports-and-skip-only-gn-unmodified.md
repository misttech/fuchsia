# Non bazel2gn exports and skip only gn unmodified compat

- **Learned from:** inner loop friction on a local task (task `pkg-tests-pkg-resolver-resources`)
- **Date:** 2026-10-20
- **Changed:** `checks/migration_sanity.sh`

## Root cause

When a migration adds or updates a BUILD.bazel that only exports files or
defines # @bazel2gn:skip targets alongside an existing BUILD.gn (or when HEAD is
already a [bazel_migration] commit under jj @- diffing), review_diff emits
gn_unmodified ERROR unless migration_sanity unconditionally records the matching
INFO compatibility entry across all candidate directories in the change.

## Why not a one-off fix

Any multi-package migration that exports shard or certificate files or migrates
non-bazel2gn resource/manifest targets in helper packages will encounter the
same gn_unmodified false positive if migration_sanity skips candidate
directories committed in HEAD or gated on changed-file membership.
