# Co migrated and subrepo dependency reachability and rebased bmt baseline

- **Learned from:** inner loop friction on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 3 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-27
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`

## Root cause

First, check_unlanded_bazel_dependencies in checks/cq_reachability.sh checked
only git cat-file -e against origin/main in the root repository without checking
whether the dependency BUILD.bazel was created in HEAD (co-migrated in the same
CL) or resides inside a nested jiri git subrepository whose files are not
tracked in the root repository tree. Second, advance_rebased_base in
checks/commit_message_format.sh did not advance PLANTER_CHANGE_BASE to HEAD~1
when the initial base commit was no longer an ancestor of HEAD after rebasing or
amending during review rounds, and base_gn_names did not include
bazel2gn-generated post-sentinel targets.

## Why not a one-off fix

Any migration that co-migrates a dependency package in the same CL, depends on a
BUILD.bazel target inside a nested jiri subrepository under third_party/, or
rebases and amends during Gerrit review while packaging a migrated binary with
fx_packaged_binary will hit the same false-positive
unlanded_bazel_dependency_package and bazel_migration_target_not_migrated errors
unless both checks handle co-migrated/subrepository packages and rebased base
revisions generically.
